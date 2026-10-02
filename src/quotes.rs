//! Quote retrieval from Yahoo Finance's public chart endpoint (no API key needed).
//!
//! The endpoint is unofficial, so it can change or start rate-limiting without notice. Everything
//! that knows about its shape lives in this file, which makes swapping providers a local change.

use serde::Deserialize;

use crate::winhttp::Http;

pub const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) StockRock/0.1";
const HOST: &str = "query1.finance.yahoo.com";

#[derive(Debug, Clone, PartialEq)]
pub struct Quote {
    pub price: f64,
    pub prev_close: f64,
}

impl Quote {
    pub fn change(&self) -> f64 {
        self.price - self.prev_close
    }

    pub fn change_pct(&self) -> f64 {
        if self.prev_close == 0.0 {
            0.0
        } else {
            self.change() / self.prev_close * 100.0
        }
    }
}

pub fn fetch(http: &Http, symbol: &str) -> Result<Quote, String> {
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
    Ok(Quote { price, prev_close })
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
                prev_close: 333.02
            }
        );
        assert!((quote.change() + 2.70).abs() < 1e-9);
        assert!((quote.change_pct() + 0.8108).abs() < 1e-3);
    }

    #[test]
    fn falls_back_to_previous_close_then_to_price() {
        let body =
            br#"{"chart":{"result":[{"meta":{"regularMarketPrice":10.0,"previousClose":9.0}}]}}"#;
        assert_eq!(parse(200, body).unwrap().prev_close, 9.0);
        let body = br#"{"chart":{"result":[{"meta":{"regularMarketPrice":10.0}}]}}"#;
        assert_eq!(parse(200, body).unwrap().prev_close, 10.0);
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
        assert_eq!(
            Quote {
                price: 5.0,
                prev_close: 0.0
            }
            .change_pct(),
            0.0
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
