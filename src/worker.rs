//! Background polling thread.
//!
//! One thread fetches quotes sequentially (cheapest on memory and CPU), publishes the merged
//! result for the UI thread to pick up, then sleeps until the next refresh is due. It can be
//! woken early for a manual refresh, a config change, or shutdown.

use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};

use crate::config::Config;
use crate::quotes::{self, Quote};
use crate::winhttp::Http;

/// Posted to the window when a new [`Update`] is ready to be taken.
pub const WM_QUOTES: u32 = WM_APP + 1;

/// One configured symbol with its latest known quote (if it has ever loaded).
#[derive(Debug, Clone)]
pub struct Entry {
    pub symbol: String,
    pub quote: Option<Quote>,
    /// The most recent refresh failed, so `quote` is from an earlier cycle and shouldn't look live.
    pub stale: bool,
}

pub struct Update {
    pub entries: Vec<Entry>,
    /// Summary of what went wrong in this cycle, if anything.
    pub error: Option<String>,
}

pub struct Shared {
    inner: Mutex<Inner>,
    wake: Condvar,
}

struct Inner {
    config: Arc<Config>,
    refresh: bool,
    quit: bool,
    update: Option<Update>,
}

impl Shared {
    pub fn new(config: Arc<Config>) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner {
                config,
                refresh: false,
                quit: false,
                update: None,
            }),
            wake: Condvar::new(),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Swaps in a new config and asks for an immediate refresh.
    pub fn set_config(&self, config: Arc<Config>) {
        let mut inner = self.lock();
        inner.config = config;
        inner.refresh = true;
        self.wake.notify_all();
    }

    pub fn refresh(&self) {
        self.lock().refresh = true;
        self.wake.notify_all();
    }

    pub fn quit(&self) {
        self.lock().quit = true;
        self.wake.notify_all();
    }

    pub fn take_update(&self) -> Option<Update> {
        self.lock().update.take()
    }

    fn config(&self) -> Arc<Config> {
        self.lock().config.clone()
    }

    fn is_quit(&self) -> bool {
        self.lock().quit
    }

    fn publish(&self, update: Update) {
        self.lock().update = Some(update);
    }

    /// Sleeps for `duration`, returning early on refresh/config change. Returns false on quit.
    fn sleep(&self, duration: Duration) -> bool {
        let deadline = Instant::now() + duration;
        let mut inner = self.lock();
        loop {
            if inner.quit {
                return false;
            }
            if inner.refresh {
                inner.refresh = false;
                return true;
            }
            let Some(remaining) = deadline
                .checked_duration_since(Instant::now())
                .filter(|d| !d.is_zero())
            else {
                return true;
            };
            inner = self
                .wake
                .wait_timeout(inner, remaining)
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .0;
        }
    }
}

pub fn spawn(shared: Arc<Shared>, hwnd: HWND) {
    let hwnd = hwnd.0 as isize;
    thread::Builder::new()
        .name("quotes".into())
        .stack_size(512 * 1024)
        .spawn(move || run(&shared, hwnd))
        .expect("failed to spawn quote thread");
}

fn run(shared: &Shared, hwnd: isize) {
    let hwnd = HWND(hwnd as *mut c_void);
    let notify = || unsafe {
        let _ = PostMessageW(Some(hwnd), WM_QUOTES, WPARAM(0), LPARAM(0));
    };

    let http = match Http::new(quotes::USER_AGENT) {
        Ok(http) => http,
        Err(error) => {
            shared.publish(Update {
                entries: Vec::new(),
                error: Some(error),
            });
            notify();
            return;
        }
    };

    let mut last_good: HashMap<String, Quote> = HashMap::new();
    let mut failed_cycles = 0u32;

    loop {
        let config = shared.config();
        let mut results = Vec::with_capacity(config.symbols.len());
        let mut superseded = false;

        for symbol in &config.symbols {
            if shared.is_quit() {
                return;
            }
            if !Arc::ptr_eq(&config, &shared.config()) {
                superseded = true;
                break;
            }
            results.push((symbol.clone(), quotes::fetch(&http, symbol)));
        }

        if !superseded {
            let (update, loaded) = build_update(&config.symbols, &mut last_good, results);
            shared.publish(update);
            notify();
            failed_cycles = if loaded == 0 { failed_cycles + 1 } else { 0 };
        }

        let wait = if superseded {
            Duration::ZERO
        } else if failed_cycles > 0 {
            backoff(failed_cycles)
        } else {
            Duration::from_secs(config.refresh_seconds)
        };
        if !shared.sleep(wait) {
            return;
        }
    }
}

