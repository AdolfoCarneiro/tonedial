# Tone Dial

## O que é

Um app de desktop pequeno, em **Rust nativo**, para o meu notebook Linux, que transforma áudio em texto **local**: grava direto no notebook ou pega um áudio que já existe (gravado no celular, por exemplo), transcreve e separa quem falou.

O nome vem de One Piece: o Tone Dial é a concha de Skypiea que grava um som e toca de volta. Também pensei em Mantra (o Haki da Observação em Skypiea, que ouve todo mundo); fiquei com Tone Dial.

## Por que

- Acontece muita coisa no dia e, na hora de escrever o diário, não lembro de tudo.
- Gravar com o notebook é ruim, e vou aposentar o Granola: ele não separa pessoas diferentes, não tem versão para PC (Linux) e às vezes estou sem o notebook.
- Conversa presencial e call fora do Teams ficam sem registro.
- O celular é o que está sempre comigo. A ideia é gravar nele e passar o áudio para o PC (KDE Connect ou Syncthing) para processar.

## A v1 (decidido)

- **Janela simples**, não linha de comando.
- **Gravar direto no notebook** e, ao parar, processar sozinho.
- **Abrir um áudio que já existe** e processar.
- **Transcrição e separação de falantes** (Falante 1, Falante 2...) já na v1.
- Tudo local: sem servidor, sem nuvem.
- Simples de verdade. Nada complicado.

## Depois (não é v1)

- Saber quando quem fala sou eu (com uma amostra da minha voz) e, mais para frente, quem é quem pelo nome.
- Gravar call: pegar o áudio do sistema junto com o microfone (PipeWire).

## Fora

- **O app não conhece o meu vault do Obsidian.** Ele salva o texto numa pasta que eu escolho, e eu decido o que vai para lá.
- Sem servidor e sem app de celular. O celular usa o gravador padrão.
- Gravar o dia inteiro, estilo Plaud: descartado por enquanto (áudio enorme, pouco sinal, e seria gravar gente sem ela saber).

## Máquina

Notebook com Fedora KDE e uma **RTX 2050 (4 GB de VRAM)**, com CUDA.

## Caminho técnico sugerido (não decidido, rever no design)

- **Transcrição:** Whisper large-v3-turbo via `whisper-rs` (whisper.cpp com CUDA). Cabe com folga nos 4 GB; o large-v3 cheio só cabe quantizado e fica no limite.
- **Separação de falantes:** o melhor aberto é o pyannote (community-1), mas ele é Python. Em Rust, rodar em ONNX (`sherpa-rs`/sherpa-onnx ou `pyannote-rs`).
- **Áudio:** `cpal` para gravar; `ffmpeg` chamado por fora para converter m4a/mp3/ogg em 16 kHz mono.
- **Janela:** a escolher (egui, iced, slint...).
- Essa sugestão saiu de um modelo com conhecimento até meados de 2026. Vale checar se tem modelo melhor para pt-BR antes de fechar.

## Estado

v1 implementada na branch `feat/v1` (2026-10-28): janela, gravação, transcrição e separação de falantes. Modelos conforme `docs/superpowers/specs/2026-10-28-comparacao-modelos.md`. Instalação e uso no `README.md`.

## Convenções

- Conversa e docs em pt-BR.
