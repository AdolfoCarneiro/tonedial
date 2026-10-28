# Comparação de modelos (Task 6, passos 6–7)

Data: 2026-10-28. Código: `tonedial-cli` no commit `e4bbfe4` (inclui a correção de palavras coladas, `6b4f685`).

## Setup

- Máquina: Fedora KDE, RTX 2050 (4 GB). O desktop já usa cerca de 2,4 GB de VRAM, então o preflight de VRAM do app mandou **todas as execuções para CPU** ("Sem memória na GPU, usando CPU"). A qualidade da transcrição é a mesma na CPU; só o tempo muda.
- Modelos Whisper (ggerganov/whisper.cpp): `ggml-large-v3-q5_0.bin` (1,08 GB), `ggml-large-v3-turbo-q8_0.bin` (0,87 GB); o `large-v3` q8_0 (1,66 GB) foi quantizado localmente do f16 com a ferramenta `whisper-quantize`. VAD: `ggml-silero-v5.1.2.bin`.
- Diarização: speakrs 0.7.0 (pyannote community-1 em Rust) e sherpa-onnx (pyannote segmentation-3.0 ONNX + embedding 3D-Speaker CAM++ `zh_en`, com os limiares padrão do app).
- Áudios (rodadas sequenciais, uma por vez):
  - **NURC**: trecho de 5 min (0–300 s) do NURC-SP Corpus Minimo (SP_D2_062, gravação de 1972, 3 falantes). Tem referência por segmento com falante. **Atenção:** o áudio foi reconstruído a partir de segmentos soltos (cada segmento colocado no seu tempo de início sobre uma linha do tempo silenciosa; sobreposições sobrescrevem), então é 1972, qualidade ruim e artificial. Serve para comparar modelos entre si, não como medida absoluta.
  - **real5**: 5 min (minuto 21–26) de uma reunião real do usuário (47 min, vários falantes, conversa densa). Sem referência: avaliação qualitativa. O áudio é privado e não está no repositório; os trechos abaixo são curtos e escolhidos por não conterem dado sensível.
- Métricas: WER sobre texto normalizado (minúsculas, sem pontuação, espaços colapsados). Acerto de falante é **aproximado**: o `.md` só traz o início de cada turno, então cada segmento de referência recebe o falante do turno ativo no seu ponto médio, contado por palavras e com o melhor mapeamento falante-saída para falante-referência.

## Resultados

| Modelo | Diarizador | Áudio | WER | Falantes detectados | Acerto de falante (aprox.) | Tempo | Device |
|---|---|---|---|---|---|---|---|
| large-v3 q8_0 | speakrs | NURC 5 min | 64,7% | 2 (de 3) | 54,9% | 7m49s | CPU |
| large-v3 q5_0 | speakrs | NURC 5 min | 72,3% | 2 (de 3) | 53,3% | 7m14s | CPU |
| large-v3-turbo q8_0 | speakrs | NURC 5 min | 75,0% | 2 (de 3) | 53,3% | 5m23s | CPU |
| large-v3 q8_0 | speakrs | real5 | n/a | 3 | n/a (sem referência) | 7m26s | CPU |
| large-v3 q5_0 | speakrs | real5 | n/a | 3 | n/a | 8m15s | CPU |
| large-v3-turbo q8_0 | speakrs | real5 | n/a | 3 | n/a | 6m05s | CPU |
| large-v3 q8_0 | sherpa | real5 | n/a | 11 | n/a | 6m16s | CPU |

Dado de desempenho: o primeiro run (large-v3 q8_0 + speakrs, commit anterior com o bug das palavras coladas) em todo o clipe NURC de 12,1 min levou **30m09s na CPU** (cerca de 2,5x o tempo real). Nos trechos de 5 min o tempo ficou em 1,3 a 1,6x o tempo real (a CPU estava menos disputada). Os tempos incluem decodificação, VAD, Whisper e diarização.

