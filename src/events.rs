//! Eventos que chegam ao `App` de fora da janela (outra instância, bandeja, detector).

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
