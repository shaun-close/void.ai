mod app;
mod commands;
mod editor;
mod layout;
mod palette;
mod panels;
mod state;
mod theme;

use std::path::PathBuf;

fn main() -> eframe::Result {
    let open = std::env::args_os().nth(1).map(PathBuf::from);
    // VOID_AI_CONFIG / VOID_AI_DATA_DIR point at another config file / data folder (testing,
    // portable setups); otherwise ~/.config/void.ai/config.toml and ~/.local/share/void.ai.
    let (default_config, default_data) = void_core::store::Store::default_paths();
    let config_path = std::env::var_os("VOID_AI_CONFIG").map_or(default_config, PathBuf::from);
    let data_dir = std::env::var_os("VOID_AI_DATA_DIR").map_or(default_data, PathBuf::from);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_app_id("void-ai")
            .with_title("void.ai")
            .with_inner_size([1280.0, 820.0])
            .with_min_inner_size([640.0, 400.0]),
        // eframe would turn Ctrl+V into nothing when the clipboard holds no text, so Vim never
        // saw it; the app maps clipboard keys itself (`VoidApp::raw_input_hook`).
        clipboard_shortcuts: false,
        ..Default::default()
    };
    eframe::run_native(
        "void.ai",
        options,
        Box::new(move |cc| Ok(Box::new(app::VoidApp::new(cc, open, config_path, data_dir)))),
    )
}