WER absoluto alto (65–75%) é esperado: áudio de 1972 com sobreposição, fala espontânea, e o clipe reconstruído. Em ambos os modelos "menores" a transcrição **perdeu trechos inteiros** do NURC (374 e 463 palavras na saída contra 793 na referência; o q8_0 produziu 676). O ranking é o que importa.

## Trechos lado a lado

Real5, mesmo trecho (por volta de 00:04:05), puramente técnico:

- **q8_0**: "Hermes? Cara... Aí, tipo... É, a gente, assim... Ah, e como que, assim, essa é a minha grande dúvida, na verdade, porque o... Como utiliza-se plataformas agênticas igual o Hermes, por exemplo..."
- **q5_0**: "o Hermes? Aí, tipo É, a gente sim Ah, e como que, assim Essa é a minha grande dúvida na verdade Porque como utiliza-se plataformas agênticas igual o Hermes, por exemplo..." (pontuação e maiúsculas irregulares)
- **turbo q8_0**: (final do arquivo) "ou para o lado B e aí ele vai ou para o lado B" (repetição em loop; texto sem pontuação em todo o arquivo)

NURC, início (referência: "bom o vocês poderiam no caso falar então de início para nós né se o clima de são paulo..."):

- **q8_0**: "Estão vocês conversando, vocês falaram sobre o clima, não? É, eu fiz fato, mas vamos ver se vai."
- **q5_0**: "Estão vocês conversando? Você disse que falasse sobre o clima, não? É, eu fiz fato, mas vamos ver se vai."
- **turbo**: "Então, vocês conversando, eu queria que vocês falassem sobre o clima, não? É um pouquinho chato, mas vamos ver se vai." (aqui o turbo foi o mais fiel à referência)

NURC, passagem seguinte (~1:41): q5_0 e turbo derraparam em frases soltas ("Eu não sei se você é assim como uma espelha aqui...", turbo) e depois perderam o resto; o q8_0 manteve o fio da conversa.

Diarização no real5: speakrs deu 3 falantes, com turnos estáveis e curtos (12 a 14 turnos em 5 min). O sherpa deu **11 falantes** e 25 turnos: dividiu as mesmas pessoas em vários "falantes" (excesso de segmentação com os limiares padrão), o que deixa o resultado inutilizável sem ajuste.

## Observações

- O q5_0 não é indistinguível do q8_0: no NURC o WER subiu 7,6 pontos absolutos e a saída perdeu mais de 40% do texto; no real5 perdeu a pontuação consistente. Pela regra, fica o q8_0.
- O turbo q8_0 é mais rápido (cerca de 20% menos tempo), mas na reunião real saiu sem pontuação e com repetição em loop; no NURC perdeu texto. Não compensa.
- Sherpa: excesso de falantes (11) e nenhum ganho. Fica de fora da v1.
- Limites desta comparação: um único trecho de 5 min de cada tipo, uma execução por variante; o acerto de falante é aproximado e baixo para todos porque o NURC só foi detectado com 2 falantes (um dos três, o entrevistador, fala pouco) e porque o clipe é reconstruído. Não há referência para a reunião real, então o julgamento ali é qualitativo.

## Decisão

**`ggml-large-v3-q8_0.bin` + speakrs.** É o padrão previsto, e nenhum candidato foi melhor: o q5_0 piorou o WER e a pontuação (não é indistinguível), o turbo perdeu pontuação e repetiu, e o sherpa fragmentou os falantes. Como o repositório `ggerganov/whisper.cpp` não publica o large-v3 q8_0 (só o f16 e o q5_0), a Task 7 já baixa o f16 e quantiza localmente (port em Rust, saída byte-idêntica à do `whisper-quantize`).

## DTW (timestamps por token)

Data: 2026-10-28. Pergunta: o DTW do whisper.cpp (alignment heads, `t_dtw`) melhora a atribuição de falante nas trocas de turno, em relação aos tempos legados `t0/t1` de `set_token_timestamps(true)`?

