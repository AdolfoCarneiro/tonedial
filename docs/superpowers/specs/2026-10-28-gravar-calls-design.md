# Tone Dial: gravar calls (design)

Data: 2026-10-28

## Para que serve

Hoje o Tone Dial grava só o microfone. Numa call pelo navegador (principalmente Google Meet), a voz das outras pessoas sai pelo alto-falante ou pelo fone e não fica registrada direito. O usuário quer o que o Granola fazia: o app percebe que começou uma call, pergunta "tá numa call, quer gravar?" e grava o próprio microfone e o som das outras pessoas. Depois transcreve como já faz hoje.

Calls do Teams têm transcrição própria, mas a notificação pode aparecer nelas também. O usuário ignora (decisão dele). Calls presenciais continuam como hoje (gravar o mic ou abrir o áudio do celular).

## Objetivos

1. **Ficar na bandeja e abrir com o sistema.** O Tone Dial passa a rodar em segundo plano, com ícone na bandeja do KDE. Fechar a janela só esconde. Ele abre sozinho no login, já escondido.
2. **Perceber a call.** Quando outro programa (o navegador, na prática) começa a usar o microfone, o app mostra uma notificação do sistema: "Parece que você está numa call. Gravar?", com os botões **Gravar** e **Agora não**. Ignorar é o mesmo que "Agora não".
3. **Gravar os dois lados.** Ao aceitar, grava ao mesmo tempo, em arquivos separados:
   - o microfone escolhido no app (o mesmo da gravação normal);
   - o som que sai do computador (as outras pessoas da call).
4. **Não parar sozinho.** Quando o outro programa solta o microfone, o app **pergunta** "A call acabou? Parar e processar?", com **Parar** e **Continuar gravando**. Ele não para por conta própria, porque a call pode ter só caído (decisão do usuário).
5. **Marcar quem é você.** No texto, o que veio do microfone aparece como **Eu**. As outras pessoas são separadas como hoje (**Falante 1**, **Falante 2**...), rodando a separação só no som do computador.
6. **Gravar call à mão.** Na janela dá para gravar uma call sem depender da detecção, pelo caso de ela falhar.

## Não é agora

- Pausar o próprio microfone. **O lado do usuário é gravado sempre**, mesmo quando ele está mutado no Meet, inclusive conversa paralela e barulho do lugar (decisão do usuário).
- Ficar quieto em calls do Teams ou escolher quais programas disparam a notificação.
- Gravar só o som do navegador (em vez de todo o som do computador).
- Reconhecer a voz do usuário por amostra, ou nomear os outros falantes.
- Windows.

## Anti-objetivos

- Gravar sem o usuário aceitar. Nada começa sem um clique em **Gravar** (na notificação, na bandeja ou na janela).
- Parar uma gravação de call sem perguntar.
- Pesar no notebook enquanto está só esperando na bandeja.
- Complicar a janela: continua uma tela só, sem tela de configurações.
- Mandar áudio para fora da máquina (vale tudo o que já vale na v1).

## Como funciona hoje (o que já existe)

- Janela egui/eframe com uma tela que troca de estado (`Setup`, `Idle`, `Recording`, `Processing`, `Done`, `Error`), em `src/ui/`.
- Gravação do mic com `cpal` em WAV temporário em `~/.local/share/tonedial/gravacoes/gravacao-AAAAMMDD-HHMMSS.wav`. Gravações que sobram (app fechou no meio) aparecem como "Gravação não processada" com **Processar** e **Descartar**.
- Pipeline (`src/pipeline.rs`): decodifica para 16 kHz mono, guarda a gravação como `.ogg` na pasta de saída, transcreve com whisper, separa falantes com speakrs e escreve o `.md`.
- Bloqueio de suspensão (`systemd-inhibit`) enquanto grava/processa.

## Máquina (o que foi visto em 2026-10-28)

