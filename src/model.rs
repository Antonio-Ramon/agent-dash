//! Tipos de dados compartilhados entre o coletor (thread de fundo) e a UI.

use chrono::{DateTime, Local, NaiveDate};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Nome do agente do Claude Code nos totais de uso (os outros usam o nome
/// de `agents.rs`, ex.: "Antigravity").
pub const CLAUDE: &str = "Claude";

/// Ordem de exibição dos agentes nos totais: Claude, depois os demais por nome.
pub fn agent_order(a: &str, b: &str) -> std::cmp::Ordering {
    (a != CLAUDE).cmp(&(b != CLAUDE)).then_with(|| a.cmp(b))
}

/// Contagem de tokens de uma ou mais respostas da API.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tokens {
    pub input: u64,
    pub cache_write: u64,
    pub cache_read: u64,
    pub output: u64,
}

impl Tokens {
    /// Soma de tudo que passou pela API (o que conta para limites de uso).
    pub fn total(&self) -> u64 {
        self.input + self.cache_write + self.cache_read + self.output
    }

    /// Tamanho do contexto enviado num turno (entrada + cache).
    pub fn context(&self) -> u64 {
        self.input + self.cache_write + self.cache_read
    }

    pub fn add(&mut self, o: &Tokens) {
        self.input += o.input;
        self.cache_write += o.cache_write;
        self.cache_read += o.cache_read;
        self.output += o.output;
    }

    pub fn sub(&mut self, o: &Tokens) {
        self.input = self.input.saturating_sub(o.input);
        self.cache_write = self.cache_write.saturating_sub(o.cache_write);
        self.cache_read = self.cache_read.saturating_sub(o.cache_read);
        self.output = self.output.saturating_sub(o.output);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskKind {
    Shell,
    Monitor,
    Workflow,
    Agent,
}

impl TaskKind {
    pub fn label(&self) -> &'static str {
        match self {
            TaskKind::Shell => "shell",
            TaskKind::Monitor => "monitor",
            TaskKind::Workflow => "workflow",
            TaskKind::Agent => crate::i18n::t("agente", "agent"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskStatus {
    Running,
    /// Terminou; guarda o status informado (completed, killed, failed…).
    Done(String),
    /// Sem notificação de término, mas a sessão dona já morreu.
    Orphan,
}

#[derive(Debug, Clone)]
pub struct TaskInfo {
    pub id: String,
    pub kind: TaskKind,
    pub description: String,
    pub command: Option<String>,
    pub status: TaskStatus,
    pub started: Option<DateTime<Local>>,
    pub ended: Option<DateTime<Local>>,
    pub output_file: Option<PathBuf>,
    pub session_id: String,
    pub session_name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    Prompt,
    Tool,
    Text,
    Notice,
}

#[derive(Debug, Clone)]
pub struct Event {
    pub ts: Option<DateTime<Local>>,
    pub kind: EventKind,
    pub text: String,
}

/// Uso agregado de um transcript (sessão principal ou subagente).
#[derive(Debug, Clone, Default)]
pub struct Usage {
    pub tokens: Tokens,
    pub by_model: BTreeMap<String, Tokens>,
    /// Tokens da resposta mais recente (usado para "contexto atual").
    pub last_turn: Tokens,
    pub turns: u32,
    pub model: Option<String>,
    pub tool_counts: BTreeMap<String, u32>,
    /// Minuto unix -> tokens totais naquele minuto.
    pub per_minute: BTreeMap<i64, u64>,
    pub first_ts: Option<DateTime<Local>>,
    pub last_ts: Option<DateTime<Local>>,
}

#[derive(Debug, Clone)]
pub struct AgentInfo {
    pub agent_id: String,
    pub agent_type: String,
    pub description: String,
    pub workflow: Option<String>,
    pub session_id: String,
    pub session_name: String,
    pub running: bool,
    pub usage: Usage,
    pub events: Vec<Event>,
    pub last_activity: Option<DateTime<Local>>,
}

#[derive(Debug, Clone)]
pub struct ProcInfo {
    pub pid: u32,
    pub name: String,
    pub cpu: f32,
    pub mem: u64,
    pub cmd: String,
    pub depth: usize,
}

#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub pid: Option<u32>,
    pub session_id: String,
    pub name: String,
    pub cwd: String,
    /// idle | busy | shell | encerrada …
    pub status: String,
    /// interactive | background | …
    pub kind: String,
    /// Agente da sessão quando não é o Claude Code (ex.: "Antigravity").
    /// Nessas sessões não há tokens, subagentes nem tasks.
    pub agent: Option<String>,
    /// Id curto de sessão em background (`claude agents`), para stop/attach/rm.
    pub bg_id: Option<String>,
    /// Sessão em background parada esperando resposta: a pergunta dela.
    pub pending: Option<String>,
    pub version: String,
    pub started_at: Option<DateTime<Local>>,
    pub alive: bool,
    pub title: Option<String>,
    pub last_prompt: Option<String>,
    pub usage: Usage,
    /// Uso somado dos subagentes desta sessão.
    pub agents_usage: Tokens,
    pub events: Vec<Event>,
    pub transcript: Option<PathBuf>,
    pub cpu: f32,
    pub mem: u64,
    pub procs: Vec<ProcInfo>,
    pub agents_total: usize,
    pub agents_running: usize,
    pub tasks_running: usize,
}

#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub sessions: Vec<SessionInfo>,
    pub agents: Vec<AgentInfo>,
    pub tasks: Vec<TaskInfo>,
    /// Uso por dia somando todos os agentes.
    pub daily: BTreeMap<NaiveDate, Tokens>,
    /// Uso por dia de cada agente ("Claude", "Antigravity"…).
    pub daily_by_agent: BTreeMap<NaiveDate, BTreeMap<String, Tokens>>,
    pub daily_by_model: BTreeMap<NaiveDate, BTreeMap<String, Tokens>>,
    /// Modelo -> agente que o usou (para agrupar e filtrar a tabela de modelos).
    pub model_agent: BTreeMap<String, String>,
    /// Sessões com consumo no dia, por agente.
    pub daily_sessions: BTreeMap<NaiveDate, BTreeMap<String, usize>>,
    /// Tokens por minuto na última hora, todas as sessões (60 posições).
    pub per_minute: Vec<u64>,
    /// O mesmo, separado por agente.
    pub per_minute_by_agent: BTreeMap<String, Vec<u64>>,
    /// Bytes de histórico já lidos / total.
    pub history_done: u64,
    pub history_total: u64,
    pub loaded: bool,
    /// Diretórios onde já houve sessões (para abrir uma instância nova).
    pub repos: Vec<String>,
    /// Mensagens das ações executadas desde o último snapshot.
    pub notices: Vec<String>,
}
