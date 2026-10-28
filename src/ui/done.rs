use super::{App, Screen, file_name_of, format_duration_ms};

pub fn show(app: &mut App, ui: &mut egui::Ui) {
    let Screen::Done(o) = &app.screen else { return };
    let text = format!(
        "✓ {} · {} {} · {}",
        file_name_of(&o.md_path),
        o.speakers,
        if o.speakers == 1 { "falante" } else { "falantes" },
        format_duration_ms(o.duration_ms)
    );
    let md = o.md_path.clone();
    ui.add_space(30.0);
    ui.vertical_centered(|ui| ui.heading(text));
    ui.add_space(20.0);
    let mut again = false;
    ui.horizontal(|ui| {
        if ui.button("Abrir texto").clicked() {
            let _ = open::that_detached(&md);
        }
        if ui.button("Abrir pasta").clicked() {
            if let Some(dir) = md.parent() {
                let _ = open::that_detached(dir);
            }
        }
    });
    ui.add_space(10.0);
    if ui.button("Nova transcrição").clicked() {
        again = true;
    }
    if again {
        app.go_idle();
    }
}
