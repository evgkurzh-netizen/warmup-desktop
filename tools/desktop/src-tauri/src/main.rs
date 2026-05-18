// Prevent an extra Windows console window from popping up in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    warmup_desktop_lib::run()
}