**Como foi testado.** Protótipo em `asr.rs` (não mantido): `WhisperContextParameters::dtw_parameters` com `DtwMode::ModelPreset { LargeV3 }` (preset escolhido pelo nome do arquivo do modelo; desconhecido → sem DTW), início da palavra = `t_dtw` do primeiro token (fallback para `t0` quando `-1`), fim = `t1` (ou início da palavra seguinte quando o DTW passa do `t1`), tudo passando por `map_to_original` (VAD). Chave `TONEDIAL_DTW=0|1`. Testes unitários do preset e do fallback passaram, assim como `cargo test` inteiro e o teste ignorado `vad_timestamps_stay_on_original_timeline` (tiny, com DTW ligado). Execuções com large-v3 q8_0 + speakrs, **GPU (Vulkan, RTX 2050 livre)**, mesmo binário, só a variável mudando.

Métricas extras (além do `eval.py`): alinhamento palavra a palavra hipótese × referência (Levenshtein); cada palavra de referência casada herda o falante do turno da saída; acerto com o melhor mapeamento. "Bordas" = até 3 palavras de cada lado de uma troca de falante na referência.

| Áudio | DTW | WER | Acerto aprox. (ponto médio) | Acerto por palavra | Acerto nas bordas | Turnos | Turnos ≤2 palavras | Tempo | Pico VRAM |
|---|---|---|---|---|---|---|---|---|---|
| NURC 5 min | não | 69,5% | 53,3% | 57,2% | 48,1% (52 pal.) | 6 | 0 | 179 s | 3047 MiB |
| NURC 5 min | sim | 69,5% | 53,1% | 57,7% | 48,1% (52 pal.) | 6 | 1 | 181 s | 3588 MiB |
| real5 | não | n/a | n/a | n/a | n/a | 12 | 0 | 163 s | 3111 MiB |
| real5 | sim | n/a | n/a | n/a | n/a | 14 | 2 | 163 s | 3588 MiB |

(O texto é idêntico com e sem DTW: o DTW só muda os tempos. Na GPU o large-v3 q8_0 produziu 431 palavras no NURC contra 636 na CPU da rodada anterior, por isso o WER difere da tabela acima.)

**Bordas, olhando turno a turno.**

- NURC: as 3 trocas que mudaram foram para o lado certo (ex.: "Bom, colega" passou inteiro para o turno de quem fala, em vez de "Bom," ficar no fim do turno anterior).
- real5 (11 trocas comparáveis): 2 melhoraram (fim de frase voltou para o falante certo), 5 ficaram iguais e 4 pioraram: um nome próprio separado da sua frase, uma resposta curta jogada para o turno anterior, "pro | lado B" e "com o | Hermes?" partidos em lugar pior, e uma troca virou três fragmentos de 1 palavra (2 turnos de ≤2 palavras novos).

**Custo.** Tempo igual. VRAM: +540 MiB de pico (3,05 → 3,59 GiB de 4 GiB), por causa dos QKs de cross-attention guardados para o DTW. Além disso o DTW do whisper.cpp é marcado como experimental e usa `WHISPER_ASSERT` (aborta o processo) em vários pontos.

**Decisão: fica sem DTW.** Nas métricas de falante o ganho é ruído (+0,5 ponto por palavra, −0,2 no ponto médio, bordas iguais), na reunião real as bordas ficaram mistas, com mais fragmentos, e o custo de VRAM é alto numa placa de 4 GB. O código do DTW foi removido; os tempos continuam vindo de `t0/t1`. Se for revisitar, o problema de borda parece estar mais na diarização (segmentos que cortam no meio da frase) do que nos tempos das palavras.

Observação lateral: mesmo sem DTW, o pico de VRAM (≈3,0–3,1 GiB) passa do que o preflight reserva (modelo 1,66 GB + 1 GiB ≈ 2,7 GB); a folga `GPU_MARGIN_BYTES` talvez precise de uns 0,5 GiB a mais.
