//! User configuration (TOML) and persisted window placement (JSON).
//!
//! Both live in `%APPDATA%\StockRock`.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

const DEFAULT_CONFIG: &str = r#"# StockRock configuration.
# Save this file and the ticker reloads it automatically.

# What to show, in Yahoo Finance notation: stocks (AAPL), indices (^GSPC, ^IXIC, ^DJI),
# crypto (BTC-USD), forex (EURUSD=X), futures (GC=F gold, CL=F WTI crude, BZ=F Brent crude).
symbols = ["AAPL", "MSFT", "GOOGL", "AMZN", "NVDA", "TSLA", "META", "^GSPC", "^IXIC", "BTC-USD"]

# Seconds between price refreshes (minimum 5).
refresh_seconds = 30

# Scroll speed in pixels per second at 100% display scaling. 0 = don't scroll.
scroll_speed = 60

# Font size in points.
font_size = 13

# Animation frame rate while scrolling. Lower values use less CPU.
fps = 60

# Pause scrolling while the mouse pointer is over the ticker.
pause_on_hover = true

# Optional display names, shown on the ticker instead of the symbol. Symbols with no name are
# shown as they are. Keep this section last: every key below a [header] belongs to that table.
# [names]
# "BZ=F" = "Brent"
# "^GSPC" = "S&P 500"
"#;

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub symbols: Vec<String>,
    /// Display names by (normalized) symbol. A `BTreeMap` so that keys which collide once
    /// normalized (`bz=f` and `BZ=F`) resolve the same way every time.
    pub names: BTreeMap<String, String>,
    pub refresh_seconds: u64,
    pub scroll_speed: f64,
    pub font_size: f64,
    pub fps: u32,
    pub pause_on_hover: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            symbols: [
                "AAPL", "MSFT", "GOOGL", "AMZN", "NVDA", "TSLA", "META", "^GSPC", "^IXIC",
                "BTC-USD",
            ]
            .map(String::from)
            .to_vec(),
            names: BTreeMap::new(),
            refresh_seconds: 30,
            scroll_speed: 60.0,
            font_size: 13.0,
            fps: 60,
            pause_on_hover: true,
        }
    }
}

impl Config {
    /// Normalizes symbols and clamps numeric settings to sane ranges.
    fn sanitize(mut self) -> Result<Self, String> {
        let mut seen = HashSet::new();
        self.symbols = self
            .symbols
            .into_iter()
            .map(|s| s.trim().to_uppercase())
            .filter(|s| !s.is_empty() && seen.insert(s.clone()))
            .collect();
        if self.symbols.is_empty() {
            return Err("`symbols` is empty".into());
        }
        // Names are looked up by normalized symbol; a blank name falls back to the symbol itself.
        self.names = self
            .names
            .into_iter()
            .filter_map(|(symbol, name)| {
                let (symbol, name) = (symbol.trim().to_uppercase(), name.trim().to_string());
                (!symbol.is_empty() && !name.is_empty()).then_some((symbol, name))
            })
            .collect();
        self.refresh_seconds = self.refresh_seconds.clamp(5, 86_400);
        self.scroll_speed = if self.scroll_speed.is_finite() {
            self.scroll_speed.clamp(0.0, 1000.0)
        } else {
            0.0
        };
        self.font_size = if self.font_size.is_finite() {
            self.font_size.clamp(6.0, 48.0)
        } else {
            13.0
        };
        self.fps = self.fps.clamp(5, 144);
        Ok(self)
    }
}

/// Parses and validates config text. Errors are single-line and show the offending line number.
pub fn parse(text: &str) -> Result<Config, String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let config: Config = toml::from_str(text).map_err(|e| {
        let line = e.span().map(|span| {
            let end = span.start.min(text.len());
            text.as_bytes()[..end]
                .iter()
                .filter(|&&b| b == b'\n')
                .count()
                + 1
        });
        // For typos the parser lists every valid key; the offending name is the useful part.
        let message = e.message();
        let message = message.split(", expected one of").next().unwrap_or(message);
        match line {
            Some(line) => format!("line {line}: {message}"),
            None => message.to_string(),
        }
    })?;
    config.sanitize()
}

/// Loads the config file, creating it with commented defaults on first run.
pub fn load() -> Result<Config, String> {
    let path = config_path();
    if !path.exists() {
        let _ = fs::create_dir_all(dir());
        let _ = fs::write(&path, DEFAULT_CONFIG);
    }
    let text = fs::read_to_string(&path).map_err(|e| format!("can't read config.toml: {e}"))?;
    parse(&text)
}