/// Folds one polling cycle into the last-known-good quotes. Symbols that failed this cycle keep
/// their previous quote but are marked stale. Returns the update and how many symbols loaded.
fn build_update(
    symbols: &[String],
    last_good: &mut HashMap<String, Quote>,
    results: Vec<(String, Result<Quote, String>)>,
) -> (Update, usize) {
    let mut errors = Vec::new();
    let mut fresh = HashSet::new();
    for (symbol, result) in results {
        match result {
            Ok(quote) => {
                last_good.insert(symbol.clone(), quote);
                fresh.insert(symbol);
            }
            Err(error) => errors.push(format!("{symbol}: {error}")),
        }
    }
    last_good.retain(|symbol, _| symbols.contains(symbol));

    let entries = symbols
        .iter()
        .map(|symbol| Entry {
            symbol: symbol.clone(),
            quote: last_good.get(symbol).cloned(),
            stale: !fresh.contains(symbol),
        })
        .collect();
    let update = Update {
        entries,
        error: summarize(&errors),
    };
    (update, fresh.len())
}

/// Retry delay after consecutive fully-failed cycles: 15s, 30s, 60s, ... capped at 5 minutes.
fn backoff(failed_cycles: u32) -> Duration {
    let seconds = 15u64 << (failed_cycles - 1).min(5);
    Duration::from_secs(seconds.min(300))
}

fn summarize(errors: &[String]) -> Option<String> {
    let first = errors.first()?;
    Some(match errors.len() {
        1 => first.clone(),
        n => format!("{first} (+{} more)", n - 1),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_and_caps() {
        let secs: Vec<u64> = (1..=8).map(|n| backoff(n).as_secs()).collect();
        assert_eq!(secs, [15, 30, 60, 120, 240, 300, 300, 300]);
    }

    #[test]
    fn summarizes_errors() {
        assert_eq!(summarize(&[]), None);
        assert_eq!(summarize(&["a: x".into()]).as_deref(), Some("a: x"));
        assert_eq!(
            summarize(&["a: x".into(), "b: y".into()]).as_deref(),
            Some("a: x (+1 more)")
        );
    }

    fn quote(price: f64) -> Quote {
        Quote {
            price,
            prev_close: Some(price - 1.0),
        }
    }

    fn symbols(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_failed_refresh_keeps_the_old_quote_but_marks_it_stale() {
        let symbols = symbols(&["AAPL", "MSFT"]);
        let mut last_good = HashMap::new();

        let first = vec![
            ("AAPL".to_string(), Ok(quote(100.0))),
            ("MSFT".to_string(), Ok(quote(200.0))),
        ];
        let (update, loaded) = build_update(&symbols, &mut last_good, first);
        assert_eq!(loaded, 2);
        assert!(update.error.is_none());
        assert!(update.entries.iter().all(|e| e.quote.is_some() && !e.stale));

        let second = vec![
            ("AAPL".to_string(), Ok(quote(101.0))),
            ("MSFT".to_string(), Err("timed out".to_string())),
        ];
        let (update, loaded) = build_update(&symbols, &mut last_good, second);
        assert_eq!(loaded, 1);
        assert_eq!(update.error.as_deref(), Some("MSFT: timed out"));
        let (aapl, msft) = (&update.entries[0], &update.entries[1]);
        assert_eq!(aapl.quote.as_ref().unwrap().price, 101.0);
        assert!(!aapl.stale);
        assert_eq!(
            msft.quote.as_ref().unwrap().price,
            200.0,
            "old quote retained"
        );
        assert!(msft.stale);
    }

    #[test]
    fn never_loaded_symbols_have_no_quote_and_removed_symbols_are_forgotten() {
        let mut last_good = HashMap::new();
        last_good.insert("OLD".to_string(), quote(1.0));

        let results = vec![("NEW".to_string(), Err("HTTP 404".to_string()))];
        let (update, loaded) = build_update(&symbols(&["NEW"]), &mut last_good, results);
        assert_eq!(loaded, 0);
        assert!(update.entries[0].quote.is_none());
        assert!(!last_good.contains_key("OLD"));
    }

    #[test]
    fn sleep_wakes_early_on_refresh_and_stops_on_quit() {
        let shared = Shared::new(Arc::new(Config::default()));
        shared.refresh();
        let started = Instant::now();
        assert!(shared.sleep(Duration::from_secs(30)));
        assert!(started.elapsed() < Duration::from_secs(5));

        shared.quit();
        assert!(!shared.sleep(Duration::from_secs(30)));
    }
}
