# Gravar calls: plano de implementação

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** O Tone Dial passa a ficar na bandeja, percebe pelo PipeWire quando outro programa usa o microfone, oferece gravar a call (mic + som do computador) e transcreve marcando o usuário como "Eu" e os outros como Falante N.

**Architecture:** O estado do app (`ui::App`) passa a viver fora da janela: um `tick()` faz todo o trabalho não visual e é chamado tanto pelo `update` do eframe quanto por um laço sem janela em `main.rs`, que reabre a janela com `run_native` quando pedido (validado no spike). Threads auxiliares (bandeja, instância única, observador do PipeWire, notificações) falam com o `App` por um único canal `AppEvent`. A gravação de call junta o `Recorder` de hoje (mic, cpal) a um `SystemRecorder` (processo `pw-record` capturando a saída). O pipeline ganha a fonte `Source::Call`, que transcreve os dois lados, separa falantes só no lado do sistema e junta tudo com "Eu".

**Tech Stack:** Rust 2024, eframe/egui 0.37, cpal 0.18, whisper-rs, speakrs, `ksni` 0.3.7 (feature `blocking`), `notify-rust` 4.18.2, `pw-dump`/`pw-record` (PipeWire 1.6), ffmpeg.

**Spec:** `docs/superpowers/specs/2026-10-28-gravar-calls-design.md` (leia antes de cada tarefa).

## Global Constraints

- Tudo em pt-BR: textos de interface, mensagens de erro, comentários e mensagens de commit (siga o estilo dos commits existentes: frase curta em português, sem prefixo `feat:`).
- Testes ficam no próprio módulo (`#[cfg(test)] mod tests`), como no resto do projeto. Rode com `cargo test <nome>`.
- Comentários e nomes no estilo do código vizinho: poucos comentários, explicando o porquê.
- Nada começa a gravar sem um clique do usuário (notificação, bandeja ou janela).
- Uma gravação de call nunca para sozinha por causa da detecção: só pergunta.
- Tempos do detector: notificar após **3 s** seguidos de mic em uso; considerar solto após **5 s** seguidos sem uso; repetir a pergunta de fim após **5 min**; ler o `pw-dump` a cada **~2 s**.
- Arquivos de call: `gravacao-AAAAMMDD-HHMMSS-mic.wav` e `gravacao-AAAAMMDD-HHMMSS-sistema.wav` em `recordings_dir()`.
- `.ogg` de call: estéreo, esquerdo = mic, direito = sistema, Opus **96 kbps**.
- Rótulo do usuário no `.md`: `**Eu**`. Linha "Áudio" do cabeçalho de call: `<stem>.ogg (call: mic + som do computador)`.
- Janela de remoção de eco: palavra do mic descartada se uma palavra do sistema com o mesmo `normalize()` começa a até **1000 ms**.
- Socket de instância única: `$XDG_RUNTIME_DIR/tonedial.sock`. Autostart: `~/.config/autostart/tonedial.desktop` com `Exec=<exe> --background`, criado só se não existir.
- Commits terminam com `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.

## Review Focus

1. **O Tone Dial disparando a própria notificação.** O `Monitor` (ondinhas) e o `Recorder` abrem o mic pelo cpal, com o pid do próprio processo; o `pw-record` do sistema tem `stream.capture.sink=true`. Nenhum deles pode contar como "call". Fixado no teste `ignora_streams_do_proprio_app` (Tarefa 4).
2. **`pw-record` ausente ou morrendo no meio.** A gravação de call precisa virar só mic com aviso, nunca falhar inteira. Fixado em `system_recorder_falha_ao_abrir_com_binario_inexistente` (Tarefa 3) e `call_sem_sistema_grava_so_mic_com_aviso` (Tarefa 6).
3. **Processamento termina com a janela fechada.** O progresso precisa ser lido sem janela e o resultado virar notificação. Fixado em `tick_sem_janela_processa_progresso_e_avisa` (Tarefa 5/7).
4. **"Parar" atrasado depois que a call voltou.** Um clique velho na notificação de fim não pode parar uma call em andamento. Fixado em `resposta_de_fim_antiga_e_ignorada` (Tarefa 7).
5. **Sair pela bandeja durante uma gravação de call.** Os dois WAVs precisam ficar finalizados e aparecer como uma pendente só na próxima abertura. Fixado em `pendentes_agrupam_par_de_call` (Tarefa 3) e `sair_gravando_deixa_par_pendente` (Tarefa 6).

---

### Tarefa 1: Junção "Eu" + Falantes no texto

**Files:**
- Modify: `src/transcript.rs`

**Interfaces:**
- Consumes: `Word`, `SpeakerSegment`, `Utterance`, `align`, `normalize`, `render_markdown` existentes.
- Produces:
  - `pub const ME: u32 = 0;` (o `align` de hoje numera a partir de 1, então 0 fica livre para "Eu").
  - `pub fn drop_echo(mic: Vec<Word>, system: &[Word]) -> Vec<Word>`
  - `pub fn align_call(mic: &[Word], system: &[Word], segments: &[SpeakerSegment]) -> Vec<Utterance>`
  - `render_markdown` passa a escrever `**Eu**` quando `speaker == ME`.

- [ ] **Step 1: Testes que falham**

```rust
#[test]
fn drop_echo_remove_palavra_do_mic_repetida_no_sistema() {
    let mic = vec![word(1000, 1300, "Bom"), word(5000, 5200, "certo")];
    let sys = [word(1600, 1900, "bom,")];
    assert_eq!(drop_echo(mic, &sys), vec![word(5000, 5200, "certo")]);
}