- Fedora KDE no Wayland, PipeWire 1.6.9 com `pipewire-pulse`. `pw-dump`, `pw-record`, `pactl` e `parec` instalados.
- A fonte padrão é `mic_limpo`, um filtro de cancelamento de eco e ruído (WebRTC, `libpipewire-module-echo-cancel`) criado pelo usuário em `~/.config/pipewire/pipewire.conf.d/60-mic-limpo.conf`. Isso já tira do mic boa parte da voz que sai do alto-falante, o que ajuda a não ter "Eu" falando o que os outros falaram.
- Navegador padrão: **Opera** (Flatpak, baseado em Chromium). Também tem Firefox, Discord (Flatpak) e Teams for Linux (Flatpak).
- O serviço de notificações do KDE suporta botões (`actions`) e a bandeja (`org.kde.StatusNotifierWatcher`) existe.

### Spike feito (descartável, em 2026-10-28)

Um programa de teste com eframe 0.37, `ksni` 0.3.7 (feature `blocking`) e `notify-rust` 4.18.2 confirmou nesta máquina:

- `eframe::run_native` com `run_and_return: true` roda, fecha e **abre de novo no mesmo processo** no Wayland, sem erro.
- O ícone na bandeja via `ksni` sobe sem erro.
- A notificação com dois botões aparece.

### Como um programa usando o mic aparece no PipeWire (medido)

No `pw-dump`, o stream de gravação de um programa é um nó com `media.class = "Stream/Input/Audio"`. O pid do programa fica nas props do **client** do nó (`client.id` → `application.process.id` / `pipewire.sec.pid`), não no nó. Existem links (`PipeWire:Interface:Link`) do nó da fonte (`media.class = "Audio/Source"`, ex.: `mic_limpo`) para o nó do stream.

Também aparecem streams que **não** são programas usando o mic:

- o próprio filtro `mic_limpo.capture` (`node.passive = true`, sem client de aplicativo);
- streams que gravam o som de saída (`stream.capture.sink = true`), como o que o próprio Tone Dial vai abrir;
- os streams do próprio Tone Dial (o `Monitor` das ondinhas e o `Recorder`, pelo pid do processo).

## O que muda

### 1. Processo em segundo plano com bandeja

- `tonedial` passa a ser um processo de longa duração. Sem argumentos, abre a janela. Com `--background` (usado pelo autostart), sobe escondido, só com a bandeja.
- **Uma instância só.** Se já existe um Tone Dial rodando, abrir de novo (pelo menu do KDE, por exemplo) só manda o que está rodando mostrar a janela e sai. *Decisão minha:* socket Unix em `$XDG_RUNTIME_DIR/tonedial.sock`.
- **Fechar a janela esconde.** O app continua na bandeja, inclusive gravando ou processando. A confirmação de fechar que existe hoje deixa de valer para o botão de fechar da janela e vai para o **Sair** da bandeja.
- **Bandeja** (`ksni`). Clique no ícone abre a janela. Menu:
  - **Abrir Tone Dial**
  - **Gravar call** / **Parar gravação** (conforme o estado)
  - **Sair**. Gravando ou processando, abre a janela com a confirmação de hoje antes de sair.
  - *Decisão minha:* o ícone (ou o tooltip) muda enquanto grava, para dar para ver que está gravando.
