#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod config;
mod db;
mod edit;
mod i18n;
mod native_menu;
mod scheduler;
mod sql;
mod update;
mod ui;
mod worker;

fn main() -> eframe::Result {
    if std::env::args_os().nth(1).is_some_and(|arg| arg == "--scheduler") {
        if let Err(error) = scheduler::run_background() {
            eprintln!("SQL scheduler: {error:#}");
            std::process::exit(1);
        }
        return Ok(());
    }
    let mut options = eframe::NativeOptions {
        viewport: ui::window_chrome::viewport(),
        ..Default::default()
    };
    // Demo mode (see app.rs): don't touch the user's persisted state.
    if std::env::var("DATACRAFT_DEMO").is_ok_and(|v| !v.is_empty()) {
        options.persistence_path = Some(std::env::temp_dir().join("datacraft-demo-state"));
        options.persist_window = false;
    }
    eframe::run_native("DataCraft", options, Box::new(|cc| Ok(Box::new(app::DataCraftApp::new(cc)))))
}
