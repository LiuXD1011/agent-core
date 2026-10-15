#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

use agent_desktop::app::DesktopApp;
use eframe::egui;

fn main() -> eframe::Result {
    if std::env::args().any(|arg| arg == "--smoke-test") {
        match agent_desktop::app::smoke_test() {
            Ok(result) => {
                if let Some(path) = std::env::var_os("AGENT_CORE_SMOKE_REPORT") {
                    std::fs::write(path, format!("{result}\n")).expect("write smoke report");
                }
                println!("{result}");
                return Ok(());
            }
            Err(error) => {
                eprintln!("{error}");
                std::process::exit(1);
            }
        }
    }
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_visible(
                !(cfg!(feature = "ui-screenshot")
                    && std::env::var_os("EFRAME_SCREENSHOT_TO").is_some()),
            )
            .with_inner_size([1280.0, 840.0])
            .with_min_inner_size([900.0, 640.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Agent Core · Rust Desktop",
        options,
        Box::new(|cc| Ok(Box::new(DesktopApp::new(&cc.egui_ctx)))),
    )
}