- **Abrir com o sistema.** *Decisão minha:* ao iniciar, o app grava `~/.config/autostart/tonedial.desktop` (com `Exec=<caminho do executável> --background`) se ele não existir. O usuário já disse que quer. *Decisão minha (na implementação):* o arquivo gerado leva a marca `X-Tonedial-Generated=true`, e o app corrige o `Exec` se o executável mudar. Ele não cria nem mexe no arquivo quando roda de um `target/` do cargo. Como apagar o arquivo não adianta (o app recria no próximo início), para desligar é preciso pôr `Hidden=true` no arquivo ou tirar a linha da marca (vai no README), sem opção na janela.
- **Como a janela some e volta.** No Wayland, esconder uma janela do winit não funciona. Por isso, fechar encerra o `run_native`, e o estado do app (gravação, processamento, configuração) continua vivo fora dele. Abrir de novo chama `run_native` outra vez (validado no spike). Enquanto a janela está fechada, o processo fica num laço que espera eventos (bandeja, detector, notificações, outra instância pedindo a janela) e continua cuidando da gravação e do processamento que já acontecem hoje dentro do `update()` (ler o progresso do pipeline, perceber o mic que sumiu etc.). Detalhes ficam com quem for construir. O que importa é que **nada do que roda hoje pode depender de a janela estar aberta**.
- **O que aparece com a janela fechada.** *Decisões minhas:*
  - Clicar **Gravar** na notificação **não** abre a janela (para não atrapalhar a call). Quem mostra que está gravando é o ícone da bandeja (muda de aparência) e o tooltip ("Gravando call · 00:12:34").
  - Ao terminar o processamento com a janela fechada, uma notificação "Transcrição pronta: <nome>.md" com os botões **Abrir texto** e **Abrir pasta**. Os avisos do processamento (CPU, separação falhou, som do computador parou) vão no corpo dessa notificação.
  - Se o processamento falhar com a janela fechada, uma notificação "Não consegui transcrever a gravação" com o botão **Abrir Tone Dial**, que abre a janela na tela de erro de hoje.
  - Com a janela aberta, nada disso vira notificação: a janela já mostra.
- **Modelos não baixados.** Sem os modelos, a gravação ainda funciona (o WAV fica pendente), mas o processamento não. *Decisão minha:* se os modelos faltam ao aceitar uma gravação pela notificação, grava normalmente. Ao parar, a janela abre na tela de preparo de hoje e a gravação fica como pendente.

### 2. Detector de call

- Uma thread lê o grafo do PipeWire **a cada ~2 s** com `pw-dump` (custa ~40 ms por chamada aqui; sem dependência nova de compilação). *Decisão minha:* polling em vez de `pw-dump --monitor` ou do crate `pipewire`, por ser mais simples e fácil de testar.
- **"Alguém está usando o mic"** quando existe pelo menos um nó que satisfaz tudo isto:
  - `media.class` é `Stream/Input/Audio`;
  - não é `node.passive = true`;
  - não tem `stream.capture.sink = true`;
  - tem um link vindo de um nó `Audio/Source` (ou `Audio/Source/Virtual`);
  - o pid do client não é o do próprio Tone Dial nem de um processo filho dele.
- O nome do programa (`application.name` do client, ex.: "Opera", "Firefox") vai no texto da notificação quando existir: "O Opera está usando o microfone. Gravar a call?".
- A parte que decide (recebe o JSON do `pw-dump`, devolve quem está usando o mic) é uma função pura com testes usando JSON de exemplo, incluindo o `mic_limpo.capture`, um stream `stream.capture.sink`, um stream do próprio pid e um stream sem link.
- **Para evitar alarme falso:**
  - Só notifica depois de o mic estar em uso por **3 s seguidos** (site testando o mic, por exemplo).
  - Só considera que soltou depois de **5 s seguidos** sem ninguém usando.
  - Uma notificação por "sessão de uso". Se o usuário disse "Agora não" ou ignorou, não pergunta de novo até o mic ser solto e pego outra vez.
  - Não notifica se o Tone Dial já está gravando (qualquer tipo de gravação) ou processando.
- *Decisão minha:* se o pw-dump falhar (PipeWire fora do ar, comando ausente), o detector fica quieto e tenta de novo no próximo ciclo. Nada aparece para o usuário além de uma linha no stderr.

### 3. Gravar os dois lados

