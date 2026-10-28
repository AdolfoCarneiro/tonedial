# Tone Dial

App de desktop em Rust para Linux que grava (ou abre) um áudio, transcreve e separa quem falou (Falante 1, Falante 2...). Tudo local: sem servidor, sem nuvem. O texto é salvo numa pasta que você escolhe.

## Pré-requisitos (Fedora)

```sh
sudo dnf install -y clang glslc alsa-lib-devel vulkan-loader-devel ffmpeg
```

Também precisa do Rust (cargo). Se o build não achar a libclang: `export LIBCLANG_PATH=/usr/lib64`.

A GPU é usada via Vulkan (sem CUDA). Ela só entra se houver VRAM livre suficiente (tamanho do modelo + 1 GiB); senão roda na CPU, mais devagar. `TONEDIAL_GPU_DEVICE=N` escolhe outro dispositivo Vulkan.

## Instalar

```sh
cargo install --path .
mkdir -p ~/.local/share/applications
sed "s|^Exec=tonedial|Exec=$HOME/.cargo/bin/tonedial|" assets/tonedial.desktop > ~/.local/share/applications/tonedial.desktop
update-desktop-database ~/.local/share/applications
```

Isso instala `tonedial` (a janela) e `tonedial-cli` (ferramenta de desenvolvimento) em `~/.cargo/bin`. O `sed` grava o caminho absoluto no `Exec` da cópia instalada, porque o ambiente da sessão do KDE normalmente não tem `~/.cargo/bin` no PATH (o `Exec=tonedial` do `assets/tonedial.desktop` só funciona se tiver).

## Primeira execução (modelos)

Os modelos são baixados na primeira vez, para `~/.local/share/tonedial/models`. Precisa de uns 5 GB livres durante o preparo.

- **Whisper large-v3:** baixa o f16 oficial (~3,1 GB), quantiza localmente para q8_0 (~1,66 GB) e apaga o f16.
- **VAD (Silero):** ~0,9 MB.
- **Separação de falantes:** 9 arquivos, ~33 MB.

A configuração fica em `~/.config/tonedial/config.json`.

## Como gravar bem no celular

- Use o gravador padrão do celular, no formato padrão dele (m4a, por exemplo).
- Evite modos de baixa qualidade ou "economia de espaço". Se der para escolher, use 44,1 ou 48 kHz e pelo menos 64 kbps.
- Deixe o celular no meio da mesa, perto de quem fala e longe do ar-condicionado.
- Passe o arquivo para o PC (KDE Connect ou Syncthing) e abra no app.

## Créditos e licenças

Este projeto é MIT (veja `LICENSE`). Ele baixa e usa modelos de terceiros:

- **Whisper** (OpenAI, MIT), executado com **whisper.cpp** (MIT) via `whisper-rs`.
- **Silero VAD** (MIT).
- **Separação de falantes:** [speakrs](https://github.com/avencera/speakrs) (Apache-2.0), com modelos exportados do pipeline **pyannote community-1**: segmentation-3.0 (MIT) e WeSpeaker voxceleb resnet34 e o pipeline community-1 / PLDA (CC-BY-4.0), de [pyannote](https://huggingface.co/pyannote) e WeSpeaker. Os modelos vêm do espelho `avencera/speakrs-models` no Hugging Face.
