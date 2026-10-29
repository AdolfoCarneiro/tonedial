use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::os::unix::process::CommandExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Instant;

use anyhow::{Context, anyhow};
use chrono::{Local, NaiveDateTime};

use super::record::{Levels, Meter, WavSink, lock};

const RATE: u32 = 48_000;

/// `pw-record` capturando o que toca nas caixas (raw f32le no stdout, mono, 48 kHz).
pub fn pw_record_command() -> Command {
    let mut c = Command::new("pw-record");
    c.arg("-P")
        .arg(r#"{ stream.capture.sink=true node.name=tonedial.sistema application.name="Tone Dial" }"#)
        .args(["--rate", "48000", "--channels", "1", "--format", "f32", "--raw", "-"]);
    c
}

#[derive(Default)]
struct Shared {
    stop: AtomicBool,
    /// Grupo do processo atual, para matar também os filhos dele.
    pgid: Mutex<Option<i32>>,
    stopped_at: Mutex<Option<NaiveDateTime>>,
}

impl Shared {
    fn kill(&self) {
        if let Some(pg) = *lock(&self.pgid) {
            // SAFETY: só envia um sinal ao grupo que nós mesmos criamos.
            unsafe { libc::kill(-pg, libc::SIGKILL) };
        }
    }
}

/// Grava o som do computador num WAV mono f32 48 kHz, lendo o stdout de um processo.
pub struct SystemRecorder {
    pub levels: Levels,
    pub wav_path: PathBuf,
    shared: Arc<Shared>,
    thread: Option<JoinHandle<anyhow::Result<()>>>,
}

fn spawn(make: &impl Fn() -> Command, shared: &Shared) -> anyhow::Result<(Child, std::process::ChildStdout)> {
    let mut cmd = make();
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).process_group(0);
    let mut child = cmd.spawn().context("não foi possível iniciar o pw-record")?;
    let stdout = child.stdout.take().ok_or_else(|| anyhow!("pw-record sem saída"))?;
    let mut pg = lock(&shared.pgid);
    *pg = Some(child.id() as i32);
    if shared.stop.load(Ordering::Acquire) {
        // stop() chegou antes de o pid ser registrado: ele não teria como matar este processo
        drop(pg);
        shared.kill();
    }
    Ok((child, stdout))
}

/// Silêncio para o WAV ficar no mesmo relógio do microfone: o que falta entre
/// o que já foi escrito e o tempo decorrido desde `reference`.
fn silence_gap(written: u64, reference: Instant) -> usize {
    let target = (reference.elapsed().as_secs_f64() * f64::from(RATE)) as u64;
    target.saturating_sub(written) as usize
}

fn pump(
    mut stdout: std::process::ChildStdout,
    sink: &mut WavSink,
    meter: &mut Meter,
    written: &mut u64,
    reference: Instant,
) -> anyhow::Result<()> {
    let mut buf = [0u8; 4096 * 4];
    let mut carry: Vec<u8> = Vec::new();
    let mut first = true;
    loop {
        let n = match stdout.read(&mut buf) {
            Ok(0) | Err(_) => return Ok(()),
            Ok(n) => n,
        };
        carry.extend_from_slice(&buf[..n]);
        let whole = carry.len() / 4 * 4;
        let samples: Vec<f32> = carry[..whole]
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect();
        carry.drain(..whole);
        if first {
            // atraso até o primeiro bloco (e, depois de reabrir, o buraco): vira silêncio
            let gap = silence_gap(*written, reference);
            if gap > 0 {
                let zeros = vec![0.0f32; gap];
                sink.push(&zeros)?;
                meter.feed(&zeros);
                *written += gap as u64;
            }
            first = false;
        }
        sink.push(&samples)?;
        meter.feed(&samples);
        *written += samples.len() as u64;
    }
}

fn read_loop(
    make: impl Fn() -> Command,
    first: (Child, std::process::ChildStdout),
    mut sink: WavSink,
    mut meter: Meter,
    reference: Instant,
    shared: Arc<Shared>,
) -> anyhow::Result<()> {
    let mut written = 0u64;
    let mut current = Some(first);
    let mut retried = false;
    let result = loop {
        let Some((mut child, stdout)) = current.take() else { break Ok(()) };
        let r = pump(stdout, &mut sink, &mut meter, &mut written, reference);
        // tira o grupo antes do wait: depois de colhido, o pgid pode ser reusado por outro
        // processo e o stop() mataria o grupo errado. Ainda não colhido, o grupo é nosso:
        // mata o que sobrou dele para o wait não ficar preso.
        if let Some(pg) = lock(&shared.pgid).take() {
            // SAFETY: o líder do grupo ainda não foi colhido, então o grupo é o que criamos.
            unsafe { libc::kill(-pg, libc::SIGKILL) };
        }
        let _ = child.wait();
        if let Err(e) = r {
            break Err(e);
        }
        if shared.stop.load(Ordering::Acquire) {
            break Ok(());
        }
        // o processo terminou sozinho: uma tentativa de reabrir
        if !retried && let Ok(c) = spawn(&make, &shared) {
            retried = true;
            current = Some(c);
            continue;
        }
        *lock(&shared.stopped_at) = Some(Local::now().naive_local());
        break Ok(());
    };
    let finished = sink.finish();
    result.and(finished)
}

