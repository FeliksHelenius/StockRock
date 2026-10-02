//! StockRock: a tiny native Windows stock ticker.
//!
//! Design goals are a small memory footprint and near-zero CPU: GDI rendering from a pre-drawn
//! bitmap, WinHTTP for networking, and no animation work at all unless the tape is scrolling.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod anim;
mod app;
mod config;
mod icon;
mod quotes;
mod render;
mod winhttp;
mod worker;

fn main() {
    // A GUI-subsystem process has no console, so leave a trace of any crash on disk.
    std::panic::set_hook(Box::new(|info| {
        let _ = std::fs::create_dir_all(config::dir());
        let _ = std::fs::write(config::dir().join("crash.log"), format!("{info}\n"));
    }));

    if let Err(message) = app::run() {
        app::fatal(&message);
    }
}
