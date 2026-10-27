//! Quote retrieval. Yahoo Finance's public chart endpoint (no API key needed) is the default; a
//! symbol written `blackbull:NAME` is read from BlackBull Markets' public price feed instead.
//!
//! Both endpoints are unofficial, so they can change or start rate-limiting without notice.
//! Everything that knows about their shape lives in this file, which makes swapping providers a
//! local change.

use serde::Deserialize;

use crate::winhttp::Http;

pub const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) StockRock/0.1";
const HOST: &str = "query1.finance.yahoo.com";
const BLACKBULL_HOST: &str = "blackbull.com";
const BLACKBULL_PREFIX: &str = "BLACKBULL:";

#[derive(Debug, Clone, PartialEq)]
pub struct Quote {
    pub price: f64,
    /// `None` when the source has no previous close; the ticker then shows the price alone.
    pub prev_close: Option<f64>,
}

impl Quote {
    pub fn change(&self) -> Option<f64> {
        self.prev_close.map(|prev| self.price - prev)
    }

    pub fn change_pct(&self) -> Option<f64> {
        let prev = self.prev_close.filter(|prev| *prev != 0.0)?;
        Some((self.price - prev) / prev * 100.0)
    }
}

pub fn fetch(http: &Http, symbol: &str) -> Result<Quote, String> {
    // Symbols are normalized to upper case by the config.
    if let Some(name) = symbol.strip_prefix(BLACKBULL_PREFIX) {
        return fetch_blackbull(http, name);
    }
    let path = format!(
        "/v8/finance/chart/{}?range=1d&interval=1d",
        percent_encode(symbol)
    );
    let response = http.get(HOST, &path)?;
    parse(response.status, &response.body)
}

#[derive(Deserialize)]
struct ChartResponse {
    chart: Chart,
}

#[derive(Deserialize)]
struct Chart {
    result: Option<Vec<ChartResult>>,
    error: Option<ApiError>,
}

#[derive(Deserialize)]
struct ApiError {
    code: Option<String>,
    description: Option<String>,
}

