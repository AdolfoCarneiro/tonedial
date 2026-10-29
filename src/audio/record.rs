use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow};
use chrono::{Local, NaiveDateTime};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{ErrorKind, FromSample, Sample, SampleFormat};

const MAX_LEVELS: usize = 300;
/// Um valor de pico a cada ~33 ms.
const LEVEL_WINDOW_MS: u64 = 33;

pub(crate) fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Dispositivos que não são microfone de verdade (o `null` do ALSA grava silêncio).
fn is_unusable(name: &str) -> bool {
    name.starts_with("Discard all samples")
}

fn is_null_device(id: Option<&str>, name: &str) -> bool {
    id == Some("alsa:null") || is_unusable(name)
}

/// Índice do candidato (id, nome) com esse nome; entre homônimos, o que tem o id do padrão.
fn pick_device(cands: &[(Option<String>, String)], name: &str, default_id: Option<&str>) -> Option<usize> {
    let by_name = |(_, n): &(Option<String>, String)| n == name;
    if let Some(did) = default_id
        && let Some(i) = cands.iter().position(|c| by_name(c) && c.0.as_deref() == Some(did))
    {
        return Some(i);
    }
    cands.iter().position(by_name)
}

/// Esta entrada é o microfone padrão? Sem id do padrão, nenhuma é.
fn is_default_entry<T: PartialEq>(id: Option<T>, default_id: Option<T>) -> bool {
    default_id.is_some() && id == default_id
}

/// Padrão primeiro, depois os demais na ordem original; sem duplicatas e sem `null`.
fn order_devices(all: Vec<String>, default: Option<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |n: String| {
        if !is_unusable(&n) && !out.contains(&n) {
            out.push(n);
        }
    };
    if let Some(d) = &default
        && all.contains(d)
    {
        push(d.clone());
    }
    for n in all {
        push(n);
    }
    out
}

/// Nomes dos microfones disponíveis (host padrão), o padrão do sistema primeiro.
pub fn list_input_devices() -> Vec<String> {
    let host = cpal::default_host();
    let Ok(devices) = host.input_devices() else {
        return Vec::new();
    };
    let default_id = host.default_input_device().and_then(|d| d.id().ok());
    let mut default_name = None;
    let mut all = Vec::new();
    for d in devices {
        let Ok(desc) = d.description() else { continue };
        let name = desc.name().to_string();
        let id = d.id().ok();
        if is_null_device(id.as_ref().map(|i| i.to_string()).as_deref(), &name) {
            continue;
        }
        // O nome da descrição do padrão ("Default Audio Device") não está na lista;
        // por isso o padrão é identificado pelo id e listado com o nome da sua entrada.
        if default_name.is_none() && is_default_entry(id.as_ref(), default_id.as_ref()) {
            default_name = Some(name.clone());
        }
        all.push(name);
    }
    order_devices(all, default_name)
}

/// Últimos ~300 valores de pico (0..1), um a cada ~33 ms.
#[derive(Clone, Default)]
pub struct Levels(Arc<Mutex<VecDeque<f32>>>);

impl Levels {
    pub fn snapshot(&self) -> Vec<f32> {
        lock(&self.0).iter().copied().collect()
    }

    fn push(&self, v: f32) {
        let mut q = lock(&self.0);
        if q.len() >= MAX_LEVELS {
            q.pop_front();
        }
        q.push_back(v);
    }
}

/// Acumula picos e publica um valor a cada janela de ~33 ms.
pub(crate) struct Meter {
    levels: Levels,
    window: usize,
    count: usize,
    peak: f32,
}

impl Meter {
    pub(crate) fn new(levels: Levels, sample_rate: u32) -> Self {
        let window = (sample_rate as u64 * LEVEL_WINDOW_MS / 1000).max(1) as usize;
        Meter { levels, window, count: 0, peak: 0.0 }
    }

    pub(crate) fn feed(&mut self, mono: &[f32]) {
        for &s in mono {
            self.peak = self.peak.max(s.abs());
            self.count += 1;
            if self.count >= self.window {
                self.levels.push(self.peak.min(1.0));
                self.count = 0;
                self.peak = 0.0;
            }
        }
    }
}

/// WAV mono f32 na taxa nativa do dispositivo.
pub struct WavSink {
    writer: hound::WavWriter<std::io::BufWriter<std::fs::File>>,
    since_flush: usize,
    flush_every: usize,
}

