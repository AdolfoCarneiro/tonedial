//! A janela do Tone Dial (egui). Uma tela só, trocando de estado.

mod done;
mod error;
mod idle;
mod processing;
mod recording;
mod setup;

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use chrono::NaiveDateTime;

use crate::audio::decode;
use crate::audio::record::{
    Failure, Monitor, Recorder, list_input_devices, pending_recordings, recordings_dir,
    temp_wav_path, wav_duration_ms,
};
use crate::config::Config;
use crate::inhibit::SleepGuard;
use crate::models::{SetupProgress, ensure_models, missing_models, models_dir};
use crate::pipeline::{self, Job, Outcome, Progress, Source, Stage};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SetupState {
    pub file: String,
    pub done: u64,
    pub total: u64,
    pub quantizing: bool,
}

pub struct ProcessingState {
    pub name: String,
    pub stage: Stage,
    pub fraction: f32,
    pub stage_started: Instant,
    pub notice: Option<String>,
    /// Aviso fixo (ex.: microfone desconectado).
    pub note: Option<String>,
}

impl ProcessingState {
    pub fn new(name: String, note: Option<String>) -> Self {
        ProcessingState {
            name,
            stage: Stage::Preparing,
            fraction: 0.0,
            stage_started: Instant::now(),
            notice: None,
            note,
        }
    }
}

/// Gravação que ficou em `gravacoes/` sem virar transcrição.
#[derive(Debug, Clone, PartialEq)]
pub struct Pending {
    pub wav: PathBuf,
    pub started_at: NaiveDateTime,
    pub duration_ms: Option<u64>,
    /// Som do computador, se for uma call (o par do `wav`, que é o mic).
    pub system: Option<PathBuf>,
}

pub enum Screen {
    Setup(SetupState),
    Idle,
    Recording(Recorder),
    Processing(ProcessingState),
    Done(Outcome),
    Error(String),
}

#[derive(Clone, Copy)]
enum Kind {
    Setup,
    Idle,
    Recording,
    Processing,
    Done,
    Error,
}

fn kind(s: &Screen) -> Kind {
    match s {
        Screen::Setup(_) => Kind::Setup,
        Screen::Idle => Kind::Idle,
        Screen::Recording(_) => Kind::Recording,
        Screen::Processing(_) => Kind::Processing,
        Screen::Done(_) => Kind::Done,
        Screen::Error(_) => Kind::Error,
    }
}

pub struct App {
    screen: Screen,
    config: Config,
    devices: Vec<String>,
    monitor: Option<Monitor>,
    confirm_close: bool,
    force_close: bool,
    models_dir: PathBuf,
    mic_error: Option<String>,
    setup_rx: Option<Receiver<SetupProgress>>,
    proc_rx: Option<Receiver<Progress>>,
    recordings_dir: PathBuf,
    pending: Vec<Pending>,
    /// WAV pendente esperando confirmação de "Descartar".
    confirm_discard: Option<PathBuf>,
    sleep_guard: Option<SleepGuard>,
    /// Aviso da preparação dos modelos (ex.: não apagou o f16), mostrado no Idle.
    setup_warning: Option<String>,
}

/// Título da tela de processamento de uma gravação.
pub fn recording_title(started_at: NaiveDateTime) -> String {
    format!("Gravação de {}", started_at.format("%H:%M"))
}

/// "Gravação não processada de 28/10/2026 14:30 (12 min)".
pub fn pending_label(started_at: NaiveDateTime, duration_ms: Option<u64>) -> String {
    let base = format!("Gravação não processada de {}", started_at.format("%d/%m/%Y %H:%M"));
    match duration_ms {
        Some(ms) => format!("{base} ({})", format_duration_ms(ms)),
        None => base,
    }
}

/// Texto do modal de fechar: gravando ou processando.
pub fn close_message(recording: bool) -> &'static str {
    if recording {
        "Parar a gravação e fechar? O áudio fica salvo e você pode processar depois."
    } else {
        "Fechar agora? O processamento será perdido, mas o áudio continua salvo."
    }
}

/// Segura a suspensão enquanto grava, processa ou baixa os modelos.
fn inhibit_wanted(screen: &Screen, setup_running: bool) -> bool {
    match screen {
        Screen::Recording(_) | Screen::Processing(_) => true,
        Screen::Setup(_) => setup_running,
        Screen::Idle | Screen::Done(_) | Screen::Error(_) => false,
    }
}

