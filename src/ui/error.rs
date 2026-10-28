use super::{App, MISSING_DIR_MSG, Screen};

pub fn show(app: &mut App, ui: &mut egui::Ui) {
    let Screen::Error(msg) = &app.screen else { return };
    let msg = msg.clone();
    ui.add_space(30.0);
    ui.colored_label(ui.visuals().error_fg_color, &msg);
    ui.add_space(20.0);
    let mut back = false;
    let mut pick = false;
    ui.horizontal(|ui| {
        if ui.button("Voltar").clicked() {
            back = true;
        }
        if msg == MISSING_DIR_MSG && ui.button("✎ Escolher outra pasta").clicked() {
            pick = true;
        }
    });
    if pick {
        app.choose_output_dir_from_error();
    } else if back {
        app.go_idle();
    }
}
