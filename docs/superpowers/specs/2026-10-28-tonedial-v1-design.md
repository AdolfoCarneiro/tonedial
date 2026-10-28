# Tone Dial v1: design

Data: 2026-10-28

## Para que serve

Reuniões longas (até umas duas horas) que hoje ficam sem registro: presenciais, ou calls fora do Teams, onde não tem Granola (não existe para Linux) nem Plaud. O usuário grava no celular (gravador padrão) ou no próprio notebook, e o Tone Dial transforma o áudio em uma transcrição completa com os falantes separados. O objetivo é não perder o contexto de calls importantes e ter o texto para alimentar uma wiki pessoal. Resumir, extrair decisões etc. acontece depois, fora do app (o usuário ou uma IA).

## Objetivos

- App de desktop **nativo** para Linux (Fedora KDE), com janela simples.
- **Gravar no notebook**: escolher o microfone, ver as ondinhas (nível do sinal) para saber se está captando bem, cronômetro. Ao parar, processa sozinho.
- **Abrir um áudio existente** (vindo do celular por KDE Connect/Syncthing), por botão ou arrastar e soltar, e processar.
- **Transcrição em pt-BR com separação de falantes** (Falante 1, Falante 2...).
- **Máxima qualidade.** O tempo de processamento não importa (decisão do usuário).
- Tudo local: sem servidor, sem nuvem. A internet só é usada para baixar os modelos na primeira vez.

## Não é v1 (depois)

- Windows (o usuário tem um PC com 12 GB de VRAM, mas na v1 basta Linux).
- Outros idiomas além de pt-BR.
- Reconhecer a voz do usuário e, depois, nomear quem é quem.
- Renomear falantes dentro do app (na v1 o usuário edita o `.md`).
- Gravar o áudio do sistema junto com o mic (calls, via PipeWire).
- Vigiar uma pasta e processar sozinho o que chegar nela.
- Resumo ou qualquer pós-processamento do texto.

## Anti-objetivos (seria fracasso mesmo funcionando)

- Electron, ou qualquer coisa que não seja um "appzinho nativo".
- Mandar áudio para fora da máquina.
- O app conhecer o vault do Obsidian. Ele só salva numa pasta escolhida.
- Ficar complicado: mais de uma tela, configurações demais, fluxo com várias etapas.
- Transcrição que inventa texto em silêncio ou repete frases em loop (alucinação clássica do Whisper em áudio longo).

## Máquina

Notebook com Fedora KDE e RTX 2050 (4 GB de VRAM), com CUDA.

## A janela (aprovada com mockup)

Uma tela só, sem abas, de cima para baixo:

1. **Microfone:** um seletor com os dispositivos de entrada.
2. **Ondinhas:** uma área grande mostrando o nível/forma de onda ao vivo. Antes de gravar, ela também já mostra o sinal do mic selecionado, para o usuário conferir que está captando.
3. **Cronômetro** grande (`00:00:00`).
4. **Botão Gravar** ocupando a largura toda. Durante a gravação vira "Parar".
5. Um divisor "ou".
6. **Área de arquivo:** "Arraste um áudio aqui ou clique para abrir".
7. **Rodapé:** "Salvar em: <pasta> ✎". A pasta é escolhida uma vez e o app lembra dela.

**Processando:** substitui ou cobre o conteúdo principal com o nome do arquivo, a etapa atual (transcrevendo / separando falantes), uma barra de progresso e uma estimativa do tempo restante.

**Pronto:** `✓ <nome>.md · N falantes · duração`, com os botões "Abrir texto" e "Abrir pasta" (abrem com o programa padrão do sistema) e um jeito de voltar à tela inicial.

Mockups em `.superpowers/brainstorm/` (fora do git).

## Comportamento

- **Um áudio por vez.** Enquanto processa, não dá para gravar nem abrir outro.
- **Áudio gravado** fica salvo na pasta de saída com o mesmo nome do texto, para reprocessar se preciso. *Decisão minha:* durante a gravação o app escreve num arquivo temporário (para não perder nada se travar). Ao parar, converte para Opus (`.ogg`, ~64 kbps mono, sem perda perceptível em fala) via ffmpeg, para o arquivo não ficar enorme (1h30 em WAV a 48 kHz dá ~500 MB, em Opus ~45 MB). O processamento usa o áudio original sem compressão. O `.ogg` é só a cópia guardada.
- **Arquivo aberto** (do celular) não é copiado nem movido. Só o `.md` vai para a pasta de saída.
- **Formatos de entrada:** qualquer coisa que o ffmpeg leia (m4a, mp3, ogg, opus, wav...). O app converte para 16 kHz mono antes de processar.
- **Fechar a janela no meio do processamento:** o app pergunta antes. Se confirmar, o trabalho é perdido, mas o áudio continua salvo.
- **Primeira execução:** os modelos não vêm junto com o app. Ele baixa tudo com progresso visível e guarda num diretório de dados do usuário (ex.: `~/.local/share/tonedial/models`). Depois disso funciona sem internet.
- **Erros** (ffmpeg ausente, falha de download, áudio ilegível): mensagem clara na própria janela e volta para a tela inicial.

## Saída

Um `.md` por áudio, na pasta escolhida.

- **Nome:** data e hora da gravação, `AAAA-MM-DD-HHMM.md`. *Decisão minha:* para o arquivo aberto, a data vem, nesta ordem, da tag `creation_time` do áudio (via ffprobe), de uma data/hora no próprio nome do arquivo (gravadores de celular costumam colocar, ex.: `Gravação 2026-10-28 14-30.m4a`, `20261028_143000.m4a`) e, por último, da data de modificação. A data de modificação é o último recurso porque KDE Connect/Syncthing costumam gravar ali a hora da transferência. *Decisão minha:* se o nome já existir, acrescenta `-2`, `-3`...
- **Conteúdo:** uma linha (parágrafo) por fala, na ordem:

  ```
  **Falante 1** [00:12:30]: texto...

  **Falante 2** [00:12:41]: texto...
  ```

  Falas seguidas da mesma pessoa viram um só parágrafo. O timestamp é o início da fala. Os falantes são numerados na ordem em que aparecem pela primeira vez.