/// Tempo restante por extrapolação linear; vazio nos primeiros 5% da etapa.
pub fn estimate_remaining(elapsed: Duration, fraction: f32) -> Option<Duration> {
    if !(0.05..=1.0).contains(&fraction) {
        return None;
    }
    Some(elapsed.mul_f32((1.0 - fraction) / fraction))
}

/// "faltam ~8 min" / "faltam ~40 s".
pub fn format_remaining(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        format!("faltam ~{} s", s.max(1))
    } else {
        format!("faltam ~{} min", s.div_ceil(60))
    }
}

/// `HH:MM:SS`.
pub fn format_clock(d: Duration) -> String {
    let s = d.as_secs();
    format!("{:02}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
}

/// `1h52` ou `12 min`.
pub fn format_duration_ms(ms: u64) -> String {
    let min = ms / 60_000;
    if min >= 60 {
        format!("{}h{:02}", min / 60, min % 60)
    } else {
        format!("{} min", min.max(1))
    }
}

impl App {
    /// Construtor puro (sem áudio, sem threads): usado também nos testes.
    pub fn with_state(config: Config, devices: Vec<String>, screen: Screen) -> Self {
        App {
            screen,
            config,
            devices,
            monitor: None,
            confirm_close: false,
            force_close: false,
            models_dir: models_dir(),
            mic_error: None,
            setup_rx: None,
            proc_rx: None,
            recordings_dir: recordings_dir(),
            pending: Vec::new(),
            confirm_discard: None,
            sleep_guard: None,
            setup_warning: None,
        }
    }

    /// Relê `gravacoes/`. Só chamar fora da gravação (o WAV em andamento também apareceria).
    fn refresh_pending(&mut self) {
        self.pending = pending_recordings(&self.recordings_dir)
            .into_iter()
            .map(|p| Pending {
                duration_ms: wav_duration_ms(&p.wav),
                wav: p.wav,
                started_at: p.started_at,
                system: p.system,
            })
            .collect();
    }

    /// Botão "Descartar": só pede confirmação.
    fn request_discard(&mut self, i: usize) {
        self.confirm_discard = self.pending.get(i).map(|p| p.wav.clone());
    }

    /// "Descartar" confirmado: apaga o WAV.
    fn discard_confirmed(&mut self) {
        let Some(wav) = self.confirm_discard.take() else { return };
        if let Err(e) = std::fs::remove_file(&wav) {
            if wav.exists() {
                self.mic_error = Some(format!("Não consegui apagar {}: {e}", wav.display()));
            }
        }
        self.refresh_pending();
    }

    /// Botão "Processar" de uma gravação pendente.
    fn process_pending(&mut self, i: usize) {
        let Some(p) = self.pending.get(i).cloned() else { return };
        if self.ensure_output_dir().is_none() {
            return;
        }
        self.monitor = None;
        self.start_processing(
            Source::Recording { wav: p.wav, started_at: p.started_at },
            recording_title(p.started_at),
            None,
        );
    }

    /// Liga/desliga o bloqueio de suspensão conforme a tela.
    fn sync_sleep_guard(&mut self) {
        let want = inhibit_wanted(&self.screen, self.setup_rx.is_some());
        if want && self.sleep_guard.is_none() {
            self.sleep_guard = Some(SleepGuard::acquire("Gravando/processando áudio"));
        } else if !want {
            self.sleep_guard = None;
        }
    }

    /// Atualiza o estado a partir de uma mensagem do pipeline. Puro.
    pub fn on_progress(&mut self, p: Progress) {
        match p {
            Progress::Done(o) => {
                if matches!(self.screen, Screen::Processing(_)) {
                    self.screen = Screen::Done(o);
                }
            }
            Progress::Failed(m) => {
                if matches!(self.screen, Screen::Processing(_)) {
                    self.screen = Screen::Error(m);
                }
            }
            other => {
                let Screen::Processing(s) = &mut self.screen else { return };
                match other {
                    Progress::Stage(st) => {
                        s.stage = st;
                        s.fraction = 0.0;
                        s.stage_started = Instant::now();
                    }
                    Progress::Fraction(f) => s.fraction = f.clamp(0.0, 1.0),
                    Progress::Notice(m) => s.notice = Some(m),
                    Progress::Done(_) | Progress::Failed(_) => {}
                }
            }
        }
    }

    pub fn new() -> Self {
        let devices = list_input_devices();
        let mut app = App::with_state(Config::load(), devices, Screen::Idle);
        app.go_idle();
        app
    }

    fn selected_mic(&self) -> Option<String> {
        match &self.config.last_mic {
            Some(m) if self.devices.contains(m) => Some(m.clone()),
            _ => self.devices.first().cloned(),
        }
    }

    fn start_monitor(&mut self) {
        self.monitor = None;
        self.mic_error = None;
        match self.selected_mic() {
            None => self.mic_error = Some("Nenhum microfone encontrado".into()),
            Some(mic) => match Monitor::start(&mic) {
                Ok(m) => self.monitor = Some(m),
                Err(e) => self.mic_error = Some(format!("{e:#}")),
            },
        }
    }

    /// Volta para o início: Setup se faltam modelos, senão Idle com o Monitor ligado.
    fn go_idle(&mut self) {
        self.monitor = None;
        self.proc_rx = None;
        if !missing_models(&self.models_dir).is_empty() {
            self.start_setup();
        } else {
            self.setup_rx = None;
            self.screen = Screen::Idle;
            self.refresh_pending();
            self.start_monitor();
        }
    }

    fn start_setup(&mut self) {
        let (tx, rx) = mpsc::channel();
        let dir = self.models_dir.clone();
        std::thread::spawn(move || ensure_models(&dir, tx));
        self.setup_rx = Some(rx);
        self.screen = Screen::Setup(SetupState::default());
    }

    fn poll_setup(&mut self) {
        let Some(rx) = &self.setup_rx else { return };
        let msgs: Vec<_> = rx.try_iter().collect();
        for m in msgs {
            match m {
                SetupProgress::Downloading { file, done, total } => {
                    self.screen = Screen::Setup(SetupState { file, done, total, quantizing: false });
                }
                SetupProgress::Quantizing => {
                    if let Screen::Setup(s) = &mut self.screen {
                        s.quantizing = true;
                    }
                }
                SetupProgress::Warning(w) => self.setup_warning = Some(w),
                SetupProgress::Finished => {
                    self.setup_rx = None;
                    self.go_idle();
                    return;
                }
                SetupProgress::Failed(e) => {
                    self.setup_rx = None;
                    self.screen = Screen::Error(format!("Não foi possível preparar os modelos: {e}"));
                    return;
                }
            }
        }
    }

    fn poll_progress(&mut self) {
        let Some(rx) = &self.proc_rx else { return };
        let msgs: Vec<_> = rx.try_iter().collect();
        for m in msgs {
            self.on_progress(m);
        }
    }

    fn save_config(&mut self) {
        if let Err(e) = self.config.save() {
            self.mic_error = Some(format!("{e:#}"));
        }
    }

    fn set_mic(&mut self, mic: String) {
        self.config.last_mic = Some(mic);
        self.save_config();
        self.start_monitor();
    }

    /// Abre o seletor de pasta; devolve `true` se o usuário escolheu uma.
    fn pick_output_dir(&mut self) -> bool {
        let mut dlg = rfd::FileDialog::new().set_title("Pasta para salvar as transcrições");
        if let Some(d) = &self.config.output_dir {
            dlg = dlg.set_directory(d);
        }
        match dlg.pick_folder() {
            Some(dir) => {
                self.config.output_dir = Some(dir);
                self.save_config();
                true
            }
            None => false,
        }
    }

    /// Garante uma pasta de saída válida. `None` se o usuário cancelou ou a pasta sumiu
    /// (neste caso a tela vira Error).
    fn ensure_output_dir(&mut self) -> Option<PathBuf> {
        if self.config.output_dir.is_none() && !self.pick_output_dir() {
            return None;
        }
        let dir = self.config.output_dir.clone()?;
        if !dir.is_dir() {
            self.monitor = None;
            self.screen = Screen::Error(MISSING_DIR_MSG.into());
            return None;
        }
        Some(dir)
    }

    /// Botão "Escolher outra pasta" da tela de erro.
    fn choose_output_dir_from_error(&mut self) {
        if self.pick_output_dir() {
            self.go_idle();
        }
    }

    fn start_recording(&mut self) {
        if self.ensure_output_dir().is_none() {
            return;
        }
        // Sem ffmpeg a gravação não viraria texto: avisa antes de gravar.
        if let Err(e) = decode::check_ffmpeg() {
            self.monitor = None;
            self.screen = Screen::Error(format!("{e:#}"));
            return;
        }
        let Some(mic) = self.selected_mic() else { return };
        if let Err(e) = std::fs::create_dir_all(&self.recordings_dir) {
            self.monitor = None;
            self.screen = Screen::Error(format!(
                "Não foi possível criar {}: {e}",
                self.recordings_dir.display()
            ));
            return;
        }
        self.monitor = None; // o Recorder abre o próprio stream
        let wav = temp_wav_path(&self.recordings_dir, chrono::Local::now().naive_local());
        match Recorder::start(&mic, wav) {
            Ok(r) => self.screen = Screen::Recording(r),
            Err(e) => self.screen = Screen::Error(format!("Não foi possível gravar: {e:#}")),
        }
    }

    /// Para a gravação e processa o que foi capturado. `failure`: por que parou sozinha.
    fn stop_recording(&mut self, failure: Option<Failure>) {
        let Screen::Recording(rec) = std::mem::replace(&mut self.screen, Screen::Idle) else {
            return;
        };
        let wav = rec.wav_path.clone();
        let started_at = rec.started_at;
        if let Err(e) = rec.stop() {
            if !wav.is_file() {
                self.screen = Screen::Error(format!("Não foi possível finalizar a gravação: {e:#}"));
                return;
            }
        }
        let note = failure.map(|f| f.note().to_string());
        self.start_processing(Source::Recording { wav, started_at }, recording_title(started_at), note);
    }

    fn open_file(&mut self, path: PathBuf) {
        if self.ensure_output_dir().is_none() {
            return;
        }
        self.monitor = None;
        let name = file_name_of(&path);
        self.start_processing(Source::File(path), name, None);
    }

    fn start_processing(&mut self, source: Source, name: String, note: Option<String>) {
        let Some(output_dir) = self.config.output_dir.clone() else {
            self.screen = Screen::Error("Escolha a pasta de saída primeiro.".into());
            return;
        };
        let job = Job { source, output_dir, models_dir: self.models_dir.clone() };
        self.proc_rx = Some(spawn_pipeline(job));
        self.screen = Screen::Processing(ProcessingState::new(name, note));
    }

    fn wants_close_confirmation(&self) -> bool {
        matches!(self.screen, Screen::Recording(_) | Screen::Processing(_))
    }

    /// Botão "Fechar" do modal. Gravando, finaliza o WAV em `gravacoes/` (oferecido na
    /// próxima abertura).
    fn close_now(&mut self, ctx: &egui::Context) {
        if let Screen::Recording(rec) = std::mem::replace(&mut self.screen, Screen::Idle) {
            let _ = rec.stop();
        }
        self.sleep_guard = None;
        self.force_close = true;
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    fn show_close_modal(&mut self, ctx: &egui::Context) {
        let mut close = false;
        let mut keep = false;
        let recording = matches!(self.screen, Screen::Recording(_));
        egui::Modal::new(egui::Id::unique("confirm_close")).show(ctx, |ui| {
            ui.set_width(300.0);
            ui.heading("Fechar?");
            ui.add_space(6.0);
            ui.label(close_message(recording));
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button("Fechar").clicked() {
                    close = true;
                }
                if ui.button("Continuar").clicked() {
                    keep = true;
                }
            });
        });
        if close {
            self.close_now(ctx);
        } else if keep {
            self.confirm_close = false;
        }
    }

    fn show_discard_modal(&mut self, ctx: &egui::Context) {
        let mut discard = false;
        let mut cancel = false;
        egui::Modal::new(egui::Id::unique("confirm_discard")).show(ctx, |ui| {
            ui.set_width(300.0);
            ui.heading("Descartar a gravação?");
            ui.add_space(6.0);
            ui.label("O áudio será apagado e não dá para desfazer.");
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button("Descartar").clicked() {
                    discard = true;
                }
                if ui.button("Cancelar").clicked() {
                    cancel = true;
                }
            });
        });
        if discard {
            self.discard_confirmed();
        } else if cancel {
            self.confirm_discard = None;
        }
    }
}