#[test]
fn drop_echo_mantem_palavra_igual_longe_no_tempo() {
    let mic = vec![word(3000, 3200, "sim")];
    let sys = [word(1000, 1200, "sim")];
    assert_eq!(drop_echo(mic.clone(), &sys), mic);
}

#[test]
fn align_call_intercala_eu_e_falantes_por_tempo() {
    let mic = [word(0, 400, "oi"), word(500, 900, "gente")];
    let sys = [word(1000, 1400, "olá"), word(3000, 3400, "tchau")];
    let segs = [seg(900, 2000, 5), seg(2900, 3500, 2)];
    let u = align_call(&mic, &sys, &segs);
    assert_eq!(u.iter().map(|x| (x.speaker, x.text.as_str())).collect::<Vec<_>>(),
        vec![(ME, "oi gente"), (1, "olá"), (2, "tchau")]);
}

#[test]
fn align_call_sem_sistema_fica_so_eu() {
    let u = align_call(&[word(0, 100, "a")], &[], &[]);
    assert_eq!(u.len(), 1);
    assert_eq!(u[0].speaker, ME);
}

#[test]
fn markdown_escreve_eu() {
    let h = Header { recorded_at: /* data fixa do teste existente */, duration_ms: 60_000, source_name: "x.ogg".into() };
    let md = render_markdown(&h, &[Utterance { speaker: ME, start_ms: 3000, text: "bom dia".into() }]);
    assert!(md.contains("**Eu** [00:00:03]: bom dia"), "{md}");
}
```

- [ ] **Step 2: Rodar e ver falhar** — `cargo test --lib transcript` → erro de compilação (`ME`, `drop_echo`, `align_call` não existem).

- [ ] **Step 3: Implementar.** `drop_echo`: descarta a palavra do mic se existe palavra do sistema com `normalize` igual (e não vazio) e `|start_mic - start_sys| <= 1000`. `align_call`: os falantes do sistema vêm do mesmo critério de `align` (reaproveite `pick_segment` e a renumeração por primeira aparição, começando em 1). As palavras do mic recebem `ME`. Junta tudo ordenado por `start_ms` (empate: mic primeiro) e agrupa palavras seguidas do mesmo falante numa `Utterance`, com o mesmo `split_whitespace().join(" ")` de hoje. Sem segmentos (separação falhou), as palavras do sistema viram falante 1 (é o que `pick_segment` já faz ao devolver 0 e renumerar).

- [ ] **Step 4: Rodar e ver passar** — `cargo test --lib transcript` → todos passam, incluindo os antigos.

- [ ] **Step 5: Commit** — `git commit -am "Texto de call: Eu + Falantes, sem eco do sistema no mic"`

---

### Tarefa 2: Pipeline processa uma call

**Files:**
- Modify: `src/audio/decode.rs`, `src/pipeline.rs`, `src/bin/tonedial-cli.rs` (só se quebrar a compilação)

**Interfaces:**
- Consumes: `drop_echo`, `align_call`, `ME` (Tarefa 1).
- Produces:
  - `Source::Call { mic: PathBuf, system: PathBuf, started_at: NaiveDateTime }`
  - `pub fn encode_ogg_stereo(left: &Path, right: &Path, out: &Path) -> Result<()>` em `decode.rs` (mesmo esquema `.part` + rename do `encode_ogg`; ffmpeg com dois `-i` e `-filter_complex "[0:a][1:a]amerge=inputs=2[a]" -map "[a]" -ac 2 -c:a libopus -b:a 96k`; os dois WAVs podem ter taxas iguais (48 kHz), não precisa reamostrar).
  - `Transcriber` passa de `FnOnce` para `FnMut` (o padrão guarda o `LoadedModel` num `Option` capturado e carrega uma vez só). `execute` faz `drop(transcribe)` antes de diarizar, para soltar a VRAM.

- [ ] **Step 1: Testes que falham** (em `pipeline::tests`, usando o `wav_fixture` existente para os dois lados e um transcritor falso que devolve palavras diferentes por chamada)

```rust
#[test]
fn call_gera_md_com_eu_e_falante_e_ogg_estereo() {
    // transcritor: 1ª chamada (mic) -> [0..400 "oi"], 2ª (sistema) -> [1000..1400 "olá"]
    // diarizador: segmento único 0..2000 falante 7
    // espera: Done com speakers == 2; md contém "**Eu** [00:00:00]: oi" e "**Falante 1** [00:00:01]: olá"
    //         e "(call: mic + som do computador)"; existe <stem>.ogg com 2 canais (decode::probe ou ffprobe)
    //         e os dois WAVs foram apagados
}

