//! Rojo Manager: a tray app (the engine, tray icon, and control servers) that starts its window as
//! a second process only while the window is open. The tray app never loads the GPU, so it stays
//! light however often the window is opened and closed.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod agents;
mod host;
mod instance;
mod ipc;
mod logic;
mod theme;
mod tray;
mod view;
mod widgets;
mod window;

fn main() {
    if std::env::args().any(|arg| arg == "--window") {
        window::run();
    } else {
        host::run(host::Launch::parse(std::env::args().skip(1)));
    }
}
