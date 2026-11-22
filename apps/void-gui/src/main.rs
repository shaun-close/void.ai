mod app;
mod layout;
mod panels;
mod state;

use std::path::PathBuf;

fn main() -> eframe::Result {
    let open = std::env::args_os().nth(1).map(PathBuf::from);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_app_id("void-ai")
            .with_title("void.ai")
            .with_inner_size([1280.0, 820.0])
            .with_min_inner_size([640.0, 400.0]),
        ..Default::default()
    };
    eframe::run_native(
        "void.ai",
        options,
        Box::new(move |cc| Ok(Box::new(app::VoidApp::new(cc, open)))),
    )
}