impl WavSink {
    pub fn create(path: &Path, sample_rate: u32) -> anyhow::Result<Self> {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let writer = hound::WavWriter::create(path, spec)
            .with_context(|| format!("não foi possível criar {}", path.display()))?;
        Ok(WavSink { writer, since_flush: 0, flush_every: sample_rate as usize })
    }

    pub fn push(&mut self, mono: &[f32]) -> anyhow::Result<()> {
        for &s in mono {
            self.writer.write_sample(s)?;
        }
        self.since_flush += mono.len();
        if self.since_flush >= self.flush_every {
            // flush também atualiza o cabeçalho: o arquivo fica legível mesmo se o app cair
            self.writer.flush()?;
            self.since_flush = 0;
        }
        Ok(())
    }

    pub fn finish(self) -> anyhow::Result<()> {
        self.writer.finalize()?;
        Ok(())
    }
}

const WAV_PREFIX: &str = "gravacao-";
const WAV_TIME_FORMAT: &str = "%Y%m%d-%H%M%S";

/// Onde o WAV fica enquanto grava e até virar .ogg: `~/.local/share/tonedial/gravacoes`.
/// Fora da pasta de saída, para não sumir com ela nem ser sincronizado pela metade.
pub fn recordings_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("tonedial")
        .join("gravacoes")
}

/// `<dir>/gravacao-<AAAAMMDD-HHMMSS>.wav`
pub fn temp_wav_path(dir: &Path, at: NaiveDateTime) -> PathBuf {
    dir.join(format!("{WAV_PREFIX}{}.wav", at.format(WAV_TIME_FORMAT)))
}

/// `(mic, sistema)`: `<dir>/gravacao-<AAAAMMDD-HHMMSS>-mic.wav` e `...-sistema.wav`.
pub fn temp_call_paths(dir: &Path, at: NaiveDateTime) -> (PathBuf, PathBuf) {
    let stamp = at.format(WAV_TIME_FORMAT);
    (
        dir.join(format!("{WAV_PREFIX}{stamp}-mic.wav")),
        dir.join(format!("{WAV_PREFIX}{stamp}-sistema.wav")),
    )
}

/// Gravação que não chegou a virar .ogg. Em call, `wav` é o mic e `system` o som do computador.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingAudio {
    pub started_at: NaiveDateTime,
    pub wav: PathBuf,
    pub system: Option<PathBuf>,
}

/// WAVs de gravações que não chegaram a virar .ogg (app fechado, falha...), mais antigos primeiro.
/// Os dois WAVs de uma call viram um item só; um lado sozinho vira gravação simples.
pub fn pending_recordings(dir: &Path) -> Vec<PendingAudio> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    // por carimbo: (simples, mic, sistema)
    let mut groups: std::collections::BTreeMap<NaiveDateTime, [Option<PathBuf>; 3]> = Default::default();
    for p in entries.flatten().map(|e| e.path()).filter(|p| p.is_file()) {
        let Some(name) = p.file_name().and_then(|n| n.to_str()) else { continue };
        let Some(stem) = name.strip_prefix(WAV_PREFIX).and_then(|n| n.strip_suffix(".wav")) else { continue };
        let (ts, slot) = if let Some(t) = stem.strip_suffix("-mic") {
            (t, 1)
        } else if let Some(t) = stem.strip_suffix("-sistema") {
            (t, 2)
        } else {
            (stem, 0)
        };
        let Ok(at) = NaiveDateTime::parse_from_str(ts, WAV_TIME_FORMAT) else { continue };
        groups.entry(at).or_default()[slot] = Some(p);
    }
    let mut out = Vec::new();
    for (started_at, [simple, mic, sis]) in groups {
        match (simple, mic, sis) {
            (simple, Some(mic), Some(sis)) => {
                out.push(PendingAudio { started_at, wav: mic, system: Some(sis) });
                out.extend(simple.map(|wav| PendingAudio { started_at, wav, system: None }));
            }
            (simple, mic, sis) => {
                out.extend(simple.or(mic).or(sis).map(|wav| PendingAudio { started_at, wav, system: None }));
            }
        }
    }
    out
}

