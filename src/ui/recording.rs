use super::{App, idle};

pub fn show(app: &mut App, ui: &mut egui::Ui) {
    idle::layout(app, ui, true);
}
