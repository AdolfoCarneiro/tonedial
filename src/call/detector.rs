use std::time::{Duration, Instant};

const USO_ATE_PERGUNTAR: Duration = Duration::from_secs(3);
const SOLTO_ATE_FIM: Duration = Duration::from_secs(5);
const REPETIR_FIM: Duration = Duration::from_secs(300);

pub struct DetectorCtx {
    /// gravando qualquer coisa, processando ou preparando modelos
    pub busy: bool,
    pub call_recording: bool,
}

#[derive(Debug, PartialEq)]
pub enum DetectorAction {
    AskStart { app: Option<String> },
    AskEnd,
    CallBack,
}

#[derive(Default)]
pub struct Detector {
    in_use_since: Option<Instant>,
    free_since: Option<Instant>,
    asked_start: bool,
    was_call_recording: bool,
    seen_in_recording: bool,
    last_ask_end: Option<Instant>,
}

impl Detector {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn observe(
        &mut self,
        now: Instant,
        in_use: bool,
        app: Option<&str>,
        ctx: DetectorCtx,
    ) -> Option<DetectorAction> {
        // início de uma gravação de call: o que foi visto antes não vale
        if ctx.call_recording && !self.was_call_recording {
            self.seen_in_recording = false;
            self.last_ask_end = None;
        }
        self.was_call_recording = ctx.call_recording;

        if in_use {
            self.free_since = None;
            self.in_use_since.get_or_insert(now);
            if ctx.call_recording {
                self.seen_in_recording = true;
            }
        } else {
            self.free_since.get_or_insert(now);
            self.in_use_since = None;
        }
        let free_for = self.free_since.map(|t| now.duration_since(t));
        // a sessão de uso só acaba depois de 5 s solto
        if free_for.is_some_and(|d| d >= SOLTO_ATE_FIM) {
            self.asked_start = false;
        }
        // uso visto com o app ocupado: ao terminar, não pergunta pela mesma sessão
        if in_use && ctx.busy {
            self.asked_start = true;
        }

        if ctx.call_recording {
            if in_use {
                if self.last_ask_end.take().is_some() {
                    return Some(DetectorAction::CallBack);
                }
                return None;
            }
            if !self.seen_in_recording || !free_for.is_some_and(|d| d >= SOLTO_ATE_FIM) {
                return None;
            }
            let due = match self.last_ask_end {
                None => true,
                Some(t) => now.duration_since(t) >= REPETIR_FIM,
            };
            if due {
                self.last_ask_end = Some(now);
                return Some(DetectorAction::AskEnd);
            }
            return None;
        }

        if in_use && !ctx.busy && !self.asked_start {
            let since = self.in_use_since?;
            if now.duration_since(since) >= USO_ATE_PERGUNTAR {
                self.asked_start = true;
                return Some(DetectorAction::AskStart { app: app.map(str::to_string) });
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idle() -> DetectorCtx {
        DetectorCtx { busy: false, call_recording: false }
    }
    fn rec() -> DetectorCtx {
        DetectorCtx { busy: true, call_recording: true }
    }

    struct Clock(Instant);
    impl Clock {
        fn t(&self, s: u64) -> Instant {
            self.0 + Duration::from_secs(s)
        }
    }

    fn clock() -> Clock {
        Clock(Instant::now())
    }

    #[test]
    fn pergunta_depois_de_3s_de_uso() {
        let (c, mut d) = (clock(), Detector::new());
        for s in 0..3 {
            assert_eq!(d.observe(c.t(s), true, Some("Opera"), idle()), None);
        }
        assert_eq!(
            d.observe(c.t(3), true, Some("Opera"), idle()),
            Some(DetectorAction::AskStart { app: Some("Opera".into()) })
        );
    }

    #[test]
    fn pergunta_uma_vez_por_sessao_de_uso() {
        let (c, mut d) = (clock(), Detector::new());
        for s in 0..=3 {
            d.observe(c.t(s), true, Some("Opera"), idle());
        }
        for s in 4..=60 {
            assert_eq!(d.observe(c.t(s), true, Some("Opera"), idle()), None);
        }
    }

    #[test]
    fn nova_sessao_depois_de_5s_solto() {
        let (c, mut d) = (clock(), Detector::new());
        for s in 0..=3 {
            d.observe(c.t(s), true, None, idle());
        }
        for s in 10..=15 {
            assert_eq!(d.observe(c.t(s), false, None, idle()), None);
        }
        for s in 16..=18 {
            assert_eq!(d.observe(c.t(s), true, None, idle()), None);
        }
        assert_eq!(
            d.observe(c.t(19), true, None, idle()),
            Some(DetectorAction::AskStart { app: None })
        );
    }

    #[test]
    fn solto_menos_de_5s_nao_e_nova_sessao() {
        let (c, mut d) = (clock(), Detector::new());
        for s in 0..=3 {
            d.observe(c.t(s), true, None, idle());
        }
        for s in 10..=13 {
            assert_eq!(d.observe(c.t(s), false, None, idle()), None);
        }
        for s in 14..=30 {
            assert_eq!(d.observe(c.t(s), true, None, idle()), None);
        }
    }

    #[test]
    fn nao_pergunta_ocupado() {
        let (c, mut d) = (clock(), Detector::new());
        for s in 0..=30 {
            let ctx = DetectorCtx { busy: true, call_recording: false };
            assert_eq!(d.observe(c.t(s), true, None, ctx), None);
        }
    }

    #[test]
    fn uso_visto_ocupado_conta_como_ja_perguntado() {
        let (c, mut d) = (clock(), Detector::new());
        let busy = || DetectorCtx { busy: true, call_recording: false };
        for s in 0..=10 {
            assert_eq!(d.observe(c.t(s), true, Some("Opera"), busy()), None);
        }
        // terminou de processar com a mesma sessão de uso: não pergunta
        for s in 11..=60 {
            assert_eq!(d.observe(c.t(s), true, Some("Opera"), idle()), None);
        }
        // soltou de verdade e pegou de novo: sessão nova
        for s in 61..=66 {
            d.observe(c.t(s), false, None, idle());
        }
        for s in 67..=69 {
            assert_eq!(d.observe(c.t(s), true, None, idle()), None);
        }
        assert_eq!(d.observe(c.t(70), true, None, idle()), Some(DetectorAction::AskStart { app: None }));
    }

    #[test]
    fn pergunta_fim_so_depois_de_ver_o_mic_na_gravacao() {
        let (c, mut d) = (clock(), Detector::new());
        for s in 0..=600 {
            assert_eq!(d.observe(c.t(s), false, None, rec()), None);
        }
    }

    #[test]
    fn pergunta_fim_5s_depois_de_soltar_e_repete_em_5min() {
        let (c, mut d) = (clock(), Detector::new());
        for s in 0..30 {
            assert_eq!(d.observe(c.t(s), true, None, rec()), None);
        }
        for s in 30..35 {
            assert_eq!(d.observe(c.t(s), false, None, rec()), None);
        }
        assert_eq!(d.observe(c.t(35), false, None, rec()), Some(DetectorAction::AskEnd));
        for s in 36..=334 {
            assert_eq!(d.observe(c.t(s), false, None, rec()), None);
        }
        assert_eq!(d.observe(c.t(335), false, None, rec()), Some(DetectorAction::AskEnd));
    }

    #[test]
    fn call_volta_depois_da_pergunta() {
        let (c, mut d) = (clock(), Detector::new());
        for s in 0..30 {
            d.observe(c.t(s), true, None, rec());
        }
        for s in 30..=35 {
            d.observe(c.t(s), false, None, rec());
        }
        assert_eq!(d.observe(c.t(40), true, None, rec()), Some(DetectorAction::CallBack));
        assert_eq!(d.observe(c.t(41), true, None, rec()), None);
    }

    #[test]
    fn gravacao_nova_zera_o_visto() {
        let (c, mut d) = (clock(), Detector::new());
        for s in 0..10 {
            d.observe(c.t(s), true, None, rec());
        }
        d.observe(c.t(10), true, None, idle());
        for s in 11..=600 {
            assert_eq!(d.observe(c.t(s), false, None, idle()), None);
        }
        for s in 601..=1200 {
            assert_eq!(d.observe(c.t(s), false, None, rec()), None);
        }
    }
}