- **Microfone:** o `Recorder` de hoje (`cpal`), com o mic escolhido na janela (`config.last_mic` ou o padrão).
- **Som do computador:** processo `pw-record` com `stream.capture.sink=true`, sem alvo fixo, gravando f32 mono 48 kHz para stdout, que o app lê e escreve em WAV. Sem alvo fixo, o WirePlumber liga o stream à saída padrão e o acompanha se a saída mudar (ex.: o usuário liga o fone Bluetooth no meio da call). Isso precisa ser conferido de verdade (vai na lista de conferência). As props do stream devem identificar o Tone Dial (ex.: `node.name=tonedial.sistema`, `application.name=Tone Dial`).
  - Grava **todo** o som que sai do computador, não só o do navegador. Se tocar música ou notificação durante a call, entra também.
  - Testado aqui: com nada tocando, o stream entrega silêncio continuamente (~48k amostras/s), então o WAV fica do tamanho certo.
- **Arquivos:** dois WAVs em `gravacoes/` com o mesmo carimbo de tempo: `gravacao-AAAAMMDD-HHMMSS-mic.wav` e `gravacao-AAAAMMDD-HHMMSS-sistema.wav`. As gravações pendentes reconhecem o par e o tratam como uma gravação só (**Processar**/**Descartar** valem para os dois arquivos). Um par incompleto (só um dos dois) é tratado como gravação simples do arquivo que sobrou.
- **Sincronia:** os dois começam com diferença de alguns décimos de segundo. *Decisão minha:* anotar o instante do primeiro bloco de cada lado e completar com silêncio o começo do que começou depois, para os dois ficarem alinhados no tempo. Diferença residual de ~100 ms é aceitável.
- **Se um dos lados falhar no meio:** mic sumiu → mesmo comportamento de hoje (para e processa o que tem). `pw-record` morreu → *decisão minha:* tenta reabrir uma vez. Se não der, continua só com o mic e avisa na tela de processamento ("O som do computador parou de ser gravado às HH:MM").
- **Se o `pw-record` não existir ou falhar ao abrir:** a gravação de call vira gravação só do mic, com aviso. Não impede de gravar.
- **Ondinhas:** durante uma gravação de call, a janela mostra duas faixas (Você / Call) em vez de uma.
- Cronômetro, bloqueio de suspensão e recuperação de gravação interrompida funcionam como hoje.

### 4. Pergunta de fim de call

- Gravando uma call, quando o detector diz que ninguém mais usa o mic (depois dos 5 s), aparece a notificação "A call acabou? Parar e processar?" com **Parar** e **Continuar gravando**.
- **Parar** → para os dois lados e processa (fluxo de hoje).
- **Continuar gravando** ou ignorar → segue gravando. *Decisão minha:* se em **5 minutos** ainda ninguém estiver usando o mic, pergunta de novo. Se o mic voltar a ser usado (a call voltou), a pergunta pendente é fechada e o ciclo recomeça.
- Isso vale só para gravações de call iniciadas pela notificação ou pela bandeja/janela com o som do computador. Gravação só do mic (uso presencial) nunca pergunta.
- *Decisão minha:* a pergunta só aparece se, **durante esta gravação**, algum outro programa foi visto usando o mic e depois soltou. Uma call gravada à mão antes de a call começar (ou numa call que a detecção não enxerga) não recebe a pergunta 5 s depois de começar. Ela só aparece quando a call de fato começou e terminou. Sem isso, o usuário para à mão, como hoje.
- Ao mutar no Meet, o navegador normalmente **não** solta o mic, mas isso não foi testado. Se soltar, a pergunta aparece no meio da call e o usuário responde **Continuar** (aceito por ele).

### 5. Gravar call à mão (janela)

- *Decisão minha:* na tela inicial, abaixo do seletor de microfone, uma caixa de seleção **"Gravar também o som do computador (call)"**. O botão **Gravar** de sempre grava conforme ela. O app lembra a escolha em `config.json`.
- O item **Gravar call** da bandeja grava com o som do computador, independente da caixa.

### 6. Processamento de uma call

Entrada: o par de WAVs (mic e sistema) com o mesmo início.

1. Decodifica os dois para 16 kHz mono.
2. Salva a gravação na pasta de saída como **um `.ogg` estéreo**: canal esquerdo = mic, canal direito = sistema (Opus, como hoje; *decisão minha:* ~96 kbps para os dois canais). Dá para ouvir normalmente e permite reprocessar depois. Apaga os WAVs quando o `.ogg` existir (como hoje).
3. Transcreve os dois com o whisper (mesmas configurações de hoje: VAD, anti-loop, reparo de blocos). Carregar o modelo uma vez e transcrever os dois é preferível, mas fica com quem for construir.
4. Separa falantes **só no áudio do sistema**, como hoje. Se a separação falhar, as falas do sistema viram todas "Falante 1" (como hoje) e as do mic continuam "Eu".
5. **Eco/vazamento:** sem fone, a voz das outras pessoas pode vazar para o mic mesmo com o filtro. *Decisão minha:* uma palavra do mic é descartada se existe uma palavra do sistema com o mesmo texto normalizado (a `normalize` que já existe) começando a até ~1 s de distância. Função pura, com testes.
6. **Junta:** palavras do mic com o rótulo "Eu", palavras do sistema com o falante que vier do alinhamento de hoje (`align`), tudo ordenado por tempo. Palavras seguidas do mesmo falante viram uma fala só. A numeração dos falantes do sistema continua por ordem de aparição, começando em 1.
7. **Se o sistema ficou mudo** (call só de áudio pelo próprio mic, ou o `pw-record` falhou desde o começo), o resultado é só "Eu". Se o **mic** ficou mudo, só os Falantes.

Formato do `.md` (o mesmo de hoje, com "Eu"):

```
# Transcrição 2026-10-28 14:30

- Duração: 47min
- Áudio: 2026-10-28-1430.ogg (call: mic + som do computador)

**Eu** [00:00:03]: bom dia, gente...

**Falante 1** [00:00:07]: bom dia! então...
```

- A contagem de falantes na tela de pronto inclui o "Eu".
- Abrir um `.ogg` estéreo antigo pelo "Abrir arquivo" processa como áudio comum (mistura para mono, como hoje). *Decisão minha:* não tentar adivinhar que é uma call.

## Fica com quem for construir

- Organização do código (módulos novos para bandeja, detector, gravação do sistema, instância única), e como o estado do app sobrevive ao fechar a janela.
- Crates: `ksni` (bandeja) e `notify-rust` (notificações) foram validados no spike. Pode trocar se houver motivo.
- Ícone da bandeja (pixmap a partir de `assets/icons/`, ou nome de ícone instalado).
- Textos exatos das notificações e da janela, em pt-BR.
- Testes automatizados pelo menos para: o filtro de "quem está usando o mic" (JSON de exemplo), a máquina de estados do detector (tempos de 3 s/5 s/5 min com relógio injetado, e a regra de só perguntar o fim depois de ver o mic em uso nesta gravação), a remoção de eco, a junção Eu + Falantes e o `.md`, o pareamento dos WAVs pendentes e o preenchimento de silêncio da sincronia.
- Atualizar o README (bandeja, autostart, como desligar, o que é gravado numa call) e o `AGENTS.md` (estado).

## Para o usuário conferir de manhã (não dá para testar sem ele)

- Uma call real no Meet pelo Opera: a notificação aparece? Grava os dois lados? A pergunta de fim aparece ao sair da call?
- Mutar e desmutar no Meet: o navegador solta o mic (e a pergunta aparece no meio da call)?
- Trocar a saída de som no meio da gravação (fone Bluetooth): o lado do sistema continua gravando?
- Sem fone (alto-falante do notebook): a voz das outras pessoas aparece como "Eu"?
- Reiniciar o notebook: o Tone Dial sobe sozinho na bandeja?