/// Duração pelo cabeçalho do WAV (atualizado a cada ~1 s durante a gravação).
pub fn wav_duration_ms(path: &Path) -> Option<u64> {
    let r = hound::WavReader::open(path).ok()?;
    let rate = u64::from(r.spec().sample_rate);
    (rate > 0).then(|| u64::from(r.duration()) * 1000 / rate)
}

/// Apaga `path` se `r` for erro (o WAV vazio de uma gravação que nem começou).
fn remove_on_err<T>(r: anyhow::Result<T>, path: &Path) -> anyhow::Result<T> {
    if r.is_err() {
        let _ = std::fs::remove_file(path);
    }
    r
}

/// Stream aberto. `cpal::Stream` não é `Send`: fica na thread da UI.
struct Capture {
    _stream: cpal::Stream,
}

fn mono_mix<T: Sample>(data: &[T], channels: usize, buf: &mut Vec<f32>)
where
    f32: FromSample<T>,
{
    buf.clear();
    if channels <= 1 {
        buf.extend(data.iter().map(|&s| f32::from_sample(s)));
    } else {
        buf.extend(data.chunks_exact(channels).map(|frame| {
            frame.iter().map(|&s| f32::from_sample(s)).sum::<f32>() / channels as f32
        }));
    }
}

fn find_device(name: &str) -> anyhow::Result<cpal::Device> {
    let host = cpal::default_host();
    if !name.is_empty()
        && let Ok(devices) = host.input_devices()
    {
        let devices: Vec<cpal::Device> = devices.collect();
        let cands: Vec<(Option<String>, String)> = devices
            .iter()
            .map(|d| {
                (
                    d.id().ok().map(|i| i.to_string()),
                    d.description().map(|x| x.name().to_string()).unwrap_or_default(),
                )
            })
            .collect();
        let default_id = host.default_input_device().and_then(|d| d.id().ok()).map(|i| i.to_string());
        if let Some(i) = pick_device(&cands, name, default_id.as_deref()) {
            return Ok(devices.into_iter().nth(i).expect("índice válido"));
        }
    }
    // microfone salvo sumiu (ou nome vazio): cai para o padrão
    host.default_input_device()
        .ok_or_else(|| anyhow!("nenhum microfone encontrado"))
}

/// Abre o microfone e entrega blocos mono (média dos canais) ao callback.
/// `make_cb` recebe a taxa nativa e devolve o callback de dados.
fn open_capture<F>(
    device: &str,
    make_cb: impl FnOnce(u32) -> anyhow::Result<F>,
    on_error: impl Fn(String) + Send + 'static,
) -> anyhow::Result<Capture>
where
    F: FnMut(&[f32]) + Send + 'static,
{
    let dev = find_device(device)?;
    let supported = dev
        .default_input_config()
        .map_err(|e| anyhow!("não foi possível configurar o microfone: {e}"))?;
    let sample_rate = supported.sample_rate();
    let channels = supported.channels() as usize;
    let format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();
    let mut on_mono = make_cb(sample_rate)?;

    let err_cb = move |e: cpal::Error| match e.kind() {
        ErrorKind::DeviceChanged | ErrorKind::Xrun | ErrorKind::RealtimeDenied => {}
        _ => on_error(e.to_string()),
    };

    macro_rules! build {
        ($t:ty) => {{
            let mut buf: Vec<f32> = Vec::new();
            dev.build_input_stream(
                config,
                move |data: &[$t], _: &_| {
                    mono_mix(data, channels, &mut buf);
                    on_mono(&buf);
                },
                err_cb,
                None,
            )
        }};
    }
    let stream = match format {
        SampleFormat::F32 => build!(f32),
        SampleFormat::I16 => build!(i16),
        SampleFormat::I32 => build!(i32),
        SampleFormat::I8 => build!(i8),
        SampleFormat::U16 => build!(u16),
        other => return Err(anyhow!("formato de áudio não suportado: {other}")),
    }
    .map_err(|e| anyhow!("não foi possível abrir o microfone: {e}"))?;
    stream
        .play()
        .map_err(|e| anyhow!("não foi possível iniciar o microfone: {e}"))?;
    Ok(Capture { _stream: stream })
}

/// Stream aberto só para mostrar as ondinhas (sem gravar nada).
pub struct Monitor {
    pub levels: Levels,
    _capture: Capture,
}