#[test]
fn call_diariza_so_o_sistema() {
    // diarizador falso que guarda o len() das amostras recebidas; os dois WAVs com durações diferentes
    // (1 s mic, 2 s sistema): o diarizador recebe só 32_000 amostras (2 s a 16 kHz)
}

#[test]
fn call_com_falha_antes_do_ogg_diz_onde_estao_os_wavs() {
    // transcritor que falha; mensagem Failed contém o caminho do WAV do mic e do sistema
}
```

- [ ] **Step 2: Rodar e ver falhar** — `cargo test --lib pipeline` → não compila (`Source::Call`).

- [ ] **Step 3: Implementar.** Em `execute`, a fonte de call: `recorded_at = started_at`; `stem` como hoje (`source` = mic); decodifica os dois para 16 kHz mono; grava o `.ogg` estéreo e apaga os dois WAVs se ele existir; transcreve mic e depois sistema (`Stage::Transcribing` antes de cada um, a barra recomeça); `drop(transcribe)`; diariza só o sistema (mesma proteção contra erro e panic de hoje, pulando se o sistema não tem palavras); `align_call(&drop_echo(mic_words, &sys_words), &sys_words, &segments)`; `speakers` conta os rótulos distintos (inclui `ME`). `duration_ms` = maior das duas durações. Em `run_inner`, a mensagem de erro de uma call cita o `.ogg` se já existe, senão os dois WAVs. `Source::Recording` e `Source::File` continuam iguais.

- [ ] **Step 4: Rodar e ver passar** — `cargo test --lib pipeline` e `cargo build --bins` → passam.

- [ ] **Step 5: Commit** — `"Pipeline: fonte Call com mic e sistema, .ogg estéreo"`

---

### Tarefa 3: Gravar o som do computador e pendentes em par

**Files:**
- Create: `src/audio/system.rs`, `src/audio/call.rs`
- Modify: `src/audio/mod.rs`, `src/audio/record.rs`

**Interfaces:**
- Consumes: `WavSink`, `Levels`, `Meter`, `Recorder`, `Failure`, `recordings_dir` (todos em `record.rs`; torne `pub(crate)` o que precisar).
- Produces:
  - `record.rs`: `pub fn temp_call_paths(dir: &Path, at: NaiveDateTime) -> (PathBuf, PathBuf)` (mic, sistema); `pub struct PendingAudio { pub started_at: NaiveDateTime, pub wav: PathBuf, pub system: Option<PathBuf> }`; `pending_recordings(dir) -> Vec<PendingAudio>` (substitui o retorno atual). Par completo → `wav` = mic, `system` = Some. Sozinho (gravação simples, ou só um lado do par) → `system` = None. Ordenado por `started_at`.
  - `system.rs`: `pub struct SystemRecorder { pub levels: Levels, pub wav_path: PathBuf, .. }` com `pub fn start(wav_path: PathBuf, reference: Instant) -> anyhow::Result<SystemRecorder>`, `pub(crate) fn start_with(cmd: impl Fn() -> std::process::Command + Send + 'static, wav_path, reference) -> ..`, `pub fn stopped_at(&self) -> Option<NaiveDateTime>` (quando o som parou de vez, depois da única tentativa de reabrir), `pub fn stop(self) -> anyhow::Result<PathBuf>`. `pub fn pw_record_command() -> Command`: `pw-record -P '{ stream.capture.sink=true node.name=tonedial.sistema application.name="Tone Dial" }' --rate 48000 --channels 1 --format f32 --raw -` (raw f32le no stdout, como testado no spike). O WAV é mono f32 48 kHz; antes do primeiro bloco, escreve `(primeiro_bloco - reference)` em silêncio (sincronia com o mic). Uma thread lê o stdout em blocos e alimenta `WavSink` e `Meter`. Se o processo terminar sozinho, tenta reabrir uma vez (preenchendo com silêncio o buraco). Se falhar de novo, guarda `stopped_at` e o WAV continua válido.
  - `call.rs`: `pub struct CallRecorder { pub mic: Recorder, pub system: Option<SystemRecorder>, pub system_note: Option<String> }` com `pub fn start(device: &str, dir: &Path, at: NaiveDateTime) -> anyhow::Result<CallRecorder>` (abre o mic primeiro; usa o instante de início dele como `reference`; se o `SystemRecorder` falhar ao abrir, `system = None` e `system_note = Some("Não consegui gravar o som do computador; gravando só o microfone")`), `pub fn note(&self) -> Option<String>` (o `system_note`, ou "O som do computador parou de ser gravado às HH:MM" a partir de `stopped_at`) e `pub fn stop(self) -> anyhow::Result<(PathBuf, Option<PathBuf>)>`.

- [ ] **Step 1: Testes que falham**

```rust
// record.rs
#[test]
fn pendentes_agrupam_par_de_call() {
    // dir com gravacao-20261028-143005-mic.wav, gravacao-20261028-143005-sistema.wav e gravacao-20261028-150000.wav
    // -> 2 itens: [ {wav: ..-mic.wav, system: Some(..-sistema.wav)}, {wav: ..150000.wav, system: None} ]
}
#[test]
fn par_incompleto_vira_gravacao_simples() {
    // só gravacao-20261028-143005-sistema.wav -> 1 item com wav = esse arquivo, system = None
}
#[test]
fn temp_call_paths_usa_o_mesmo_carimbo() {
    // -> (dir/gravacao-20261028-143005-mic.wav, dir/gravacao-20261028-143005-sistema.wav)
}

// system.rs
#[test]
fn system_recorder_grava_o_stdout_em_wav_48k() {
    // start_with(|| sh -c "head -c 192000 /dev/zero; sleep 5") (48000 amostras f32 = 1 s); espera ~1 s; stop()
    // -> WAV mono 48 kHz com >= 48000 amostras
}
#[test]
fn system_recorder_preenche_o_atraso_com_silencio() {
    // reference = Instant::now() - 500 ms; o WAV tem >= 24000 amostras de silêncio antes dos dados
}
#[test]
fn system_recorder_falha_ao_abrir_com_binario_inexistente() {
    // start_with(|| Command::new("/nao/existe/pw-record"), ..) -> Err
}
#[test]
fn system_recorder_marca_quando_o_processo_morre_de_vez() {
    // comando que sai logo (sh -c "head -c 4800 /dev/zero"): depois de ~1 s, stopped_at().is_some() e stop() devolve WAV legível
}
```

- [ ] **Step 2: Rodar e ver falhar** — `cargo test --lib audio` → não compila.

- [ ] **Step 3: Implementar** conforme Interfaces. `CallRecorder` não tem teste próprio de hardware; o comportamento sem sistema é coberto na Tarefa 6. Ajuste os chamadores de `pending_recordings` em `ui/mod.rs` só o bastante para compilar (`Pending` ganha `system: Option<PathBuf>`); o uso completo vem na Tarefa 6.

- [ ] **Step 4: Rodar e ver passar** — `cargo test` → tudo verde.

- [ ] **Step 5: Commit** — `"Gravação do som do computador (pw-record) e pendentes em par"`

---

### Tarefa 4: Detector de call (PipeWire)

**Files:**
- Create: `src/call/mod.rs`, `src/call/pipewire.rs`, `src/call/detector.rs`, `src/call/testdata/dump.json`
- Modify: `src/lib.rs` (`pub mod call;`)

**Interfaces:**
- Produces:
  - `pipewire.rs`: `pub struct MicUser { pub node_id: u32, pub app: Option<String>, pub pid: Option<u32> }`; `pub fn mic_users(dump_json: &str, own_pid: u32) -> anyhow::Result<Vec<MicUser>>` (regras da seção 2 do spec; pid e `application.name` vêm do objeto `PipeWire:Interface:Client` do `client.id`, usando `application.process.id` ou `pipewire.sec.pid`); `pub fn spawn_watcher(own_pid: u32, on_sample: impl Fn(bool, Option<String>) + Send + 'static)`: a cada 2 s roda `pw-dump`, chama `on_sample(em_uso, app_do_primeiro)`. Se o `pw-dump` falhar, escreve uma linha no stderr e pula o ciclo.
  - `detector.rs`: 
    ```rust
    pub struct DetectorCtx { pub busy: bool, pub call_recording: bool }
    #[derive(Debug, PartialEq)]
    pub enum DetectorAction { AskStart { app: Option<String> }, AskEnd, CallBack }
    pub struct Detector { .. }
    impl Detector {
        pub fn new() -> Self;
        pub fn observe(&mut self, now: Instant, in_use: bool, app: Option<&str>, ctx: DetectorCtx) -> Option<DetectorAction>;
    }
    ```
    `busy` = gravando qualquer coisa ou processando ou preparando modelos. Regras: `AskStart` quando o uso está contínuo há ≥ 3 s, `!busy`, e ainda não perguntou nesta sessão de uso (sessão termina quando fica solto ≥ 5 s). `AskEnd` só com `call_recording`, depois de o mic ter sido visto em uso **durante esta gravação** (o detector percebe o início da gravação pela transição de `call_recording`) e solto ≥ 5 s. Se continuar solto, novo `AskEnd` a cada 5 min. `CallBack` quando, depois de um `AskEnd`, o mic volta a ser usado.

- [ ] **Step 1: Montar `testdata/dump.json`** — a partir de um `pw-dump` real desta máquina (`pw-dump > ...` com um `pw-record` rodando, como no spike), reduzido aos objetos necessários, com: o `mic_limpo.capture` (passivo), o nó `mic_limpo` (Audio/Source), um stream de "Opera" (client pid 4242) ligado ao `mic_limpo`, um stream com `stream.capture.sink=true`, um stream do próprio pid (client pid 1000) ligado ao `mic_limpo` e um stream de "Firefox" sem link.

- [ ] **Step 2: Testes que falham**

```rust
// pipewire.rs
#[test]
fn acha_o_navegador_usando_o_mic() {
    let u = mic_users(DUMP, 1000).unwrap();
    assert_eq!(u.len(), 1);
    assert_eq!(u[0].app.as_deref(), Some("Opera"));
    assert_eq!(u[0].pid, Some(4242));
}
#[test]
fn ignora_streams_do_proprio_app() {
    // com own_pid = 4242 o Opera some; o stream do pid 1000 aparece (ele é "outro app" nesse caso)
}
#[test]
fn json_invalido_e_erro() { assert!(mic_users("{", 1).is_err()); }

// detector.rs (relógio: t0 = Instant::now(), t(s) = t0 + Duration::from_secs(s))
#[test] fn pergunta_depois_de_3s_de_uso()          // observe em 0,1,2 -> None; em 3 -> AskStart{app: Some("Opera")}
#[test] fn pergunta_uma_vez_por_sessao_de_uso()     // depois do AskStart, uso contínuo até 60 s -> None
#[test] fn nova_sessao_depois_de_5s_solto()         // solto de 10 a 15 s, uso de novo de 16 a 19 -> AskStart
#[test] fn solto_menos_de_5s_nao_e_nova_sessao()    // solto 10..13, uso de novo -> None
#[test] fn nao_pergunta_ocupado()                    // busy = true durante tudo -> None
#[test] fn pergunta_fim_so_depois_de_ver_o_mic_na_gravacao()
    // call_recording desde 0, mic nunca em uso -> None até 600 s
#[test] fn pergunta_fim_5s_depois_de_soltar_e_repete_em_5min()
    // gravação com mic em uso 0..30, solto a partir de 30: AskEnd em 35; None até 334; AskEnd em 335
#[test] fn call_volta_depois_da_pergunta()
    // após AskEnd, observe com in_use=true -> CallBack
#[test] fn gravacao_nova_zera_o_visto()
    // gravação 1 vê o mic; call_recording false; gravação 2 sem mic -> nunca AskEnd
```

- [ ] **Step 3: Rodar e ver falhar** — `cargo test --lib call` → não compila.

- [ ] **Step 4: Implementar** conforme Interfaces (`serde_json::Value` basta para o parse).

- [ ] **Step 5: Rodar e ver passar** — `cargo test --lib call`.

- [ ] **Step 6: Commit** — `"Detector de call: quem usa o mic no PipeWire e quando perguntar"`

---

### Tarefa 5: App vive sem janela (laço em segundo plano, instância única, autostart)

**Files:**
- Create: `src/events.rs`, `src/instance.rs`, `src/autostart.rs`
- Modify: `src/lib.rs`, `src/main.rs`, `src/ui/mod.rs`

**Interfaces:**
- Produces:
  - `events.rs`:
    ```rust
    #[derive(Debug, Clone, PartialEq)]
    pub enum AppEvent {
        ShowWindow,
        Quit,
        /// Gravar call (bandeja ou notificação).
        RecordCall,
        /// Parar a gravação atual (bandeja).
        StopRecording,
        /// Amostra do observador do PipeWire.
        MicUsage { in_use: bool, app: Option<String> },
        /// Resposta à pergunta de fim; `generation` identifica a pergunta.
        EndAnswer { generation: u64, stop: bool },
    }
    ```
  - `instance.rs`: `pub fn socket_path() -> PathBuf`; `pub enum Instance { Primary(UnixListener), Secondary }`; `pub fn acquire(path: &Path) -> std::io::Result<Instance>` (bind; se o endereço está em uso e um `connect` funciona, escreve `show\n` e devolve `Secondary`; se o `connect` falha, o socket é velho: apaga e faz bind de novo); `pub fn serve(listener: UnixListener, on_show: impl Fn() + Send + 'static)` (thread: cada conexão chama `on_show`).
  - `autostart.rs`: `pub fn ensure(autostart_dir: &Path, exe: &Path) -> std::io::Result<bool>` (cria `tonedial.desktop` só se não existir; `true` se criou). Conteúdo: o `assets/tonedial.desktop` com `Exec="<exe>" --background` e a linha `X-GNOME-Autostart-enabled=true`. `pub fn default_dir() -> Option<PathBuf>` (`dirs::config_dir()/autostart`).
  - `ui::App`: `pub fn new(events: (Sender<AppEvent>, Receiver<AppEvent>)) -> Self`; `pub fn events(&self) -> Sender<AppEvent>`; `pub fn tick(&mut self)` (todo o trabalho não visual que hoje está no começo do `ui()`: `poll_setup`, `poll_progress`, falha do mic, `sync_sleep_guard`, mais drenar o canal de eventos); `pub fn window_opened(&mut self)` (liga o `Monitor` se estiver no Idle); `pub fn window_closed(&mut self)` (desliga o `Monitor`); `pub fn take_show_request(&mut self) -> bool`; `pub fn should_quit(&self) -> bool`. `go_idle` só liga o `Monitor` com a janela aberta.
  - `ui::Shell(pub Rc<RefCell<App>>)` implementa `eframe::App` (chama `tick()` e desenha). Com a janela aberta, `ui()` pede repaint a cada ≤ 250 ms também no Done/Error, para drenar eventos.
- Comportamento novo do fechar: o X da janela **sempre** fecha a janela (sem modal) e o app continua. O modal "Fechar?" vira "Sair?" e só aparece para `AppEvent::Quit` com gravação ou processamento em andamento (abre a janela e mostra o modal). Texto: gravando → "Parar a gravação e sair? O áudio fica salvo e você pode processar depois."; processando → "Sair agora? O processamento será perdido, mas o áudio continua salvo." `Quit` sem nada em andamento sai direto.
- `main.rs`: `--background` não abre a janela. Fluxo: `instance::acquire` (Secondary → sai com 0); `autostart::ensure` (erro só vai pro stderr); `App::new`; `instance::serve` mandando `ShowWindow`; laço: se é para mostrar, `window_opened`, `run_native` com `run_and_return: true` e `Shell`, `window_closed`; se `should_quit`, sai; senão, laço sem janela chamando `tick()` a cada 200 ms até `take_show_request()` ou `should_quit()`.

- [ ] **Step 1: Testes que falham**

```rust
// instance.rs
#[test] fn primeira_instancia_e_primaria_e_a_segunda_avisa() {
    // tempdir; acquire -> Primary; serve com on_show que incrementa um AtomicUsize; segundo acquire -> Secondary;
    // em até 1 s o contador vira 1
}
#[test] fn socket_velho_e_reaproveitado() {
    // cria arquivo de socket sem ninguém escutando (bind + drop do listener) -> acquire -> Primary
}

// autostart.rs
#[test] fn cria_o_desktop_de_autostart_uma_vez() {
    // ensure(dir, "/home/x/.cargo/bin/tonedial") -> true; arquivo contém `Exec="/home/x/.cargo/bin/tonedial" --background`
    // segunda chamada -> false; conteúdo editado à mão não é sobrescrito
}

// ui/mod.rs
#[test] fn tick_sem_janela_processa_progresso() {
    // App em Processing com proc_rx de um canal de teste; manda Progress::Done; tick() -> Screen::Done
}
#[test] fn show_window_vira_pedido_de_mostrar() {
    // events().send(ShowWindow); tick(); take_show_request() == true e depois false
}
#[test] fn quit_parado_sai_e_quit_gravando_pede_confirmacao() {
    // Idle + Quit -> should_quit(); Processing + Quit -> !should_quit(), take_show_request() e confirm_close
}
```

Ajuste os testes antigos que constroem `App::with_state` para o novo construtor, se mudar.

- [ ] **Step 2: Rodar e ver falhar** — `cargo test --lib` → não compila.

- [ ] **Step 3: Implementar** conforme Interfaces. Atenção: `cpal::Stream` não é `Send`, então o `App` continua na thread principal (é o caso: o laço sem janela também roda nela).

- [ ] **Step 4: Rodar e ver passar** — `cargo test`.

- [ ] **Step 5: Teste manual curto** — `cargo run -- --background &`, depois `cargo run` → a segunda instância sai e a primeira abre a janela; fechar a janela não encerra o processo (`pgrep -f 'target/debug/tonedial'`); `kill` no fim. Anote o resultado no relatório da tarefa.

- [ ] **Step 6: Commit** — `"App vive sem janela: laço em segundo plano, instância única e autostart"`

---

### Tarefa 6: Gravar call pela janela

**Files:**
- Modify: `src/config.rs`, `src/ui/mod.rs`, `src/ui/idle.rs`, `src/ui/recording.rs`

**Interfaces:**
- Consumes: `CallRecorder`, `PendingAudio`, `temp_call_paths` (Tarefa 3), `Source::Call` (Tarefa 2), `AppEvent` (Tarefa 5).
- Produces:
  - `Config` ganha `#[serde(default)] pub record_system: bool`.
  - `Screen::Recording(Rec)`, com `pub enum Rec { Mic(Recorder), Call(CallRecorder) }` e os métodos que a UI usa (`levels`, `elapsed`, `failed`, `started_at`).
  - `App::start_recording(&mut self, call: bool)`: o botão Gravar passa `config.record_system`; o `RecordCall` (Tarefa 7) passa `true`.
  - `App::stop_recording` de call → `Source::Call { mic, system, started_at }`, com o `note()` do `CallRecorder` como aviso. Sem WAV do sistema, cai para `Source::Recording` com o aviso.
  - `Pending` usa `PendingAudio`: **Processar** de um par → `Source::Call`; **Descartar** apaga os dois arquivos.
  - Sair (`close_now`) durante uma call finaliza os dois WAVs.
- UI: abaixo do seletor de mic, `ui.checkbox(&mut ..., "Gravar também o som do computador (call)")` (desabilitado gravando; salva no config). Gravando uma call, duas faixas de ondinhas com rótulos pequenos "Você" e "Call" (reduza a altura de cada uma para caber; `draw_levels` ganha um parâmetro de altura).
- `AppEvent::StopRecording` e `RecordCall` passam a funcionar no `tick()` (o `RecordCall` só se não estiver ocupado; nesse caso é ignorado).

- [ ] **Step 1: Testes que falham** (ui/mod.rs)

```rust
#[test] fn processar_par_pendente_vira_source_call()   // use uma função pura `fn source_for(p: &Pending) -> Source` e teste-a
#[test] fn descartar_par_apaga_os_dois()                // como o teste existente de descarte, com os dois arquivos
#[test] fn call_sem_sistema_grava_so_mic_com_aviso()    // fn pura `fn source_after_call(mic, system: Option<PathBuf>, at, note) -> (Source, Option<String>)`
#[test] fn sair_gravando_deixa_par_pendente()           // dois WAVs de verdade (WavSink) no recordings_dir de teste -> refresh_pending -> 1 Pending com system Some
#[test] fn config_antigo_sem_record_system_carrega()    // config.rs: JSON {"output_dir":null,"last_mic":null} -> record_system == false
```

- [ ] **Step 2: Rodar e ver falhar** — `cargo test --lib` → não compila.

- [ ] **Step 3: Implementar** conforme Interfaces.

- [ ] **Step 4: Rodar e ver passar** — `cargo test`.

- [ ] **Step 5: Teste manual** — `cargo run`, marcar a caixa, gravar ~10 s tocando um áudio qualquer (ex.: `pw-play` de um WAV gerado com `ffmpeg -f lavfi -i sine=440:d=5`) e falando; parar; conferir o `.md` (tem `**Eu**`?) e o `.ogg` estéreo (`ffprobe` mostra 2 canais). Anote no relatório.

- [ ] **Step 6: Commit** — `"Gravar call pela janela: mic + som do computador"`

---

### Tarefa 7: Bandeja, notificações e detector ligados ao app

**Files:**
- Create: `src/tray.rs`, `src/notify.rs`
- Modify: `Cargo.toml` (`ksni = { version = "0.3.7", features = ["blocking"] }`, `notify-rust = "4.18.2"`), `src/lib.rs`, `src/main.rs`, `src/ui/mod.rs`

**Interfaces:**
- Consumes: `AppEvent`, `App::tick` (Tarefa 5), `Detector`, `spawn_watcher` (Tarefa 4), `start_recording(true)` (Tarefa 6).
- Produces:
  - `tray.rs`: `pub struct TrayState { pub recording: bool, pub tooltip: String }`; `pub struct TrayHandle`; `pub fn spawn(tx: Sender<AppEvent>) -> Option<TrayHandle>` (`None` se a bandeja não subir; o app continua); `TrayHandle::set(&self, s: TrayState)` (só atualiza se mudou). Clique no ícone → `ShowWindow`. Menu: "Abrir Tone Dial", "Gravar call" ou "Parar gravação" conforme `recording`, separador, "Sair". Ícone: pixmap ARGB a partir de `assets/icons/tonedial-32.png` e `-64.png` (o crate `image` já está na árvore? se não, decodifique com o `egui`/`eframe::icon_data::from_png_bytes`, que devolve RGBA); gravando, um ponto vermelho no canto inferior direito do pixmap. Tooltip: "Tone Dial", "Gravando call · HH:MM:SS", "Gravando · HH:MM:SS" ou "Transcrevendo…".
  - `notify.rs`: funções puras de texto e funções que mostram:
    - `pub fn start_body(app: Option<&str>) -> String`: `Some("Opera")` → `"O Opera está usando o microfone. Gravar a call?"`; `None` → `"Parece que você está numa call. Gravar?"`.
    - `pub fn ask_start(app: Option<&str>, tx: Sender<AppEvent>)`: título "Tone Dial", botões "Gravar" (→ `RecordCall`) e "Agora não".
    - `pub fn ask_end(generation: u64, tx: Sender<AppEvent>)`: "A call acabou? Parar e processar?", botões "Parar" (→ `EndAnswer{generation, stop: true}`) e "Continuar gravando" (→ `stop: false`).
    - `pub fn done(md: &Path, notices: &[String])`: "Transcrição pronta: <nome>.md" + avisos no corpo; botões "Abrir texto" e "Abrir pasta" (abrem com `open::that_detached` na própria thread).
    - `pub fn failed(msg: &str, tx: Sender<AppEvent>)`: "Não consegui transcrever a gravação", botão "Abrir Tone Dial" (→ `ShowWindow`).
    - Cada uma roda numa thread própria (`wait_for_action` bloqueia). Erros de D-Bus vão para o stderr.
    - `pub trait Notifier { fn ask_start(&self, app: Option<&str>); fn ask_end(&self, generation: u64); fn done(&self, md: &Path, notices: &[String]); fn failed(&self, msg: &str); }` e `pub struct DesktopNotifier { tx: Sender<AppEvent> }`, que implementa chamando as funções acima. O `App` guarda um `Box<dyn Notifier>`. Nos testes, um falso registra as chamadas num `Rc<RefCell<Vec<String>>>`.
  - `App`: guarda `Detector`, `end_generation: u64`, `window_open: bool` e o `Option<TrayHandle>`. No `tick()`: `MicUsage` → `detector.observe(Instant::now(), ..., ctx)`; `AskStart` → `notify::ask_start`; `AskEnd` → incrementa `end_generation` e `notify::ask_end`; `CallBack` → incrementa `end_generation` (invalida respostas antigas). `EndAnswer{generation, stop: true}` só para se `generation == end_generation` e está gravando uma call. Ao chegar `Done`/`Failed` do pipeline com a janela fechada → `notify::done`/`notify::failed` (avisos = `note` e `notice` do `ProcessingState`). Atualiza a bandeja a cada `tick()`.
  - Gravação aceita pela notificação sem modelos baixados: grava normalmente. Ao parar, a gravação fica pendente e a janela abre na tela de preparo (o `go_idle` já faz o Setup se faltam modelos).
  - `main.rs`: `tray::spawn(app.events())`, `pipewire::spawn_watcher(std::process::id(), ...)` mandando `MicUsage`.

- [ ] **Step 1: Testes que falham**

```rust
// notify.rs
#[test] fn texto_da_pergunta_de_inicio()   // os dois casos de start_body acima

// ui/mod.rs (sem D-Bus: injete as notificações por um trait ou por um campo `notifier: Box<dyn Notifier>` com implementação falsa que registra as chamadas)
#[test] fn resposta_de_fim_antiga_e_ignorada()
    // gravando call; end_generation = 2; EndAnswer{generation: 1, stop: true} -> continua gravando;
    // EndAnswer{generation: 2, stop: true} -> Processing
#[test] fn tick_sem_janela_processa_progresso_e_avisa()
    // window_open = false, Processing com note; Progress::Done -> notifier registrou done(md, [note])
#[test] fn com_janela_aberta_nao_notifica_o_fim()
#[test] fn ask_start_vira_notificacao()
    // MicUsage{in_use: true, app: Some("Opera")} em 4 ticks com Instant injetado (ou observe direto via método de teste) -> notifier.ask_start(Some("Opera"))
```

- [ ] **Step 2: Rodar e ver falhar** — `cargo test --lib` → não compila.

- [ ] **Step 3: Implementar** conforme Interfaces. Para o tempo do detector nos testes do `App`, aceite `now: Instant` num método interno (`fn handle_event(&mut self, ev: AppEvent, now: Instant)`), chamado pelo `tick()` com `Instant::now()`.

- [ ] **Step 4: Rodar e ver passar** — `cargo test`.

- [ ] **Step 5: Teste manual** — `cargo run -- --background`: ícone aparece na bandeja; `pw-record /tmp/x.wav` (outro programa usando o mic) por 5 s → notificação "O pw-cat está usando o microfone..." (ou similar). Não clique em nada que dependa do usuário; só confirme que apareceu com `busctl --user` ou pelo log do stderr. Encerre os processos. Anote no relatório o que deu para ver.

- [ ] **Step 6: Commit** — `"Bandeja, notificações e detecção de call ligadas ao app"`

---

### Tarefa 8: Documentação

**Files:**
- Modify: `README.md`, `AGENTS.md`

- [ ] **Step 1:** README: nova seção "Calls" (bandeja; abre com o sistema e como desligar: apagar `~/.config/autostart/tonedial.desktop`; a notificação quando outro programa usa o mic; o que é gravado: o mic sempre, inclusive mutado no Meet, e todo o som do computador; o `.ogg` estéreo; "Eu" no texto; recomendação de usar fone ou o `mic_limpo` para menos eco). Pré-requisito: `pipewire-utils` (para `pw-dump`/`pw-record`; já vem no Fedora KDE).
- [ ] **Step 2:** `AGENTS.md`: atualizar "Estado" (calls implementadas na branch `feat/calls`, data 2026-10-28) e tirar "Gravar call" da lista "Depois".
- [ ] **Step 3:** `cargo test` e `cargo build --release` passam.
- [ ] **Step 4: Commit** — `"README e AGENTS: gravação de calls"`
