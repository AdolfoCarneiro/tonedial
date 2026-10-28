use super::{App, Screen, estimate_remaining, format_remaining};
use crate::pipeline::Stage;

fn stage_text(s: Stage) -> &'static str {
    match s {
        Stage::Preparing => "Preparando áudio",
        Stage::Transcribing => "Transcrevendo",
        Stage::Diarizing => "Separando falantes",
        Stage::Writing => "Salvando",
    }
}

pub fn show(app: &mut App, ui: &mut egui::Ui) {
    let Screen::Processing(s) = &app.screen else { return };
    ui.add_space(30.0);
    ui.vertical_centered(|ui| {
        ui.heading(&s.name);
    });
    ui.add_space(20.0);
    ui.label(stage_text(s.stage));
    if s.stage == Stage::Diarizing {
        // o speakrs não informa progresso: indeterminado
        ui.horizontal(|ui| {
            ui.spinner();
            ui.weak("isso pode levar alguns minutos");
        });
    } else {
        ui.add(egui::ProgressBar::new(s.fraction).show_percentage());
        let eta = estimate_remaining(s.stage_started.elapsed(), s.fraction)
            .map(format_remaining)
            .unwrap_or_default();
        ui.weak(eta);
    }
    if let Some(n) = &s.note {
        ui.add_space(10.0);
        ui.colored_label(ui.visuals().warn_fg_color, n);
    }
    if let Some(n) = &s.notice {
        ui.add_space(10.0);
        ui.colored_label(ui.visuals().warn_fg_color, n);
    }
}