impl Monitor {
    pub fn start(device: &str) -> anyhow::Result<Monitor> {
        let levels = Levels::default();
        let lv = levels.clone();
        let capture = open_capture(
            device,
            move |rate| {
                let mut meter = Meter::new(lv, rate);
                Ok(move |mono: &[f32]| meter.feed(mono))
            },
            |_| {},
        )?;
        Ok(Monitor { levels, _capture: capture })
    }
}

/// Por que a gravação parou sozinha.
#[derive(Debug, Clone, PartialEq)]
pub enum Failure {
    /// O microfone sumiu ou o stream deu erro.
    Mic(String),
    /// Não deu para escrever o WAV (disco cheio etc.).
    Disk(String),
}

impl Failure {
    /// Aviso mostrado enquanto processa o que foi gravado.
    pub fn note(&self) -> &'static str {
        match self {
            Failure::Mic(_) => "O microfone desconectou; processando o que foi gravado",
            Failure::Disk(_) => "Erro ao gravar no disco; processando o que foi gravado",
        }
    }
}

enum Msg {
    Data(Vec<f32>),
    Stop,
}

fn writer_loop(
    rx: Receiver<Msg>,
    mut sink: WavSink,
    failure: Arc<Mutex<Option<Failure>>>,
) -> anyhow::Result<()> {
    let mut broken = false;
    while let Ok(Msg::Data(chunk)) = rx.recv() {
        if broken {
            continue;
        }
        if let Err(e) = sink.push(&chunk) {
            // disco cheio etc.: guarda o motivo e mantém o que já foi escrito
            *lock(&failure) = Some(Failure::Disk(format!("erro ao gravar o arquivo: {e}")));
            broken = true;
        }
    }
    sink.finish()
}

pub struct Recorder {
    pub levels: Levels,
    pub wav_path: PathBuf,
    pub started_at: NaiveDateTime,
    started: Instant,
    failure: Arc<Mutex<Option<Failure>>>,
    tx: Sender<Msg>,
    writer: Option<JoinHandle<anyhow::Result<()>>>,
    capture: Option<Capture>,
}

impl Recorder {
    /// Abre o microfone e começa a gravar em `wav_path`. Se falhar, não deixa o WAV vazio.
    pub fn start(device: &str, wav_path: PathBuf) -> anyhow::Result<Recorder> {
        let path = wav_path.clone();
        remove_on_err(Self::start_inner(device, wav_path), &path)
    }

