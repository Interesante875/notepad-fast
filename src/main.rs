// Hide the console window on Windows release builds.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use std::path::PathBuf;

use notepad_fast::app::{NotepadApp, APP_NAME};

fn main() -> eframe::Result {
    let mut open = None;
    for arg in std::env::args_os().skip(1) {
        match arg.to_str() {
            Some("-h" | "--help") => {
                println!("{APP_NAME} {}\n\nUsage: {APP_NAME} [FILE]", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            Some("-V" | "--version") => {
                println!("{APP_NAME} {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            _ => open = Some(PathBuf::from(arg)),
        }
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(APP_NAME)
            .with_app_id(APP_NAME)
            .with_inner_size([1100.0, 720.0])
            .with_min_inner_size([420.0, 260.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native(APP_NAME, options, Box::new(move |cc| Ok(Box::new(NotepadApp::from_creation_context(cc, open)))))
}
