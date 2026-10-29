use tonedial::ui::App;

const ICON_PNG: &[u8] = include_bytes!("../assets/icons/tonedial-256.png");

fn main() -> eframe::Result {
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("Tone Dial")
        .with_app_id("tonedial")
        .with_inner_size([460.0, 560.0])
        .with_min_inner_size([380.0, 480.0]);
    // No Wayland o KDE usa o Icon= do tonedial.desktop (pelo app_id); isto cobre o X11.
    if let Ok(icon) = eframe::icon_data::from_png_bytes(ICON_PNG) {
        viewport = viewport.with_icon(icon);
    }
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    eframe::run_native("Tone Dial", options, Box::new(|_cc| Ok(Box::new(App::new()))))
}