#[derive(Deserialize)]
struct ChartResult {
    meta: Meta,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Meta {
    regular_market_price: Option<f64>,
    chart_previous_close: Option<f64>,
    previous_close: Option<f64>,
}

fn parse(status: u32, body: &[u8]) -> Result<Quote, String> {
    let Ok(ChartResponse { chart }) = serde_json::from_slice(body) else {
        return Err(match status {
            200 => "unexpected response".into(),
            429 => "rate limited (HTTP 429)".into(),
            other => format!("HTTP {other}"),
        });
    };
    if let Some(error) = chart.error {
        return Err(error
            .description
            .or(error.code)
            .unwrap_or_else(|| format!("HTTP {status}")));
    }
    let meta = chart
        .result
        .and_then(|results| results.into_iter().next())
        .map(|result| result.meta)
        .ok_or("no data")?;
    let price = meta
        .regular_market_price
        .filter(|p| p.is_finite())
        .ok_or("no price")?;
    let prev_close = meta
        .chart_previous_close
        .or(meta.previous_close)
        .filter(|p| p.is_finite())
        .unwrap_or(price);
    Ok(Quote {
        price,
        prev_close: Some(prev_close),
    })
}

/// BlackBull's instrument pages poll the bid endpoint. It has no previous close, so the quote has
/// no change figure. Gold, silver and forex are absent from that feed ("no symbol or id in
/// database"); the same snapshot's bid is on the instrument metadata endpoint, so that is the
/// fallback.
fn fetch_blackbull(http: &Http, name: &str) -> Result<Quote, String> {
    let name = percent_encode(name);
    let response = http.get(
        BLACKBULL_HOST,
        &format!("/wp-json/bbm/get_bid/?action=bid&symbol={name}"),
    )?;
    if let Ok(quote) = parse_blackbull(response.status, &response.body) {
        return Ok(quote);
    }
    let response = http.get(
        BLACKBULL_HOST,
        &format!("/wp-json/bbm/get_symbol_meta?symbol={name}"),
    )?;
    parse_blackbull_meta(response.status, &response.body)
}

#[derive(Deserialize)]
struct BlackBullBid {
    result: Option<String>,
    message: Option<String>,
    sell: Option<String>,
}

fn parse_blackbull(status: u32, body: &[u8]) -> Result<Quote, String> {
    let Ok(bid) = serde_json::from_slice::<BlackBullBid>(body) else {
        return Err(match status {
            200 => "unexpected response".into(),
            other => format!("HTTP {other}"),
        });
    };
    if bid.result.as_deref() != Some("OK") {
        return Err(bid
            .message
            .filter(|message| !message.is_empty())
            .or(bid.result)
            .unwrap_or_else(|| "no data".into()));
    }
    // The page shows the sell (bid) price as the instrument's price.
    let price = bid
        .sell
        .and_then(|sell| sell.trim().parse::<f64>().ok())
        .filter(|p| p.is_finite() && *p > 0.0)
        .ok_or("no price")?;
    Ok(Quote {
        price,
        prev_close: None,
    })
}

/// `get_symbol_meta` returns one JSON string of HTML: `key = value` lines separated by `<br>`.
/// `price_bid` is the sell price. `price_close` is the stored session close, not a previous close
/// for the live bid, so it is not shown as a change.
fn parse_blackbull_meta(status: u32, body: &[u8]) -> Result<Quote, String> {
    let text = decode_meta(body).ok_or_else(|| match status {
        200 => "unexpected response".into(),
        other => format!("HTTP {other}"),
    })?;
    let Some(raw) = meta_field(&text, "price_bid") else {
        return Err(if meta_field(&text, "symbol").is_some() {
            "no price".into()
        } else {
            "Symbol not found".into()
        });
    };
    let price = raw
        .parse::<f64>()
        .ok()
        .filter(|p| p.is_finite() && *p > 0.0)
        .ok_or("no price")?;
    Ok(Quote {
        price,
        prev_close: None,
    })
}

fn decode_meta(body: &[u8]) -> Option<String> {
    if let Ok(text) = serde_json::from_slice::<String>(body) {
        return Some(text);
    }
    let text = String::from_utf8(body.to_vec()).ok()?;
    if text.contains("price_bid") || text.contains("<body>") {
        Some(text)
    } else {
        None
    }
}

fn meta_field<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    let marker = format!("{key} = ");
    let start = text.find(&marker)? + marker.len();
    let value = &text[start..];
    let end = value.find(['<', '\n', '\r']).unwrap_or(value.len());
    let value = value[..end].trim();
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

/// Percent-encodes everything outside the URL "unreserved" set (`^GSPC` -> `%5EGSPC`).
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_chart_response() {
        let body = br#"{"chart":{"result":[{"meta":{"currency":"USD","symbol":"AAPL",
            "regularMarketPrice":330.32,"chartPreviousClose":333.02,"priceHint":2}}],"error":null}}"#;
        let quote = parse(200, body).unwrap();
        assert_eq!(
            quote,
            Quote {
                price: 330.32,
                prev_close: Some(333.02)
            }
        );
        assert!((quote.change().unwrap() + 2.70).abs() < 1e-9);
        assert!((quote.change_pct().unwrap() + 0.8108).abs() < 1e-3);
    }

    #[test]
    fn falls_back_to_previous_close_then_to_price() {
        let body =
            br#"{"chart":{"result":[{"meta":{"regularMarketPrice":10.0,"previousClose":9.0}}]}}"#;
        assert_eq!(parse(200, body).unwrap().prev_close, Some(9.0));
        let body = br#"{"chart":{"result":[{"meta":{"regularMarketPrice":10.0}}]}}"#;
        assert_eq!(parse(200, body).unwrap().prev_close, Some(10.0));
    }

    #[test]
    fn reports_api_errors() {
        let body = br#"{"chart":{"result":null,"error":{"code":"Not Found","description":"No data found, symbol may be delisted"}}}"#;
        assert_eq!(
            parse(404, body).unwrap_err(),
            "No data found, symbol may be delisted"
        );
    }

    #[test]
    fn reports_non_json_bodies_by_status() {
        assert_eq!(
            parse(429, b"Too Many Requests").unwrap_err(),
            "rate limited (HTTP 429)"
        );
        assert_eq!(parse(503, b"").unwrap_err(), "HTTP 503");
        assert_eq!(parse(200, b"<html>").unwrap_err(), "unexpected response");
    }

    #[test]
    fn missing_price_is_an_error() {
        let body = br#"{"chart":{"result":[{"meta":{"symbol":"X"}}]}}"#;
        assert_eq!(parse(200, body).unwrap_err(), "no price");
    }

    #[test]
    fn zero_previous_close_does_not_divide_by_zero() {
        let quote = Quote {
            price: 5.0,
            prev_close: Some(0.0),
        };
        assert_eq!(quote.change_pct(), None);
    }

    #[test]
    fn parses_a_blackbull_bid() {
        let body = br#"{"symbol":"BRENT","digits":"3","sell":"103.397","buy":"103.431","spread":"0.034","result":"OK"}"#;
        assert_eq!(
            parse_blackbull(200, body).unwrap(),
            Quote {
                price: 103.397,
                prev_close: None
            }
        );
    }

    #[test]
    fn blackbull_errors_are_reported() {
        assert_eq!(
            parse_blackbull(200, br#"{"result":"Symbol not found"}"#).unwrap_err(),
            "Symbol not found"
        );
        assert_eq!(
            parse_blackbull(200, br#"{"result":"OK","sell":"0"}"#).unwrap_err(),
            "no price"
        );
        assert_eq!(parse_blackbull(503, b"").unwrap_err(), "HTTP 503");
        assert_eq!(
            parse_blackbull(200, b"<html>").unwrap_err(),
            "unexpected response"
        );
        assert_eq!(
            parse_blackbull(
                404,
                br#"{"result":"ERROR","message":"no symbol or id in database"}"#
            )
            .unwrap_err(),
            "no symbol or id in database"
        );
    }

    #[test]
    fn parses_a_blackbull_metadata_bid() {
        // The endpoint returns a JSON string of HTML, which is how gold is published.
        let body = br#""<html><body>symbol = XAUUSD<br>price_bid = 4025.33<br>price_ask = 4025.45<br>price_close = 4016.89<br></body></html>""#;
        assert_eq!(
            parse_blackbull_meta(200, body).unwrap(),
            Quote {
                price: 4025.33,
                prev_close: None
            }
        );
        let raw = br#"<html><body>symbol = EURUSD<br>price_bid = 1.11496<br></body></html>"#;
        assert_eq!(parse_blackbull_meta(200, raw).unwrap().price, 1.11496);
    }

    #[test]
    fn blackbull_metadata_errors_are_reported() {
        assert_eq!(
            parse_blackbull_meta(200, br#""<html><body></body></html>""#).unwrap_err(),
            "Symbol not found"
        );
        assert_eq!(
            parse_blackbull_meta(200, br#""<html><body>symbol = XAUUSD<br>price_bid = 0<br></body></html>""#)
                .unwrap_err(),
            "no price"
        );
        assert_eq!(parse_blackbull_meta(503, b"").unwrap_err(), "HTTP 503");
        assert_eq!(
            parse_blackbull_meta(200, b"{}").unwrap_err(),
            "unexpected response"
        );
    }

    #[test]
    fn encodes_reserved_characters() {
        assert_eq!(percent_encode("AAPL"), "AAPL");
        assert_eq!(percent_encode("^GSPC"), "%5EGSPC");
        assert_eq!(percent_encode("EURUSD=X"), "EURUSD%3DX");
        assert_eq!(percent_encode("BTC-USD"), "BTC-USD");
        assert_eq!(percent_encode("BRK.B"), "BRK.B");
    }
}
