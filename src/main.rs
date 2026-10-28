use tonedial::ui::App;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Tone Dial")
            .with_app_id("tonedial")
            .with_inner_size([460.0, 560.0])
            .with_min_inner_size([380.0, 480.0]),
        ..Default::default()
    };
    eframe::run_native("Tone Dial", options, Box::new(|_cc| Ok(Box::new(App::new()))))
}
