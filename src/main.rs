#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod config;
mod db;
mod edit;
mod i18n;
mod native_menu;
mod scheduler;
mod shortcuts;
mod table_shortcut;
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
    let shortcut = if std::env::args_os().nth(1).is_some_and(|arg| arg == "--open-table") {
        let payload = std::env::args_os().nth(2).and_then(|p| p.into_string().ok()).ok_or_else(|| anyhow::anyhow!(i18n::tr("Missing table shortcut target")));
        match payload.and_then(|payload| table_shortcut::decode(&payload)) {
            Ok(target) if std::env::args_os().nth(3).is_none() => Some(target),
            result => {
                let error = result.err().map_or_else(|| i18n::tr("Unexpected shortcut arguments").into(), |e| format!("{e:#}"));
                rfd::MessageDialog::new().set_title("DataCraft").set_description(error).set_level(rfd::MessageLevel::Error).show();
                return Ok(());
            }
        }
    } else { None };
    let mut options = eframe::NativeOptions {
        viewport: ui::window_chrome::viewport(),
        ..Default::default()
    };
    // Demo mode (see app.rs): don't touch the user's persisted state.
    if std::env::var("DATACRAFT_DEMO").is_ok_and(|v| !v.is_empty()) {
        options.persistence_path = Some(std::env::temp_dir().join("datacraft-demo-state"));
        options.persist_window = false;
    }
    eframe::run_native("DataCraft", options, Box::new(move |cc| {
        let mut app = app::DataCraftApp::new(cc);
        if let Some(target) = shortcut { app.start_table_shortcut(target); }
        Ok(Box::new(app))
    }))
}