const MISSING_DIR_MSG: &str = "A pasta de saída não existe mais. Escolha outra.";

fn file_name_of(p: &std::path::Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Roda o pipeline numa thread; um panic vira `Failed`, para a UI nunca ficar presa.
fn spawn_pipeline(job: Job) -> Receiver<Progress> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let tx2 = tx.clone();
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| pipeline::run(job, tx2)));
        if let Err(payload) = r {
            let msg = payload
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "desconhecido".into());
            let _ = tx.send(Progress::Failed(format!("Erro inesperado: {msg}")));
        }
    });
    rx
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll_setup();
        self.poll_progress();

        // microfone sumiu durante a gravação: para e processa o que existe
        if let Screen::Recording(rec) = &self.screen {
            if let Some(f) = rec.failed() {
                self.stop_recording(Some(f));
            }
        }

        if ctx.input(|i| i.viewport().close_requested())
            && !self.force_close
            && self.wants_close_confirmation()
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.confirm_close = true;
        }

        egui::CentralPanel::default().show(ui, |ui| match kind(&self.screen) {
            Kind::Setup => setup::show(self, ui),
            Kind::Idle => idle::show(self, ui),
            Kind::Recording => recording::show(self, ui),
            Kind::Processing => processing::show(self, ui),
            Kind::Done => done::show(self, ui),
            Kind::Error => error::show(self, ui),
        });

        if self.confirm_close {
            self.show_close_modal(&ctx);
        } else if self.confirm_discard.is_some() {
            self.show_discard_modal(&ctx);
        }
        self.sync_sleep_guard();

        let animated = matches!(
            self.screen,
            Screen::Setup(_) | Screen::Idle | Screen::Recording(_) | Screen::Processing(_)
        );
        if animated {
            ctx.request_repaint_after(Duration::from_millis(33));
        }
    }
}