    fn start_inner(device: &str, wav_path: PathBuf) -> anyhow::Result<Recorder> {
        let levels = Levels::default();
        let failure: Arc<Mutex<Option<Failure>>> = Arc::default();
        let (tx, rx) = mpsc::channel::<Msg>();

        // A taxa nativa só é conhecida ao abrir o dispositivo; o WAV é criado nesse momento.
        let sink_slot: Arc<Mutex<Option<WavSink>>> = Arc::default();
        let (slot, lv, path, tx_cb) = (sink_slot.clone(), levels.clone(), wav_path.clone(), tx.clone());
        let fail_cb = failure.clone();
        let got_data = Arc::new(AtomicBool::new(false));
        let got_cb = got_data.clone();
        let capture = open_capture(
            device,
            move |rate| {
                *lock(&slot) = Some(WavSink::create(&path, rate)?);
                let mut meter = Meter::new(lv, rate);
                Ok(move |mono: &[f32]| {
                    got_cb.store(true, Ordering::Release);
                    meter.feed(mono);
                    let _ = tx_cb.send(Msg::Data(mono.to_vec()));
                })
            },
            move |msg| {
                let mut f = lock(&fail_cb);
                if f.is_none() {
                    *f = Some(Failure::Mic(msg));
                }
            },
        )?;
        let sink = lock(&sink_slot)
            .take()
            .ok_or_else(|| anyhow!("falha ao preparar o arquivo de áudio"))?;
        // O PipeWire pode levar uns décimos de segundo para acordar o microfone: espera o
        // primeiro bloco para o cronômetro e o WAV começarem juntos.
        let deadline = Instant::now() + Duration::from_secs(3);
        while !got_data.load(Ordering::Acquire) {
            if let Some(Failure::Mic(e) | Failure::Disk(e)) = lock(&failure).clone() {
                return Err(anyhow!("{e}"));
            }
            if Instant::now() > deadline {
                return Err(anyhow!("o microfone não respondeu"));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let fail_w = failure.clone();
        let writer = std::thread::spawn(move || writer_loop(rx, sink, fail_w));
        Ok(Recorder {
            levels,
            wav_path,
            started_at: Local::now().naive_local(),
            started: Instant::now(),
            failure,
            tx,
            writer: Some(writer),
            capture: Some(capture),
        })
    }

    /// Gravador sem microfone (testes): o WAV fica vazio, e o `stop` funciona como o de verdade.
    #[cfg(test)]
    pub(crate) fn fake(wav_path: PathBuf) -> Recorder {
        let sink = WavSink::create(&wav_path, 48_000).expect("WAV do gravador falso");
        let failure: Arc<Mutex<Option<Failure>>> = Arc::default();
        let (tx, rx) = mpsc::channel::<Msg>();
        let fail_w = failure.clone();
        Recorder {
            levels: Levels::default(),
            wav_path,
            started_at: Local::now().naive_local(),
            started: Instant::now(),
            failure,
            tx,
            writer: Some(std::thread::spawn(move || writer_loop(rx, sink, fail_w))),
            capture: None,
        }
    }

    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// Instante do primeiro bloco do microfone: referência de sincronia para o som do computador.
    pub(crate) fn started_instant(&self) -> Instant {
        self.started
    }

    /// Motivo, se o microfone sumiu ou a escrita falhou.
    pub fn failed(&self) -> Option<Failure> {
        lock(&self.failure).clone()
    }

    /// Finaliza o WAV e devolve o caminho. Funciona também depois de `failed()`.
    pub fn stop(mut self) -> anyhow::Result<PathBuf> {
        drop(self.capture.take()); // para os callbacks
        let _ = self.tx.send(Msg::Stop);
        if let Some(h) = self.writer.take() {
            h.join().map_err(|_| anyhow!("thread de gravação falhou"))??;
        }
        Ok(self.wav_path.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    fn cand(id: &str, name: &str) -> (Option<String>, String) {
        (Some(id.to_string()), name.to_string())
    }

    #[test]
    fn pick_device_prefere_o_padrao_entre_nomes_iguais() {
        let c = vec![cand("alsa:hw", "HDA"), cand("alsa:default", "HDA"), cand("x", "Outro")];
        assert_eq!(pick_device(&c, "HDA", Some("alsa:default")), Some(1));
        assert_eq!(pick_device(&c, "HDA", None), Some(0));
        assert_eq!(pick_device(&c, "HDA", Some("outro-id")), Some(0));
        assert_eq!(pick_device(&c, "Outro", Some("alsa:default")), Some(2));
        assert_eq!(pick_device(&c, "Sumiu", Some("alsa:default")), None);
    }

    #[test]
    fn null_tambem_por_id() {
        assert!(is_null_device(Some("alsa:null"), "qualquer"));
        assert!(is_null_device(None, "Discard all samples (x)"));
        assert!(!is_null_device(Some("alsa:default"), "HDA"));
    }

    #[test]
    fn padrao_vem_primeiro_e_null_some() {
        let all = v(&["Discard all samples (playback) or generate zero samples (capture)", "PipeWire", "Padrao", "HDA"]);
        assert_eq!(order_devices(all, Some("Padrao".into())), v(&["Padrao", "PipeWire", "HDA"]));
    }

    #[test]
    fn sem_duplicatas() {
        let all = v(&["HDA", "HDA", "Padrao", "Padrao"]);
        assert_eq!(order_devices(all, Some("Padrao".into())), v(&["Padrao", "HDA"]));
    }

    #[test]
    fn sem_padrao_mantem_ordem_sem_null() {
        let all = v(&["Discard all samples (x)", "B", "A"]);
        assert_eq!(order_devices(all.clone(), None), v(&["B", "A"]));
        assert_eq!(order_devices(all, Some("fora da lista".into())), v(&["B", "A"]));
    }

    #[test]
    fn padrao_null_nao_e_usado() {
        let all = v(&["Discard all samples (x)", "B"]);
        assert_eq!(order_devices(all, Some("Discard all samples (x)".into())), v(&["B"]));
    }

    #[test]
    fn is_unusable_so_pega_null() {
        assert!(is_unusable("Discard all samples (playback) or generate zero samples (capture)"));
        assert!(!is_unusable("HDA Intel PCH, ALC257 Analog"));
        assert!(!is_unusable("PipeWire Sound Server"));
    }


    fn tmp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("td-{}-{}.wav", name, std::process::id()))
    }

    #[test]
    fn wav_sink_writes_readable_file() {
        let p = tmp("full");
        let mut s = WavSink::create(&p, 48_000).unwrap();
        s.push(&vec![0.1; 48_000]).unwrap();
        s.finish().unwrap();
        let r = hound::WavReader::open(&p).unwrap();
        assert_eq!(r.spec().sample_rate, 48_000);
        assert_eq!(r.spec().channels, 1);
        assert_eq!(r.len(), 48_000);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn wav_sink_finish_after_partial_push_is_valid() {
        let p = tmp("partial");
        let mut s = WavSink::create(&p, 16_000).unwrap();
        s.push(&vec![0.2; 8_000]).unwrap();
        s.push(&vec![-0.2; 4_000]).unwrap();
        s.finish().unwrap();
        let r = hound::WavReader::open(&p).unwrap();
        assert_eq!(r.len(), 12_000);
        assert_eq!(r.duration() as f32 / 16_000.0, 0.75);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn wav_sink_create_in_missing_dir_errors() {
        let p = std::env::temp_dir().join("td-nao-existe-xyz/a.wav");
        assert!(WavSink::create(&p, 16_000).is_err());
    }

    #[test]
    fn meter_emits_one_peak_per_window() {
        let levels = Levels::default();
        let mut m = Meter::new(levels.clone(), 1_000); // janela de 33 amostras
        let mut data = vec![0.1; 66];
        data[10] = -0.8;
        m.feed(&data);
        assert_eq!(levels.snapshot(), vec![0.8, 0.1]);
    }

    #[test]
    fn levels_keep_only_last_300() {
        let l = Levels::default();
        for i in 0..400 {
            l.push(i as f32 / 400.0);
        }
        let v = l.snapshot();
        assert_eq!(v.len(), 300);
        assert_eq!(v[0], 100.0 / 400.0);
    }

    #[test]
    fn mono_mix_averages_channels() {
        let mut buf = Vec::new();
        mono_mix(&[1.0f32, 0.0, 0.5, 0.5], 2, &mut buf);
        assert_eq!(buf, vec![0.5, 0.5]);
    }

    fn at(h: u32, m: u32, s: u32) -> NaiveDateTime {
        chrono::NaiveDate::from_ymd_opt(2026, 10, 28).unwrap().and_hms_opt(h, m, s).unwrap()
    }

    #[test]
    fn temp_wav_path_format() {
        assert_eq!(
            temp_wav_path(Path::new("/dados/gravacoes"), at(9, 5, 7)),
            PathBuf::from("/dados/gravacoes/gravacao-20261028-090507.wav")
        );
    }

    #[test]
    fn recordings_dir_is_in_data_dir() {
        let d = recordings_dir();
        assert!(d.ends_with("tonedial/gravacoes"), "{}", d.display());
    }

    #[test]
    fn pending_recordings_finds_leftover_wavs_sorted() {
        let d = tempfile::tempdir().unwrap();
        let a = temp_wav_path(d.path(), at(14, 30, 0));
        let b = temp_wav_path(d.path(), at(9, 5, 7));
        std::fs::write(&a, b"x").unwrap();
        std::fs::write(&b, b"x").unwrap();
        std::fs::write(d.path().join("outro.wav"), b"x").unwrap();
        std::fs::write(d.path().join("gravacao-lixo.wav"), b"x").unwrap();
        std::fs::write(d.path().join("gravacao-20261028-090507.ogg"), b"x").unwrap();
        assert_eq!(
            pending_recordings(d.path()),
            vec![
                PendingAudio { started_at: at(9, 5, 7), wav: b, system: None },
                PendingAudio { started_at: at(14, 30, 0), wav: a, system: None },
            ]
        );
    }

    #[test]
    fn pendentes_agrupam_par_de_call() {
        let d = tempfile::tempdir().unwrap();
        let (mic, sis) = temp_call_paths(d.path(), at(14, 30, 5));
        let simples = temp_wav_path(d.path(), at(15, 0, 0));
        for p in [&mic, &sis, &simples] {
            std::fs::write(p, b"x").unwrap();
        }
        assert_eq!(
            pending_recordings(d.path()),
            vec![
                PendingAudio { started_at: at(14, 30, 5), wav: mic, system: Some(sis) },
                PendingAudio { started_at: at(15, 0, 0), wav: simples, system: None },
            ]
        );
    }

    #[test]
    fn par_incompleto_vira_gravacao_simples() {
        let d = tempfile::tempdir().unwrap();
        let (_, sis) = temp_call_paths(d.path(), at(14, 30, 5));
        std::fs::write(&sis, b"x").unwrap();
        assert_eq!(
            pending_recordings(d.path()),
            vec![PendingAudio { started_at: at(14, 30, 5), wav: sis, system: None }]
        );
    }

    #[test]
    fn temp_call_paths_usa_o_mesmo_carimbo() {
        assert_eq!(
            temp_call_paths(Path::new("/g"), at(14, 30, 5)),
            (
                PathBuf::from("/g/gravacao-20261028-143005-mic.wav"),
                PathBuf::from("/g/gravacao-20261028-143005-sistema.wav")
            )
        );
    }

    #[test]
    fn pending_recordings_of_missing_dir_is_empty() {
        let d = tempfile::tempdir().unwrap();
        assert!(pending_recordings(&d.path().join("nao-existe")).is_empty());
    }

    #[test]
    fn wav_duration_reads_header() {
        let p = tmp("dur");
        let mut s = WavSink::create(&p, 16_000).unwrap();
        s.push(&vec![0.0; 24_000]).unwrap();
        s.finish().unwrap();
        assert_eq!(wav_duration_ms(&p), Some(1_500));
        let _ = std::fs::remove_file(&p);
        assert_eq!(wav_duration_ms(&p), None);
    }

    #[test]
    fn failed_start_removes_the_empty_wav() {
        let p = tmp("start-fail");
        std::fs::write(&p, b"").unwrap();
        let r: anyhow::Result<()> = remove_on_err(Err(anyhow!("o microfone não respondeu")), &p);
        assert!(r.is_err());
        assert!(!p.exists());
        let q = tmp("start-ok");
        std::fs::write(&q, b"").unwrap();
        assert!(remove_on_err(Ok(()), &q).is_ok());
        assert!(q.exists());
        let _ = std::fs::remove_file(&q);
    }

    #[test]
    fn disk_error_is_not_reported_as_mic() {
        // /dev/full aceita abrir, mas toda escrita falha com "sem espaço".
        let sink = WavSink::create(Path::new("/dev/full"), 1_000).unwrap();
        let failure: Arc<Mutex<Option<Failure>>> = Arc::default();
        let (tx, rx) = mpsc::channel();
        tx.send(Msg::Data(vec![0.1; 10_000])).unwrap();
        tx.send(Msg::Stop).unwrap();
        let _ = writer_loop(rx, sink, failure.clone());
        assert!(matches!(lock(&failure).clone(), Some(Failure::Disk(_))), "{:?}", lock(&failure));
    }

    #[test]
    fn failure_messages() {
        assert_eq!(
            Failure::Mic("x".into()).note(),
            "O microfone desconectou; processando o que foi gravado"
        );
        assert_eq!(
            Failure::Disk("x".into()).note(),
            "Erro ao gravar no disco; processando o que foi gravado"
        );
    }

    #[test]
    fn default_name_needs_a_default_id() {
        assert!(!is_default_entry::<&str>(None, None));
        assert!(is_default_entry(Some("a"), Some("a")));
        assert!(!is_default_entry(Some("a"), Some("b")));
        assert!(!is_default_entry(Some("a"), None));
    }

    #[test]
    #[ignore = "precisa de microfone"]
    fn records_three_seconds() {
        let p = tmp("mic3s");
        let r = Recorder::start("", p.clone()).unwrap();
        std::thread::sleep(Duration::from_secs(3));
        assert!(r.failed().is_none(), "{:?}", r.failed());
        let path = r.stop().unwrap();
        let rd = hound::WavReader::open(&path).unwrap();
        let secs = rd.duration() as f32 / rd.spec().sample_rate as f32;
        let _ = std::fs::remove_file(&path);
        assert!((2.8..=3.3).contains(&secs), "duração {secs}");
    }
}
