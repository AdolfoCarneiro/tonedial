//! Tela principal: usada no Idle (Monitor ligado) e, com `recording = true`, na gravação.

use std::time::Duration;

use super::{App, Screen, draw_levels, format_clock};

const AUDIO_EXTENSIONS: &[&str] = &["m4a", "mp3", "ogg", "opus", "wav", "aac", "flac", "webm"];

pub fn show(app: &mut App, ui: &mut egui::Ui) {
    layout(app, ui, false);
}

pub fn layout(app: &mut App, ui: &mut egui::Ui, recording: bool) {
    let mut new_mic: Option<String> = None;
    let mut toggle_record = false;
    let mut open_dialog = false;
    let mut change_dir = false;

    ui.label("Microfone");
    let current = app.selected_mic().unwrap_or_else(|| "(nenhum)".into());
    ui.add_enabled_ui(!recording, |ui| {
        egui::ComboBox::from_id_salt("mic")
            .selected_text(&current)
            .width(ui.available_width())
            .show_ui(ui, |ui| {
                for d in &app.devices {
                    if ui.selectable_label(*d == current, d).clicked() && *d != current {
                        new_mic = Some(d.clone());
                    }
                }
            });
    });
    if let Some(e) = &app.mic_error {
        ui.colored_label(ui.visuals().warn_fg_color, e);
    }
    ui.add_space(8.0);

    let (levels, elapsed) = match &app.screen {
        Screen::Recording(r) => (r.levels.snapshot(), r.elapsed()),
        _ => (
            app.monitor.as_ref().map(|m| m.levels.snapshot()).unwrap_or_default(),
            Duration::ZERO,
        ),
    };
    draw_levels(ui, &levels);

    ui.add_space(8.0);
    ui.vertical_centered(|ui| {
        ui.label(egui::RichText::new(format_clock(elapsed)).monospace().size(24.0));
    });
    ui.add_space(8.0);

    let label = if recording { "■ Parar" } else { "● Gravar" };
    let btn = egui::Button::new(egui::RichText::new(label).size(16.0))
        .min_size(egui::vec2(ui.available_width(), 38.0));
    if ui.add(btn).clicked() {
        toggle_record = true;
    }

    ui.add_space(14.0);
    ui.horizontal(|ui| {
        let line_w = (ui.available_width() - 40.0) / 2.0;
        let y = ui.cursor().center().y;
        let stroke = ui.visuals().widgets.noninteractive.bg_stroke;
        let x0 = ui.cursor().left();
        ui.painter().hline(x0..=x0 + line_w, y, stroke);
        ui.painter().hline(x0 + line_w + 40.0..=x0 + 2.0 * line_w + 40.0, y, stroke);
        ui.add_space(line_w + 12.0);
        ui.weak("ou");
    });
    ui.add_space(8.0);

    // área de soltar / clicar
    let hovering = ui.ctx().input(|i| !i.raw.hovered_files.is_empty());
    let (rect, resp) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), 64.0),
        if recording { egui::Sense::hover() } else { egui::Sense::click() },
    );
    let stroke_color = if hovering && !recording {
        ui.visuals().hyperlink_color
    } else {
        ui.visuals().weak_text_color()
    };
    let r = rect.shrink(1.0);
    let pts = [r.left_top(), r.right_top(), r.right_bottom(), r.left_bottom(), r.left_top()];
    ui.painter()
        .extend(egui::Shape::dashed_line(&pts, egui::Stroke::new(1.0, stroke_color), 6.0, 4.0));
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        "Arraste um áudio aqui ou clique para abrir",
        egui::FontId::proportional(14.0),
        if recording { ui.visuals().weak_text_color() } else { ui.visuals().text_color() },
    );
    if !recording && resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
        open_dialog = true;
    }

    ui.add_space(12.0);
    ui.horizontal(|ui| {
        let dir = app
            .config
            .output_dir
            .as_ref()
            .map(|d| d.display().to_string())
            .unwrap_or_else(|| "(escolher pasta)".into());
        ui.weak(format!("Salvar em: {dir}"));
        if ui.add_enabled(!recording, egui::Button::new("✎").frame(false)).clicked() {
            change_dir = true;
        }
    });

    // ações (fora dos closures, para poder mexer no App inteiro)
    if let Some(m) = new_mic {
        app.set_mic(m);
    }
    if change_dir {
        app.pick_output_dir();
    }
    if toggle_record {
        if recording {
            app.stop_recording(false);
        } else {
            app.start_recording();
        }
        return;
    }
    if recording {
        return;
    }
    if open_dialog {
        let picked = rfd::FileDialog::new()
            .set_title("Escolha um áudio")
            .add_filter("Áudio", AUDIO_EXTENSIONS)
            .pick_file();
        if let Some(p) = picked {
            app.open_file(p);
        }
        return;
    }
    let dropped = ui
        .ctx()
        .input(|i| i.raw.dropped_files.iter().next().map(|f| f.path().to_path_buf()));
    if let Some(p) = dropped {
        app.open_file(p);
    }
}