impl SystemRecorder {
    pub fn start(wav_path: PathBuf, reference: Instant) -> anyhow::Result<SystemRecorder> {
        Self::start_with(pw_record_command, wav_path, reference)
    }

    pub(crate) fn start_with(
        make: impl Fn() -> Command + Send + 'static,
        wav_path: PathBuf,
        reference: Instant,
    ) -> anyhow::Result<SystemRecorder> {
        let shared = Arc::new(Shared::default());
        let first = spawn(&make, &shared)?;
        let sink = match WavSink::create(&wav_path, RATE) {
            Ok(s) => s,
            Err(e) => {
                shared.kill();
                let mut first = first;
                let _ = first.0.wait();
                return Err(e);
            }
        };
        let levels = Levels::default();
        let meter = Meter::new(levels.clone(), RATE);
        let sh = shared.clone();
        let thread = std::thread::spawn(move || read_loop(make, first, sink, meter, reference, sh));
        Ok(SystemRecorder { levels, wav_path, shared, thread: Some(thread) })
    }

    /// Quando o som parou de vez (o processo morreu e a única nova tentativa também falhou).
    pub fn stopped_at(&self) -> Option<NaiveDateTime> {
        *lock(&self.shared.stopped_at)
    }

    fn shutdown(&mut self) -> anyhow::Result<()> {
        self.shared.stop.store(true, Ordering::Release);
        self.shared.kill();
        match self.thread.take() {
            Some(h) => h.join().map_err(|_| anyhow!("thread do som do computador falhou"))?,
            None => Ok(()),
        }
    }

    /// Mata o `pw-record`, finaliza o WAV e devolve o caminho.
    pub fn stop(mut self) -> anyhow::Result<PathBuf> {
        self.shutdown()?;
        Ok(self.wav_path.clone())
    }
}

impl Drop for SystemRecorder {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::time::{Duration, Instant};

    fn tmp(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("td-sys-{}-{}.wav", name, std::process::id()))
    }

    fn sh(script: &'static str) -> impl Fn() -> Command + Send + 'static {
        move || {
            let mut c = Command::new("sh");
            c.arg("-c").arg(script);
            c
        }
    }

    #[test]
    fn system_recorder_grava_o_stdout_em_wav_48k() {
        let p = tmp("stdout");
        // 48000 amostras f32 = 1 s
        let r = SystemRecorder::start_with(sh("head -c 192000 /dev/zero; sleep 5"), p.clone(), Instant::now()).unwrap();
        std::thread::sleep(Duration::from_millis(1200));
        let out = r.stop().unwrap();
        let rd = hound::WavReader::open(&out).unwrap();
        assert_eq!(rd.spec().sample_rate, 48_000);
        assert_eq!(rd.spec().channels, 1);
        assert!(rd.len() >= 48_000, "{}", rd.len());
        let _ = std::fs::remove_file(&out);
    }

    #[test]
    fn system_recorder_preenche_o_atraso_com_silencio() {
        let p = tmp("atraso");
        let reference = Instant::now() - Duration::from_millis(500);
        let r = SystemRecorder::start_with(sh("head -c 192000 /dev/zero; sleep 5"), p, reference).unwrap();
        std::thread::sleep(Duration::from_millis(800));
        let out = r.stop().unwrap();
        let rd = hound::WavReader::open(&out).unwrap();
        assert!(rd.len() >= 24_000 + 48_000, "{}", rd.len());
        let _ = std::fs::remove_file(&out);
    }

    #[test]
    fn system_recorder_falha_ao_abrir_com_binario_inexistente() {
        let p = tmp("inexistente");
        let r = SystemRecorder::start_with(|| Command::new("/nao/existe/pw-record"), p.clone(), Instant::now());
        assert!(r.is_err());
        assert!(!p.exists());
    }

    #[test]
    fn system_recorder_esquece_o_grupo_depois_que_o_processo_morre() {
        let p = tmp("grupo");
        let r = SystemRecorder::start_with(sh("head -c 4800 /dev/zero"), p, Instant::now()).unwrap();
        std::thread::sleep(Duration::from_millis(1200));
        assert!(r.stopped_at().is_some());
        assert_eq!(*lock(&r.shared.pgid), None);
        let out = r.stop().unwrap();
        let _ = std::fs::remove_file(&out);
    }

    #[test]
    fn system_recorder_marca_quando_o_processo_morre_de_vez() {
        let p = tmp("morre");
        let r = SystemRecorder::start_with(sh("head -c 4800 /dev/zero"), p, Instant::now()).unwrap();
        std::thread::sleep(Duration::from_millis(1200));
        assert!(r.stopped_at().is_some());
        let out = r.stop().unwrap();
        assert!(hound::WavReader::open(&out).is_ok());
        let _ = std::fs::remove_file(&out);
    }
}