- *Decisão minha:* um pequeno cabeçalho no topo com data, duração e nome do áudio de origem. Pode cortar se atrapalhar.

## Caminho técnico (decisões minhas, com base na pesquisa de 2026-10-28)

O usuário deu liberdade de linguagem, desde que seja nativo e não Electron.

- **Linguagem: Rust.** Gera um executável leve, sem exigir Python e PyTorch na máquina, e as bibliotecas necessárias existem.
- **Janela: egui (eframe).** Simples, desenha as ondinhas sem esforço e funciona no Windows depois.
- **Gravação:** `cpal` (via PipeWire/ALSA).
- **Conversão de áudio:** `ffmpeg` chamado como processo externo, como dependência do sistema.
- **Transcrição:** `whisper-rs` 0.16 (whisper.cpp), modelo **`ggml-large-v3` quantizado em q8_0**, idioma fixo em `pt`.
  - Na pesquisa, o large-v3 superou o turbo em pt-BR (WER no FLEURS-pt de 3,88 contra 4,17, com distância provavelmente maior em áudio real). O q8_0 praticamente não perde qualidade e fica em ~2 a 2,5 GB de VRAM (estimativa, não medido).
  - *Decisão minha:* **backend Vulkan, não CUDA.** O notebook tem o driver NVIDIA, mas não tem o CUDA toolkit (`nvcc`), que o build com `cuda` exige. No Fedora, instalá-lo significa repo da NVIDIA e possivelmente outro gcc. Vulkan não pede nada disso, dá o mesmo resultado de transcrição (só muda a velocidade, e o tempo não importa) e facilita o Windows depois. Se o Vulkan der problema na RTX 2050, o CUDA é a alternativa, e instalar o toolkit fica a critério do usuário.
  - *Decisão minha:* **de onde vem o q8_0.** O repo oficial (`ggerganov/whisper.cpp` no Hugging Face) publica o large-v3 só em f16 (`ggml-large-v3.bin`) e em q5_0. Não publica em q8_0 (conferido em 2026-10-28). Na primeira execução o app baixa o f16, quantiza localmente para q8_0 com o código de quantização do whisper.cpp e apaga o f16. Se isso se mostrar trabalhoso demais, o fallback é usar o `ggml-large-v3-q5_0.bin` oficial, com perda pequena de qualidade. Nada de hospedar arquivo por conta própria.
  - *Decisão minha:* **falta de VRAM cai para CPU sozinho**, com aviso na tela, em vez de dar erro, já que o tempo não importa.
  - **VAD Silero ligado** (o whisper.cpp já expõe), para só transcrever trechos com fala. Desligar o condicionamento no texto anterior e descartar repetições, contra alucinações.
  - Timestamps por palavra/token (DTW) para o alinhamento com os falantes.
- **Separação de falantes:** crate `speakrs` (pipeline estilo pyannote community-1: segmentação powerset, embeddings WeSpeaker, PLDA+VBx). Ele trata sobreposição e estima o número de falantes. Roda **depois** da transcrição, para não disputar VRAM. CPU é aceitável.
  - **Plano B:** a diarização do crate oficial `sherpa-onnx` (segmentação pyannote 3.0 + embeddings 3D-Speaker/NeMo). Um pouco pior, mas mais madura.
  - O `speakrs` é novo (v0.5) e depende do `ort` em release candidate. Antes de construir em cima dele, validar que compila, roda e que a licença dos modelos que ele baixa permite o uso.
- **Juntar:** cada palavra vai para o falante cujo segmento tem maior sobreposição com o tempo dela. Depois agrupa em falas.
- **Primeiro passo de construção:** testar large-v3 q8 contra turbo e `speakrs` contra `sherpa-onnx` antes de fechar os modelos. O usuário ainda não tem uma gravação real de reunião. *Decisão minha:* usar áudio público em pt-BR, conversacional e com vários falantes, de preferência com transcrição de referência (ex.: trechos do corpus CORAA/NURC, ou um podcast brasileiro com 3+ pessoas), com um trecho de 10 a 20 minutos. Se houver referência, comparar por WER e acerto de falantes. Se não houver, mostrar os trechos lado a lado para o usuário escolher. A regra de desempate é o **padrão large-v3 q8 + `speakrs`**, a menos que fique claramente pior ou não funcione. Quando o usuário gravar a primeira reunião de verdade, vale rever a escolha.

Fontes principais: benchmark FLEURS-pt (https://models.handy.computer/languages/pt), whisper-rs (https://codeberg.org/tazz4843/whisper-rs), speakrs (https://github.com/attevon-llc/speakrs), sherpa-onnx (https://k2-fsa.github.io/sherpa/onnx/speaker-diarization/index.html), pyannote community-1 (https://huggingface.co/pyannote/speaker-diarization-community-1).

## Fica com quem for construir

- Organização do código, crates auxiliares, tratamento de threads (a UI não pode travar durante o processamento).
- Visual exato das ondinhas (barra de nível ou forma de onda rolando), cores e fontes.
- Onde a configuração é guardada (pasta de saída, último mic usado).
- Como empacotar e instalar no Fedora (binário + `.desktop` já basta).
- Testes: pelo menos o alinhamento palavra→falante e a geração do `.md` com testes automatizados, e um áudio curto de exemplo para teste ponta a ponta.