pub fn dir() -> PathBuf {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("StockRock")
}

pub fn config_path() -> PathBuf {
    dir().join("config.toml")
}

/// Where the window was last left. Position is in physical pixels; width is DPI-independent.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct WindowState {
    pub x: i32,
    pub y: i32,
    pub width_dip: i32,
}

fn state_path() -> PathBuf {
    dir().join("window.json")
}

pub fn load_state() -> Option<WindowState> {
    serde_json::from_str(&fs::read_to_string(state_path()).ok()?).ok()
}

pub fn save_state(state: &WindowState) {
    let _ = fs::create_dir_all(dir());
    if let Ok(json) = serde_json::to_string(state) {
        let _ = fs::write(state_path(), json);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shipped_defaults_match_default_impl() {
        let parsed = parse(DEFAULT_CONFIG).expect("default config parses");
        let default = Config::default();
        assert_eq!(parsed.symbols, default.symbols);
        assert_eq!(parsed.names, default.names);
        assert_eq!(parsed.refresh_seconds, default.refresh_seconds);
        assert_eq!(parsed.scroll_speed, default.scroll_speed);
        assert_eq!(parsed.font_size, default.font_size);
        assert_eq!(parsed.fps, default.fps);
        assert_eq!(parsed.pause_on_hover, default.pause_on_hover);
    }

    #[test]
    fn symbols_are_trimmed_uppercased_and_deduplicated() {
        let cfg = parse(r#"symbols = [" aapl", "AAPL", "", "btc-usd"]"#).unwrap();
        assert_eq!(cfg.symbols, ["AAPL", "BTC-USD"]);
    }

    #[test]
    fn empty_symbols_is_an_error() {
        assert!(parse("symbols = []").is_err());
    }

    #[test]
    fn names_match_normalized_symbols_and_keep_their_case() {
        let cfg = parse(
            r#"
symbols = ["BZ=F", "^GSPC"]
[names]
" bz=f " = "  Brent "
"^GSPC" = "S&P 500"
"#,
        )
        .unwrap();
        assert_eq!(cfg.names.len(), 2);
        assert_eq!(cfg.names["BZ=F"], "Brent");
        assert_eq!(cfg.names["^GSPC"], "S&P 500");
    }

    #[test]
    fn blank_names_are_dropped_so_the_symbol_shows() {
        let cfg = parse("symbols = [\"AAPL\"]\n[names]\nAAPL = \"  \"\n\"\" = \"x\"").unwrap();
        assert!(cfg.names.is_empty());
    }

    #[test]
    fn a_name_that_is_not_text_is_reported_with_its_line() {
        let err = parse("symbols = [\"AAPL\"]\n[names]\nAAPL = 5").unwrap_err();
        assert!(
            err.starts_with("line 3:") && err.contains("expected"),
            "{err}"
        );
    }

    #[test]
    fn keys_below_the_names_table_belong_to_it() {
        // The mistake the comment in the default config warns about: it is caught, with a line.
        let err = parse("[names]\nAAPL = \"Apple\"\nfps = 30").unwrap_err();
        assert!(err.starts_with("line 3:"), "{err}");
    }

    #[test]
    fn numbers_are_clamped() {
        let cfg = parse("refresh_seconds = 1\nfps = 1000\nfont_size = 500").unwrap();
        assert_eq!(cfg.refresh_seconds, 5);
        assert_eq!(cfg.fps, 144);
        assert_eq!(cfg.font_size, 48.0);
    }

    #[test]
    fn typos_are_reported_with_a_line_number() {
        let err = parse("fps = 30\nrefresh_second = 10").unwrap_err();
        assert!(err.starts_with("line 2:"), "{err}");
        assert!(err.contains("`refresh_second`"), "{err}");
        assert!(!err.contains("expected one of"), "{err}");
    }

    #[test]
    fn type_errors_keep_the_expectation() {
        let err = parse("fps = \"fast\"").unwrap_err();
        assert!(
            err.starts_with("line 1:") && err.contains("expected"),
            "{err}"
        );
    }

    #[test]
    fn utf8_bom_is_tolerated() {
        assert!(parse("\u{feff}fps = 30").is_ok());
    }
}
