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

/// BlackBull's own instrument pages poll this endpoint. It returns the live MT5 bid/ask but no
/// previous close, so the quote has no change figure.
fn fetch_blackbull(http: &Http, name: &str) -> Result<Quote, String> {
    let path = format!(
        "/wp-json/bbm/get_bid/?action=bid&symbol={}",
        percent_encode(name)
    );
    let response = http.get(BLACKBULL_HOST, &path)?;
    parse_blackbull(response.status, &response.body)
}

#[derive(Deserialize)]
struct BlackBullBid {
    result: Option<String>,
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
        return Err(bid.result.unwrap_or_else(|| "no data".into()));
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
