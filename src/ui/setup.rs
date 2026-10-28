use super::{App, Screen};

pub fn show(app: &mut App, ui: &mut egui::Ui) {
    let Screen::Setup(s) = &app.screen else { return };
    ui.add_space(30.0);
    ui.vertical_centered(|ui| {
        ui.heading("Preparando o Tone Dial (só na primeira vez)");
    });
    ui.add_space(20.0);
    if !s.file.is_empty() {
        ui.label(&s.file);
        let frac = if s.total > 0 { s.done as f32 / s.total as f32 } else { 0.0 };
        ui.add(egui::ProgressBar::new(frac.clamp(0.0, 1.0)).text(format!(
            "{} / {} MB",
            s.done / 1_000_000,
            s.total / 1_000_000
        )));
    } else {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label("Verificando os modelos…");
        });
    }
    if s.quantizing {
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label("Otimizando o modelo…");
        });
    }
}
