# Tone Dial

App de desktop em Rust para Linux que grava (ou abre) um áudio, transcreve e separa quem falou (Falante 1, Falante 2...). Tudo local: sem servidor, sem nuvem. O texto é salvo numa pasta que você escolhe.

## Pré-requisitos (Fedora)

```sh
sudo dnf install -y clang glslc alsa-lib-devel vulkan-loader-devel ffmpeg pipewire-utils
```

`pipewire-utils` (`pw-dump`/`pw-record`) é usado para gravar calls; já vem no Fedora KDE.

Também precisa do Rust (cargo). Se o build não achar a libclang: `export LIBCLANG_PATH=/usr/lib64`.

A GPU é usada via Vulkan (sem CUDA). Ela só entra se houver VRAM livre suficiente (tamanho do modelo + 1,5 GiB); senão roda na CPU, mais devagar. `TONEDIAL_GPU_DEVICE=N` escolhe outro dispositivo Vulkan.

## Instalar

```sh
cargo install --path .
mkdir -p ~/.local/share/applications
sed "s|^Exec=tonedial|Exec=$HOME/.cargo/bin/tonedial|" assets/tonedial.desktop > ~/.local/share/applications/tonedial.desktop
update-desktop-database ~/.local/share/applications
for n in 16 32 48 64 128 256 512; do install -Dm644 assets/icons/tonedial-$n.png ~/.local/share/icons/hicolor/${n}x${n}/apps/tonedial.png; done
install -Dm644 assets/icons/tonedial.svg ~/.local/share/icons/hicolor/scalable/apps/tonedial.svg
```

Isso instala `tonedial` (a janela) e `tonedial-cli` (ferramenta de desenvolvimento) em `~/.cargo/bin`. O `sed` grava o caminho absoluto no `Exec` da cópia instalada, porque o ambiente da sessão do KDE normalmente não tem `~/.cargo/bin` no PATH (o `Exec=tonedial` do `assets/tonedial.desktop` só funciona se tiver).

## Primeira execução (modelos)

Os modelos são baixados na primeira vez, para `~/.local/share/tonedial/models`. Precisa de uns 5 GB livres durante o preparo.

- **Whisper large-v3:** baixa o f16 oficial (~3,1 GB), quantiza localmente para q8_0 (~1,66 GB) e apaga o f16.
- **VAD (Silero):** ~0,9 MB.
- **Separação de falantes:** 9 arquivos, ~33 MB.

A configuração fica em `~/.config/tonedial/config.json`.

## Calls

O app fica na bandeja do sistema. Fechar a janela só esconde; para sair de verdade use **Sair** no menu da bandeja (se estiver gravando ou processando, ele pergunta antes). Se a bandeja não subir, fechar a janela sai do app (com a mesma pergunta). Abrir o Tone Dial de novo enquanto ele roda só mostra a janela.

**Abre com o sistema.** Ao abrir, o app cria `~/.config/autostart/tonedial.desktop` (com `--background`) apontando para o executável que está rodando. Se o arquivo foi criado por ele (linha `X-Tonedial-Generated=true`) e o executável mudou, ele atualiza o caminho. Builds do cargo (`target/debug`, `target/release`, o `cargo run`) não criam nem mudam o arquivo: depois do `cargo install`, abrir o `~/.cargo/bin/tonedial` instalado basta. Para desligar, ponha `Hidden=true` no arquivo, ou apague a linha `X-Tonedial-Generated=true` e edite como quiser: o app não mexe mais nele. Só apagar o arquivo não adianta, porque ele é criado de novo na próxima vez que o app abrir.

**Detecção.** Quando outro programa usa o microfone por 3 s, chega uma notificação perguntando se quer gravar. Ignorar equivale a "Agora não". Chamadas do Teams também disparam; é só ignorar. Durante uma gravação de call, se o mic ficar livre por 5 s, ele pergunta "A call acabou? Parar e processar?". Nunca para sozinho: se você não responder, pergunta de novo depois de 5 min, e se a call voltar a pergunta some.

**Gravar call na mão.** Marque "Gravar também o som do computador (call)" na janela, ou use "Gravar call" na bandeja.

**O que é gravado.** O seu microfone sempre (mesmo mutado no Meet, conversas paralelas entram) e **todo** o som que sai do computador (música e notificações também). O `.ogg` é estéreo: esquerda = seu mic, direita = computador. No texto, **Eu** é o seu mic e Falante N são os outros.

**Eco.** Sem fone, parte da voz dos outros vaza para o seu mic. O app remove do seu lado as palavras que se repetem no som do computador. Um microfone com cancelamento de eco (como o `mic_limpo`, um filtro `echo-cancel` do PipeWire configurado por fora e escolhido como mic no app) ajuda, mas o melhor é usar fone. O Whisper às vezes inventa uma linha curta de "Eu" a partir do ruído da sala quando você não falou.

**Janela fechada.** Quando a transcrição fica pronta, chega uma notificação com **Abrir texto** e **Abrir pasta** (ou um aviso se falhar).

## Gravações

- Enquanto grava, o áudio fica em `~/.local/share/tonedial/gravacoes/` (fora da pasta de saída, para não ser sincronizado pela metade). Ao processar, vira `<data>.ogg` na pasta de saída e o WAV temporário é apagado.
- Se algo der errado, a mensagem de erro diz onde o áudio está salvo.
- Se o app fechar no meio (ou você fechar durante a gravação), a gravação fica guardada. Na próxima abertura aparece "Gravação não processada de ..." com **Processar** e **Descartar**.
- Enquanto grava, processa ou baixa os modelos, o app impede o notebook de suspender (`systemd-inhibit`).
- Se não separar os falantes (erro no modelo), o texto sai mesmo assim, todo como Falante 1, com um aviso.

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