/// Barras verticais espelhadas dos níveis (0..1) numa área de ~110 px; o mais recente à direita.
pub(crate) fn draw_levels(ui: &mut egui::Ui, levels: &[f32]) {
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 110.0), egui::Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 6.0, ui.visuals().extreme_bg_color);
    let (bar_w, gap) = (3.0, 2.0);
    let n = ((rect.width() - 12.0) / (bar_w + gap)).floor() as usize;
    let tail = &levels[levels.len().saturating_sub(n)..];
    let mid = rect.center().y;
    let color = ui.visuals().hyperlink_color;
    let x0 = rect.right() - 6.0 - bar_w;
    for (i, v) in tail.iter().rev().enumerate() {
        let h = (v.clamp(0.0, 1.0).sqrt() * (rect.height() / 2.0 - 6.0)).max(1.0);
        let x = x0 - i as f32 * (bar_w + gap);
        painter.rect_filled(
            egui::Rect::from_min_max(egui::pos2(x, mid - h), egui::pos2(x + bar_w, mid + h)),
            1.0,
            color,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn processing_app() -> App {
        App::with_state(
            Config::default(),
            vec![],
            Screen::Processing(ProcessingState::new("a.m4a".into(), None)),
        )
    }

    #[test]
    fn progress_done_goes_to_done_screen() {
        let mut app = processing_app();
        app.on_progress(Progress::Done(Outcome {
            md_path: "/x/a.md".into(),
            speakers: 2,
            duration_ms: 60_000,
        }));
        assert!(matches!(app.screen, Screen::Done(ref o) if o.speakers == 2));
    }

    #[test]
    fn progress_failed_goes_to_error_screen() {
        let mut app = processing_app();
        app.on_progress(Progress::Failed("deu ruim".into()));
        assert!(matches!(app.screen, Screen::Error(ref m) if m == "deu ruim"));
    }

    #[test]
    fn notice_is_kept_in_processing_state() {
        let mut app = processing_app();
        app.on_progress(Progress::Stage(Stage::Transcribing));
        app.on_progress(Progress::Fraction(0.4));
        app.on_progress(Progress::Notice("usando CPU".into()));
        match &app.screen {
            Screen::Processing(s) => {
                assert_eq!(s.notice.as_deref(), Some("usando CPU"));
                assert_eq!(s.stage, Stage::Transcribing);
                assert!((s.fraction - 0.4).abs() < 1e-6);
            }
            _ => panic!("deveria continuar em Processing"),
        }
    }

    #[test]
    fn stage_change_resets_fraction() {
        let mut app = processing_app();
        app.on_progress(Progress::Fraction(0.9));
        app.on_progress(Progress::Stage(Stage::Diarizing));
        match &app.screen {
            Screen::Processing(s) => assert_eq!(s.fraction, 0.0),
            _ => panic!(),
        }
    }

    #[test]
    fn remaining_estimate_is_none_before_5_percent() {
        assert_eq!(estimate_remaining(Duration::from_secs(10), 0.04), None);
        assert_eq!(estimate_remaining(Duration::from_secs(10), 0.0), None);
    }

    #[test]
    fn remaining_estimate_extrapolates_linearly() {
        assert_eq!(
            estimate_remaining(Duration::from_secs(60), 0.5),
            Some(Duration::from_secs(60))
        );
    }

    fn at(h: u32, m: u32) -> chrono::NaiveDateTime {
        chrono::NaiveDate::from_ymd_opt(2026, 10, 28).unwrap().and_hms_opt(h, m, 5).unwrap()
    }

    #[test]
    fn recording_title_uses_the_start_time() {
        assert_eq!(recording_title(at(9, 5)), "Gravação de 09:05");
    }

    #[test]
    fn pending_label_text() {
        assert_eq!(
            pending_label(at(14, 30), Some(720_000)),
            "Gravação não processada de 28/10/2026 14:30 (12 min)"
        );
        assert_eq!(pending_label(at(14, 30), None), "Gravação não processada de 28/10/2026 14:30");
    }

    #[test]
    fn close_message_depends_on_state() {
        assert_eq!(
            close_message(true),
            "Parar a gravação e fechar? O áudio fica salvo e você pode processar depois."
        );
        assert_eq!(
            close_message(false),
            "Fechar agora? O processamento será perdido, mas o áudio continua salvo."
        );
    }

    #[test]
    fn inhibit_only_while_busy() {
        assert!(inhibit_wanted(&Screen::Processing(ProcessingState::new("x".into(), None)), false));
        assert!(inhibit_wanted(&Screen::Setup(SetupState::default()), true));
        assert!(!inhibit_wanted(&Screen::Setup(SetupState::default()), false));
        assert!(!inhibit_wanted(&Screen::Idle, false));
        assert!(!inhibit_wanted(&Screen::Done(Outcome { md_path: "/a.md".into(), speakers: 1, duration_ms: 1 }), false));
        assert!(!inhibit_wanted(&Screen::Error("x".into()), false));
    }

    #[test]
    fn discarding_a_pending_recording_asks_first_then_deletes() {
        let d = tempfile::tempdir().unwrap();
        let wav = d.path().join("gravacao-20261028-143005.wav");
        std::fs::write(&wav, b"x").unwrap();
        let mut app = App::with_state(Config::default(), vec![], Screen::Idle);
        app.pending = vec![Pending { wav: wav.clone(), started_at: at(14, 30), duration_ms: None, system: None }];
        app.request_discard(0);
        assert_eq!(app.confirm_discard.as_deref(), Some(wav.as_path()));
        assert!(wav.exists(), "só apaga depois de confirmar");
        app.discard_confirmed();
        assert!(!wav.exists());
        assert!(app.pending.is_empty());
        assert!(app.confirm_discard.is_none());
    }

    #[test]
    fn refresh_pending_lists_leftover_wavs() {
        let d = tempfile::tempdir().unwrap();
        let wav = crate::audio::record::temp_wav_path(d.path(), at(14, 30));
        std::fs::write(&wav, b"x").unwrap();
        let mut app = App::with_state(Config::default(), vec![], Screen::Idle);
        app.recordings_dir = d.path().to_path_buf();
        app.refresh_pending();
        assert_eq!(app.pending.len(), 1);
        assert_eq!(app.pending[0].wav, wav);
        assert_eq!(app.pending[0].started_at, at(14, 30));
    }

    #[test]
    fn formatting() {
        assert_eq!(format_clock(Duration::from_secs(3725)), "01:02:05");
        assert_eq!(format_duration_ms(6_720_000), "1h52");
        assert_eq!(format_remaining(Duration::from_secs(450)), "faltam ~8 min");
    }
}
