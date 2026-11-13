//! Estado da UI e tratamento de teclas.

use crate::actions::{Action, LaunchSpec};
use crate::agents::{self, AgentDef};
use crate::collector::Ctl;
use crate::i18n::{t, tr};
use crate::model::*;
use crate::term::{self, Term};
use chrono::{Local, NaiveDate};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Position, Rect};
use ratatui::widgets::ListState;
use regex::Regex;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::collections::HashSet;
use std::sync::mpsc::Sender;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// Agentes e tasks terminados continuam visíveis por este tempo (sem `a`).
const RECENT_DONE_MIN: i64 = 30;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Panel {
    Sessions = 0,
    Agents = 1,
    Tasks = 2,
    Usage = 3,
}

impl Panel {
    pub const ALL: [Panel; 4] = [Panel::Sessions, Panel::Agents, Panel::Tasks, Panel::Usage];

    pub fn idx(self) -> usize {
        self as usize
    }

    pub fn tabs(self) -> &'static [&'static str] {
        let en = crate::i18n::en();
        match self {
            Panel::Sessions if en => &["Summary", "Activity", "Processes", "Chat"],
            Panel::Sessions => &["Resumo", "Atividade", "Processos", "Chat"],
            Panel::Agents if en => &["Summary", "Activity"],
            Panel::Agents => &["Resumo", "Atividade"],
            Panel::Tasks if en => &["Output", "Details"],
            Panel::Tasks => &["Saída", "Detalhes"],
            Panel::Usage if en => &["History"],
            Panel::Usage => &["Histórico"],
        }
    }
}

pub struct TailCache {
    pub path: PathBuf,
    pub len: u64,
    pub lines: Vec<String>,
}

/// Pedido de confirmação para uma ação destrutiva (y/Enter confirma).
pub struct Confirm {
    pub title: String,
    pub body: Vec<String>,
    pub action: Option<Action>,
    /// Confirmar também fecha o dashboard.
    pub quit: bool,
    /// Itens a limpar da lista se a ação for confirmada.
    pub hide: Vec<String>,
    /// Chats a reabrir se confirmado (pergunta feita ao abrir o dashboard).
    pub restore: Vec<SavedChat>,
    /// Chat embutido de outro agente a fechar se confirmado.
    pub kill_chat: Option<String>,
}

/// `kind` das sessões de outros agentes (Antigravity, Codex…) que rodam nos chats
/// embutidos. Elas não vêm do `~/.claude`: são montadas a partir dos chats.
pub const EXTERNAL_KIND: &str = "externo";

/// Chat embutido que estava aberto; fica em disco enquanto o chat existir,
/// para ser oferecido de volta se o dashboard fechar com ele aberto.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct SavedChat {
    pub session_id: String,
    pub title: String,
    pub cwd: String,
    /// Agente da conversa quando não é o Claude (ex.: "Antigravity").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
}

/// Formulário "nova instância".
pub struct LaunchForm {
    /// Ver as constantes `F_*`.
    pub field: usize,
    pub repo: String,
    pub sug: usize,
    /// Índice em `App::agents` (0 = Claude).
    pub agent: usize,
    /// Onde cada agente está instalado (None = não achei no PATH).
    pub installed: Vec<Option<PathBuf>>,
    pub model: usize,
    /// 0 aqui no painel · 1 aba nova do terminal · 2 background (só Claude)
    pub mode: usize,
    pub name: String,
    pub prompt: String,
    pub error: Option<String>,
}

pub const F_REPO: usize = 0;
pub const F_AGENT: usize = 1;
pub const F_MODEL: usize = 2;
pub const F_MODE: usize = 3;
pub const F_NAME: usize = 4;
pub const F_PROMPT: usize = 5;
pub const LAUNCH_FIELDS: usize = 6;

impl LaunchForm {
    /// Quantos modos de abrir valem para o agente (o background só existe no Claude Code).
    pub fn modes(&self, agents: &[AgentDef]) -> usize {
        if agents.get(self.agent).is_some_and(|a| a.is_claude()) { 3 } else { 2 }
    }
}

/// Seleção de texto feita com o mouse no chat embutido. Pontos em
/// (linha, coluna) como em `term::text_between`.
pub struct ChatSel {
    pub key: String,
    pub anchor: (i64, u16),
    pub head: (i64, u16),
    /// Botão ainda apertado.
    pub dragging: bool,
}

impl LaunchForm {
    /// Repositórios que combinam com o texto digitado (o igual primeiro).
    pub fn suggestions<'a>(&self, repos: &'a [String]) -> Vec<&'a String> {
        let q = self.repo.trim().to_lowercase();
        // Caminho igual a um conhecido (ex.: pré-preenchido): mostra todos, ele primeiro.
        let exact = repos.iter().any(|r| r.to_lowercase() == q);
        let mut v: Vec<&String> = repos.iter().filter(|r| q.is_empty() || exact || r.to_lowercase().contains(&q)).collect();
        v.sort_by_key(|r| r.to_lowercase() != q);
        v
    }

    /// Diretório escolhido: a sugestão destacada ou o caminho digitado.
    pub fn chosen(&self, repos: &[String]) -> Option<String> {
        let sug = self.suggestions(repos);
        if let Some(r) = sug.get(self.sug.min(sug.len().saturating_sub(1))) {
            return Some((*r).clone());
        }
        let t = self.repo.trim();
        if !t.is_empty() && Path::new(t).is_dir() { Some(t.to_string()) } else { None }
    }
}

pub struct App {
    pub snap: Snapshot,
    pub focus: Panel,
    pub states: [ListState; 4],
    sel_keys: [Option<String>; 4],
    /// Primeira linha visível de cada lista da esquerda. A roda do mouse mexe
    /// só aqui; a seleção fica onde está.
    pub offsets: [usize; 4],
    /// Pedido para o próximo desenho rolar a lista até o item selecionado.
    pub reveal: [bool; 4],
    /// O item selecionado estava na tela no último desenho. Se estava, a lista
    /// o acompanha quando reordena; se a roda o tirou de vista, fica como está.
    pub sel_visible: [bool; 4],
    /// Último painel rolado pela roda: mostra a barra mesmo sem foco.
    pub wheel: Option<Panel>,
    /// Onde cada painel foi desenhado por último (para saber o que está sob o mouse).
    pub rects: [Rect; 4],
    pub main_rect: Rect,
    /// Área da tela do chat no último desenho (vazia se não há chat na tela).
    pub chat_rect: Rect,
    pub chat_sel: Option<ChatSel>,
    /// `a`: inclui sessões encerradas e agentes/tasks antigos (últimas 24h).
    pub show_all: bool,
    /// `f`: agentes/tasks só da sessão selecionada.
    pub only_selected: bool,
    /// `v`: uso de tokens de um agente só (`None` = todos somados).
    pub usage_filter: Option<String>,
    pub zoom: bool,
    pub help: bool,
    pub tab: usize,
    /// Rolagem do painel principal (na aba Saída conta a partir do fim).
    pub scroll: u16,
    pub status: Option<(String, Instant)>,
    pub quit: bool,
    pub tail: Option<TailCache>,
    pub confirm: Option<Confirm>,
    pub launcher: Option<LaunchForm>,
    /// Chats embutidos (um `claude` por pseudo-terminal).
    pub terms: Vec<Term>,
    /// O teclado está indo para o chat `chat_key`.
    pub chat_focus: bool,
    pub chat_key: Option<String>,
    /// Sessão recém-criada aqui, a selecionar assim que aparecer na lista.
    pending_sel: Option<String>,
    /// Tamanho (linhas, colunas) da área do chat no último desenho.
    pub chat_size: (u16, u16),
    /// Itens limpos da lista (`d`/`D`); voltam com `a` ou `u`.
    hidden: HashSet<String>,
    last_hidden: Vec<String>,
    /// Agentes que dá para abrir pelo `n` (o Claude primeiro).
    pub agents: Vec<AgentDef>,
}

/// Pasta do estado local (itens ocultos, chats abertos). Nos testes é uma
/// pasta temporária, para não mexer no estado de quem roda `cargo test`.
fn state_dir() -> Option<PathBuf> {
    if cfg!(test) {
        // Uma pasta por teste (cada teste roda numa thread): testes em paralelo
        // não apagam o estado uns dos outros.
        let thread = format!("{:?}", std::thread::current().id()).replace(|c: char| !c.is_ascii_alphanumeric(), "");
        return Some(std::env::temp_dir().join(format!("agent-dash-teste-estado-{}-{thread}", std::process::id())));
    }
    dirs::data_local_dir().map(|d| d.join("agent-dash"))
}

fn hidden_path() -> Option<PathBuf> {
    state_dir().map(|d| d.join("hidden.json"))
}

fn open_chats_path() -> Option<PathBuf> {
    state_dir().map(|d| d.join("open_chats.json"))
}

fn load_hidden() -> HashSet<String> {
    hidden_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str::<Vec<String>>(&t).ok())
        .map(|v| v.into_iter().collect())
        .unwrap_or_default()
}

fn recent(ts: Option<chrono::DateTime<Local>>) -> bool {
    ts.is_some_and(|t| Local::now() - t < chrono::Duration::minutes(RECENT_DONE_MIN))
}

fn ansi_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(\x07|\x1b\\)|\x1b[@-Z\\-_]").unwrap())
}

impl App {
    pub fn new() -> Self {
        let (agents, agents_err) = agents::load();
        Self {
            snap: Snapshot::default(),
            focus: Panel::Sessions,
            states: Default::default(),
            sel_keys: Default::default(),
            offsets: [0; 4],
            reveal: [true; 4],
            sel_visible: [true; 4],
            wheel: None,
            rects: [Rect::default(); 4],
            main_rect: Rect::default(),
            chat_rect: Rect::default(),
            chat_sel: None,
            show_all: false,
            only_selected: false,
            usage_filter: None,
            zoom: false,
            help: false,
            tab: 0,
            scroll: 0,
            status: agents_err.map(|e| (e, Instant::now())),
            quit: false,
            tail: None,
            confirm: None,
            launcher: None,
            terms: Vec::new(),
            chat_focus: false,
            chat_key: None,
            pending_sel: None,
            chat_size: (30, 100),
            hidden: load_hidden(),
            last_hidden: Vec::new(),
            agents,
        }
    }

    /// Grava quais chats embutidos estão abertos agora. É chamado a cada
    /// mudança (e não só ao sair), para valer também quando a janela do
    /// terminal é fechada ou o processo é morto.
    fn save_open_chats(&self) {
        let Some(p) = open_chats_path() else { return };
        let v: Vec<SavedChat> = self
            .terms
            .iter()
            .filter(|t| t.restorable)
            .map(|t| SavedChat { session_id: t.key.clone(), title: t.title.clone(), cwd: t.cwd.clone(), agent: t.agent.clone() })
            .collect();
        if v.is_empty() {
            let _ = std::fs::remove_file(p);
            return;
        }
        let _ = std::fs::create_dir_all(p.parent().unwrap());
        if let Ok(t) = serde_json::to_string_pretty(&v) {
            let _ = std::fs::write(p, t);
        }
    }

    /// Ao abrir: se da última vez o dashboard fechou com chats abertos,
    /// pergunta se é para reabri-los.
    pub fn offer_restore(&mut self) {
        let saved: Vec<SavedChat> = open_chats_path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        if saved.is_empty() {
            return;
        }
        let n = saved.len();
        let mut body: Vec<String> = saved.iter().map(|c| format!("• {} — {}", c.title, c.cwd)).collect();
        body.push(t("Estavam abertas aqui quando o dashboard fechou da última vez.", "They were open here when the dashboard last closed.").into());
        body.push(t("y retoma cada conversa de onde parou; n descarta esta lista.", "y resumes each conversation where it stopped; n discards this list.").into());
        self.confirm = Some(Confirm {
            title: match n {
                1 => t("Reabrir a sessão que ficou aberta", "Reopen the session left open").into(),
                _ => tr!("Reabrir as {n} sessões que ficaram abertas", "Reopen the {n} sessions left open"),
            },
            body,
            action: None,
            quit: false,
            hide: Vec::new(),
            restore: saved,
            kill_chat: None,
        });
    }

    fn restore_chats(&mut self, saved: &[SavedChat]) {
        let before = self.terms.len();
        let mut failed = Vec::new();
        for c in saved {
            // Ainda rodando em algum lugar (outra aba, outro dashboard): retomar
            // poria dois processos na mesma conversa.
            if self.snap.sessions.iter().any(|s| s.session_id == c.session_id && s.alive) {
                failed.push(tr!("{} (ainda está rodando)", "{} (still running)", c.title));
                continue;
            }
            let n = self.terms.len();
            match &c.agent {
                Some(agent) => self.resume_agent_chat(agent, &c.session_id, c.title.clone(), &c.cwd),
                None => self.spawn_chat(c.session_id.clone(), c.title.clone(), &c.cwd, &["--resume".to_string(), c.session_id.clone()]),
            }
            if self.terms.len() == n {
                failed.push(c.title.clone());
            }
        }
        // Reabrir não toma o teclado: Enter entra no chat que interessar.
        self.chat_focus = false;
        self.save_open_chats();
        let ok = self.terms.len() - before;
        if failed.is_empty() {
            self.flash(tr!("{ok} sessão(ões) reaberta(s) · Enter entra no chat", "{ok} session(s) reopened · Enter goes into the chat"));
        } else {
            self.flash(tr!("{ok} reaberta(s); não consegui reabrir: {}", "{ok} reopened; could not reopen: {}", failed.join(", ")));
        }
    }

    fn save_hidden(&self) {
        let Some(p) = hidden_path() else { return };
        let _ = std::fs::create_dir_all(p.parent().unwrap());
        let mut v: Vec<&String> = self.hidden.iter().collect();
        v.sort();
        let _ = std::fs::write(p, serde_json::to_string(&v).unwrap_or_default());
    }

    fn is_hidden(&self, key: &str) -> bool {
        !self.show_all && self.hidden.contains(key)
    }

    // ---------- listas visíveis ----------

    pub fn sessions(&self) -> Vec<&SessionInfo> {
        self.snap
            .sessions
            .iter()
            // Viva nunca fica oculta: uma sessão limpa com `d` pode voltar com /resume.
            .filter(|s| (self.show_all || s.alive || s.status == "pendente") && (s.alive || !self.is_hidden(&format!("s:{}", s.session_id))))
            .collect()
    }

    pub fn selected_session(&self) -> Option<&SessionInfo> {
        let v = self.sessions();
        self.states[0].selected().and_then(|i| v.get(i).copied())
    }

    fn session_filter(&self) -> Option<String> {
        if self.only_selected { self.selected_session().map(|s| s.session_id.clone()) } else { None }
    }

    pub fn agents(&self) -> Vec<&AgentInfo> {
        let sel = self.session_filter();
        self.snap
            .agents
            .iter()
            .filter(|a| match &sel {
                Some(id) => &a.session_id == id,
                None => self.show_all || a.running || recent(a.last_activity),
            })
            .filter(|a| a.running || !self.is_hidden(&format!("a:{}", a.agent_id)))
            .collect()
    }

    pub fn tasks(&self) -> Vec<&TaskInfo> {
        let sel = self.session_filter();
        self.snap
            .tasks
            .iter()
            .filter(|t| match &sel {
                Some(id) => &t.session_id == id,
                None => self.show_all || t.status == TaskStatus::Running || recent(t.ended.or(t.started)),
            })
            .filter(|t| t.status == TaskStatus::Running || !self.is_hidden(&format!("t:{}:{}", t.session_id, t.id)))
            .collect()
    }

    /// Dias com uso (mais recente primeiro), já com o filtro de agente (`v`).
    pub fn days(&self) -> Vec<(NaiveDate, Tokens)> {
        match &self.usage_filter {
            None => self.snap.daily.iter().rev().map(|(d, t)| (*d, *t)).collect(),
            Some(a) => self.snap.daily_by_agent.iter().rev().filter_map(|(d, m)| Some((*d, *m.get(a)?))).collect(),
        }
    }

    /// Agentes que já tiveram consumo, na ordem de exibição.
    pub fn usage_agents(&self) -> Vec<String> {
        let mut v: Vec<String> = self.snap.daily_by_agent.values().flat_map(|m| m.keys().cloned()).collect();
        v.sort_by(|a, b| agent_order(a, b));
        v.dedup();
        v
    }

    /// `v`: todos -> cada agente -> todos.
    fn cycle_usage_filter(&mut self) {
        let agents = self.usage_agents();
        let next = match &self.usage_filter {
            None => agents.first().cloned(),
            Some(cur) => agents.iter().position(|a| a == cur).and_then(|i| agents.get(i + 1)).cloned(),
        };
        self.usage_filter = next;
        self.states[3].select(Some(0));
        self.flash(match &self.usage_filter {
            None => t("uso de tokens: todos os agentes", "token usage: all agents").to_string(),
            Some(a) => tr!("uso de tokens: só {a}", "token usage: {a} only"),
        });
    }

    /// Agente passa no filtro de uso (`v`)?
    pub fn usage_shows(&self, agent: &str) -> bool {
        self.usage_filter.as_deref().is_none_or(|f| f == agent)
    }

    pub fn selected_agent(&self) -> Option<&AgentInfo> {
        let v = self.agents();
        self.states[1].selected().and_then(|i| v.get(i).copied())
    }

    pub fn selected_task(&self) -> Option<&TaskInfo> {
        let v = self.tasks();
        self.states[2].selected().and_then(|i| v.get(i).copied())
    }

    pub fn selected_day(&self) -> Option<(NaiveDate, Tokens)> {
        let v = self.days();
        self.states[3].selected().and_then(|i| v.get(i).copied())
    }

    fn keys(&self, p: Panel) -> Vec<String> {
        match p {
            Panel::Sessions => self.sessions().iter().map(|s| s.session_id.clone()).collect(),
            Panel::Agents => self.agents().iter().map(|a| a.agent_id.clone()).collect(),
            Panel::Tasks => self.tasks().iter().map(|t| format!("{}:{}", t.session_id, t.id)).collect(),
            Panel::Usage => self.days().iter().map(|(d, _)| d.to_string()).collect(),
        }
    }

    /// Mantém a seleção no mesmo item (por id) quando a lista muda.
    fn resync(&mut self) {
        for p in Panel::ALL {
            let keys = self.keys(p);
            let i = p.idx();
            let mut reveal = false;
            // Uma sessão nova leva um instante para aparecer; quando aparece, vira a seleção.
            if p == Panel::Sessions
                && let Some(k) = &self.pending_sel
                && keys.contains(k) {
                    self.sel_keys[i] = self.pending_sel.take();
                    reveal = true;
                }
            if keys.is_empty() {
                self.states[i].select(None);
                continue;
            }
            let by_key = self.sel_keys[i].as_ref().and_then(|k| keys.iter().position(|x| x == k));
            let idx = by_key.or_else(|| self.states[i].selected().map(|s| s.min(keys.len() - 1))).unwrap_or(0);
            self.states[i].select(Some(idx));
            self.sel_keys[i] = Some(keys[idx].clone());
            if reveal || self.sel_visible[i] {
                self.reveal[i] = true;
            }
        }
    }

    /// Linhas de lista que cabem no painel `p` (sem a moldura).
    fn list_height(&self, p: Panel) -> usize {
        self.rects[p.idx()].height.saturating_sub(2) as usize
    }

    pub fn set_snapshot(&mut self, s: Snapshot) {
        self.snap = s;
        self.rekey_terms();
        self.merge_external();
        self.resync();
    }

    /// Põe na lista de sessões os chats de outros agentes que o coletor ainda
    /// não enxerga: um Codex (que o coletor não lê) ou um Antigravity antes da
    /// primeira mensagem (a conversa só nasce aí). Quando o coletor acha a
    /// conversa, a entrada daqui sai e fica a dele (ver `rekey_terms`).
    fn merge_external(&mut self) {
        self.snap.sessions.retain(|s| s.kind != EXTERNAL_KIND);
        let known: Vec<(String, Option<u32>)> = self.snap.sessions.iter().map(|s| (s.session_id.clone(), s.pid)).collect();
        let ext: Vec<SessionInfo> = self
            .terms
            .iter()
            .filter(|t| !known.iter().any(|(id, pid)| *id == t.key || (pid.is_some() && *pid == t.pid())))
            .filter_map(|t| {
                let agent = t.agent.clone()?;
                Some(SessionInfo {
                    pid: t.pid(),
                    session_id: t.key.clone(),
                    name: t.title.clone(),
                    cwd: t.cwd.clone(),
                    // O dashboard não sabe se ele está trabalhando ou esperando.
                    status: "aberta".into(),
                    kind: EXTERNAL_KIND.into(),
                    agent: Some(agent),
                    bg_id: None,
                    pending: None,
                    version: String::new(),
                    started_at: Some(t.started),
                    alive: true,
                    title: Some(t.command_line.clone()),
                    last_prompt: None,
                    usage: Usage::default(),
                    agents_usage: Tokens::default(),
                    events: Vec::new(),
                    transcript: None,
                    cpu: 0.0,
                    mem: 0,
                    procs: Vec::new(),
                    agents_total: 0,
                    agents_running: 0,
                    tasks_running: 0,
                })
            })
            .collect();
        self.snap.sessions.splice(0..0, ext);
    }

    /// O Claude Code troca o id da sessão em alguns momentos (`/clear`, por
    /// exemplo). O chat embutido é reconhecido pelo PID do processo, e a chave
    /// dele acompanha o id atual; assim ele não vira "sessão de fora".
    ///
    /// Vale também para outros agentes: um Antigravity aberto aqui ganha o id
    /// da conversa quando o coletor a descobre, e daí em diante pode ser
    /// reaberto como o Claude.
    fn rekey_terms(&mut self) {
        for i in 0..self.terms.len() {
            let Some(pid) = self.terms[i].pid() else { continue };
            let agent = self.terms[i].agent.clone();
            let Some(s) = self.snap.sessions.iter().find(|s| s.alive && s.pid == Some(pid) && s.agent == agent && s.kind != EXTERNAL_KIND) else {
                continue;
            };
            if !s.name.is_empty() {
                self.terms[i].title = s.name.clone();
            }
            if s.session_id == self.terms[i].key {
                continue;
            }
            if let Some(a) = &agent {
                // Id provisório (`agy-pid-…`) ainda não é uma conversa: não dá para reabrir.
                let real = !s.session_id.starts_with(crate::antigravity::NEW_PREFIX);
                self.terms[i].restorable = real && self.agent_def(a).is_some_and(|d| d.resume_flag.is_some());
            }
            let (old, new) = (self.terms[i].key.clone(), s.session_id.clone());
            for slot in [&mut self.chat_key, &mut self.pending_sel, &mut self.sel_keys[0]] {
                if slot.as_deref() == Some(&old) {
                    *slot = Some(new.clone());
                }
            }
            self.terms[i].key = new;
            self.save_open_chats();
        }
    }

    fn move_sel(&mut self, delta: isize) {
        let p = self.focus;
        let keys = self.keys(p);
        if keys.is_empty() {
            return;
        }
        let cur = self.states[p.idx()].selected().unwrap_or(0) as isize;
        let next = (cur + delta).clamp(0, keys.len() as isize - 1) as usize;
        self.select_idx(p, next);
    }

    /// Seleciona o item `next` do painel em foco `p`.
    fn select_idx(&mut self, p: Panel, next: usize) {
        let keys = self.keys(p);
        if next >= keys.len() {
            return;
        }
        self.states[p.idx()].select(Some(next));
        self.sel_keys[p.idx()] = Some(keys[next].clone());
        self.reveal[p.idx()] = true;
        self.scroll = 0;
        if p == Panel::Sessions {
            // Sessão com chat aberto aqui mostra o chat; as outras, o resumo.
            let has_chat = self.term_idx(&keys[next]).is_some();
            if has_chat {
                self.tab = 3;
            } else if self.tab == 3 {
                self.tab = 0;
            }
            if self.only_selected {
                self.resync();
            }
        }
    }

    fn panel_empty(&self, p: Panel) -> bool {
        match p {
            Panel::Agents => self.agents().is_empty(),
            Panel::Tasks => self.tasks().is_empty(),
            _ => false,
        }
    }

    fn go_panel(&mut self, p: Panel) {
        if self.panel_empty(p) && self.focus != p {
            let what = if p == Panel::Agents { t("nenhum agente", "no agent") } else { t("nenhuma task", "no task") };
            self.flash(tr!("{what} no momento (a mostra os terminados nas últimas 24h)", "{what} right now (a shows the ones finished in the last 24h)"));
            return;
        }
        self.set_focus(p);
    }

    /// Próximo painel (step 1) ou anterior (step 3), pulando os vazios.
    fn cycle_panel(&mut self, step: usize) {
        let mut i = self.focus.idx();
        for _ in 0..4 {
            i = (i + step) % 4;
            if !self.panel_empty(Panel::ALL[i]) {
                break;
            }
        }
        self.set_focus(Panel::ALL[i]);
    }

    fn set_focus(&mut self, p: Panel) {
        if self.focus != p {
            self.focus = p;
            self.tab = 0;
            self.scroll = 0;
        }
    }

    pub fn flash(&mut self, msg: impl Into<String>) {
        self.status = Some((msg.into(), Instant::now()));
    }

    pub fn status_text(&self) -> Option<&str> {
        self.status
            .as_ref()
            .filter(|(_, t)| t.elapsed() < Duration::from_secs(4))
            .map(|(m, _)| m.as_str())
    }

    // ---------- teclas ----------

    pub fn on_key(&mut self, k: KeyEvent, ctl: &Sender<Ctl>) {
        self.wheel = None;
        if self.confirm.is_some() {
            match k.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Char('s') | KeyCode::Char('S') | KeyCode::Enter => {
                    let c = self.confirm.take().unwrap();
                    if !c.restore.is_empty() {
                        self.restore_chats(&c.restore);
                        return;
                    }
                    if c.quit {
                        self.quit = true;
                        return;
                    }
                    if let Some(key) = &c.kill_chat {
                        // O chat sai da lista no próximo ciclo (reap_terms).
                        if let Some(i) = self.term_idx(key) {
                            self.terms[i].kill();
                        }
                    }
                    if !c.hide.is_empty() {
                        self.hidden.extend(c.hide.iter().cloned());
                        self.last_hidden = c.hide.clone();
                        self.save_hidden();
                        self.resync();
                    }
                    self.flash(format!("{}…", c.title));
                    if let Some(a) = c.action {
                        let _ = ctl.send(Ctl::Act(a));
                    }
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc | KeyCode::Char('q') => {
                    let c = self.confirm.take().unwrap();
                    if c.restore.is_empty() {
                        self.flash(t("cancelado", "cancelled"));
                    } else {
                        // Recusou: esquece a lista (grava a atual, vazia).
                        self.save_open_chats();
                        self.flash(t(
                        "sessões não reabertas · a mostra as encerradas, Enter retoma uma delas",
                        "sessions not reopened · a shows the ended ones, Enter resumes one of them",
                    ));
                    }
                }
                _ => {}
            }
            return;
        }
        if self.launcher.is_some() {
            self.launcher_key(k, ctl);
            return;
        }
        if self.chat_focus {
            self.chat_key_event(k);
            return;
        }
        if self.help {
            if matches!(k.code, KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q') | KeyCode::Enter) {
                self.help = false;
            }
            return;
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match k.code {
            KeyCode::Char('c') if ctrl => self.ask_quit(),
            KeyCode::Char('q') => self.ask_quit(),
            KeyCode::Enter if self.focus == Panel::Sessions => self.enter_chat(),
            KeyCode::Esc => {
                if self.zoom {
                    self.zoom = false;
                }
            }
            KeyCode::Char(c @ '1'..='4') => self.go_panel(Panel::ALL[(c as u8 - b'1') as usize]),
            KeyCode::Tab | KeyCode::Char('l') | KeyCode::Right => {
                self.cycle_panel(1);
            }
            KeyCode::BackTab | KeyCode::Char('h') | KeyCode::Left => {
                self.cycle_panel(3);
            }
            KeyCode::Down | KeyCode::Char('j') => self.move_sel(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_sel(-1),
            KeyCode::Home | KeyCode::Char('g') => self.move_sel(-100_000),
            KeyCode::End | KeyCode::Char('G') => self.move_sel(100_000),
            KeyCode::Char('d') if ctrl => self.scroll = self.scroll.saturating_add(10),
            KeyCode::Char('u') if ctrl => self.scroll = self.scroll.saturating_sub(10),
            KeyCode::PageDown | KeyCode::Char('J') => self.scroll_main(10),
            KeyCode::PageUp | KeyCode::Char('K') => self.scroll_main(-10),
            KeyCode::Char(']') => {
                self.tab = (self.tab + 1) % self.focus.tabs().len();
                self.scroll = 0;
            }
            KeyCode::Char('[') => {
                let n = self.focus.tabs().len();
                self.tab = (self.tab + n - 1) % n;
                self.scroll = 0;
            }
            KeyCode::Enter | KeyCode::Char('+') | KeyCode::Char('z') => self.zoom = !self.zoom,
            KeyCode::Char('a') => {
                self.show_all = !self.show_all;
                self.resync();
                self.flash(match self.show_all {
                    true => t("mostrando tudo (últimas 24h)", "showing everything (last 24h)"),
                    false => t("mostrando só o que está ativo/recente", "showing only what is active/recent"),
                });
            }
            KeyCode::Char('f') => {
                self.only_selected = !self.only_selected;
                self.resync();
                self.flash(match self.only_selected {
                    true => t("agentes/tasks: só da sessão selecionada", "agents/tasks: selected session only"),
                    false => t("agentes/tasks: todas as sessões", "agents/tasks: all sessions"),
                });
            }
            KeyCode::Char('r') => {
                let _ = ctl.send(Ctl::Refresh);
                self.tail = None;
                self.flash(t("atualizando…", "refreshing…"));
            }
            KeyCode::Char('c') => self.copy(),
            KeyCode::Char('o') => self.open(ctl),
            KeyCode::Char('e') => self.explore(),
            KeyCode::Char('x') => self.stop(),
            KeyCode::Char('d') => self.dismiss(false),
            KeyCode::Char('D') => self.dismiss(true),
            KeyCode::Char('u') => self.undo_dismiss(),
            KeyCode::Char('n') => self.open_launcher(),
            KeyCode::Char('v') => self.cycle_usage_filter(),
            KeyCode::Char('?') => self.help = true,
            _ => {}
        }
    }

    // ---------- mouse ----------

    /// Roda: rola o que está sob o cursor, sem mudar foco nem seleção.
    /// Clique: dá foco ao painel e seleciona a linha. Devolve se é preciso redesenhar.
    pub fn on_mouse(&mut self, m: MouseEvent) -> bool {
        if self.confirm.is_some() || self.launcher.is_some() || self.help {
            return false;
        }
        let pos = Position::new(m.column, m.row);
        let panel = Panel::ALL.into_iter().find(|p| self.rects[p.idx()].contains(pos));
        let in_main = self.main_rect.contains(pos);
        match m.kind {
            // Arrastar dentro do chat seleciona texto; soltar copia.
            MouseEventKind::Down(MouseButton::Left) if self.chat_rect.contains(pos) && self.shown_chat().is_some() => {
                let i = self.shown_chat().unwrap();
                let key = self.terms[i].key.clone();
                if !self.chat_focus {
                    // Clicar no chat entrega o teclado a ele.
                    self.chat_key = Some(key.clone());
                    self.chat_focus = true;
                }
                let pt = self.chat_point(i, m.column, m.row);
                self.chat_sel = Some(ChatSel { key, anchor: pt, head: pt, dragging: true });
                true
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                let Some(i) = self.chat_sel.as_ref().filter(|s| s.dragging).and_then(|s| self.term_idx(&s.key)) else {
                    return false;
                };
                // Passou da borda: rola o chat para continuar selecionando.
                let r = self.chat_rect;
                if m.row < r.y {
                    self.terms[i].scroll_by(1);
                } else if m.row >= r.bottom() {
                    self.terms[i].scroll_by(-1);
                }
                let pt = self.chat_point(i, m.column, m.row);
                if let Some(s) = self.chat_sel.as_mut() {
                    s.head = pt;
                }
                true
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let Some(s) = self.chat_sel.as_mut().filter(|s| s.dragging) else { return false };
                s.dragging = false;
                if s.anchor == s.head {
                    // Clique simples: não seleciona nada.
                    self.chat_sel = None;
                } else {
                    self.copy_chat_sel();
                }
                true
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let down = m.kind == MouseEventKind::ScrollDown;
                match panel {
                    Some(Panel::Usage) => {
                        // O gráfico de dias só mostra a janela em volta do dia selecionado.
                        let keys = self.keys(Panel::Usage);
                        if keys.is_empty() {
                            return false;
                        }
                        let cur = self.states[3].selected().unwrap_or(0);
                        let next = if down { (cur + 1).min(keys.len() - 1) } else { cur.saturating_sub(1) };
                        self.states[3].select(Some(next));
                        self.sel_keys[3] = Some(keys[next].clone());
                    }
                    Some(p) => {
                        let i = p.idx();
                        let max = self.keys(p).len().saturating_sub(self.list_height(p));
                        self.offsets[i] = if down { (self.offsets[i] + 3).min(max) } else { self.offsets[i].saturating_sub(3) };
                        // A roda manda na vista: nada de puxar de volta para a seleção.
                        let (off, h) = (self.offsets[i], self.list_height(p));
                        self.reveal[i] = false;
                        self.sel_visible[i] = self.states[i].selected().is_some_and(|s| s >= off && s < off + h);
                        self.wheel = Some(p);
                    }
                    None if in_main => match self.shown_chat() {
                        Some(i) => {
                            let r = self.chat_rect;
                            let col = m.column.clamp(r.x, r.right().saturating_sub(1)) - r.x;
                            let row = m.row.clamp(r.y, r.bottom().saturating_sub(1)) - r.y;
                            if self.terms[i].wheel(!down, col, row) {
                                // O programa redesenha a tela: a seleção marcada deixaria de bater.
                                self.chat_sel = None;
                            }
                        }
                        None if self.showing_chat() => {}
                        None => self.scroll_main(if down { 3 } else { -3 }),
                    },
                    None => return false,
                }
                true
            }
            MouseEventKind::Down(MouseButton::Left) => {
                self.wheel = None;
                self.chat_sel = None;
                if let Some(p) = panel {
                    if self.panel_empty(p) {
                        return false;
                    }
                    self.chat_focus = false;
                    self.set_focus(p);
                    let r = self.rects[p.idx()];
                    let inside = m.row > r.y && m.row + 1 < r.bottom();
                    if p != Panel::Usage && inside {
                        let idx = (m.row - r.y - 1) as usize + self.offsets[p.idx()];
                        self.select_idx(p, idx);
                    }
                    true
                } else if in_main && !self.chat_focus && self.shown_chat().is_some() {
                    // Clicar no chat entrega o teclado a ele.
                    self.chat_key = self.selected_session().map(|s| s.session_id.clone());
                    self.chat_focus = true;
                    true
                } else {
                    false
                }
            }
            _ => false,
        }
    }

    /// Posição do mouse -> ponto (linha, coluna) no texto do chat `i`,
    /// presa à área da tela do chat.
    fn chat_point(&self, i: usize, col: u16, row: u16) -> (i64, u16) {
        let r = self.chat_rect;
        let col = col.clamp(r.x, r.right().saturating_sub(1)) - r.x;
        let row = row.clamp(r.y, r.bottom().saturating_sub(1)) - r.y;
        (row as i64 - self.terms[i].scroll as i64, col)
    }

    /// Copia o texto selecionado no chat para a área de transferência.
    fn copy_chat_sel(&mut self) {
        let Some(s) = &self.chat_sel else { return };
        let Some(i) = self.term_idx(&s.key) else { return };
        let (a, b) = (s.anchor, s.head);
        let text = {
            let mut p = self.terms[i].parser.lock().unwrap();
            term::text_between(&mut p, a, b)
        };
        if text.trim().is_empty() {
            self.flash(t("nada selecionado", "nothing selected"));
            return;
        }
        let n = text.lines().count();
        let s = if n == 1 { "" } else { "s" };
        match set_clipboard(&text) {
            Ok(()) => self.flash(tr!("{n} linha{s} copiada{s}", "{n} line{s} copied")),
            Err(e) => self.flash(tr!("falha ao copiar: {e}", "copy failed: {e}")),
        }
    }

    /// O painel principal está na aba de chat?
    fn showing_chat(&self) -> bool {
        self.chat_focus || (self.focus == Panel::Sessions && self.tab == 3)
    }

    /// Índice do chat embutido que está na tela, se houver.
    fn shown_chat(&self) -> Option<usize> {
        if self.chat_focus {
            self.chat_key.as_ref().and_then(|k| self.term_idx(k))
        } else if self.focus == Panel::Sessions && self.tab == 3 {
            self.selected_session().and_then(|s| self.term_idx(&s.session_id))
        } else {
            None
        }
    }

    fn scroll_main(&mut self, delta: i32) {
        // Na aba "Saída" a rolagem conta a partir do fim, então as setas se invertem.
        let inverted = self.focus == Panel::Tasks && self.tab == 0;
        let d = if inverted { -delta } else { delta };
        self.scroll = (self.scroll as i32 + d).clamp(0, u16::MAX as i32) as u16;
    }

    fn copy(&mut self) {
        let text = match self.focus {
            Panel::Sessions => self.selected_session().map(|s| match &s.agent {
                // Chat ainda sem conversa conhecida: a linha de comando dele.
                Some(_) if s.kind == EXTERNAL_KIND => s.title.clone().unwrap_or_default(),
                // Aberto, mas sem conversa ainda: a pasta, para abrir outro lá.
                Some(_) if s.session_id.starts_with(crate::antigravity::NEW_PREFIX) => s.cwd.clone(),
                Some(agent) => self
                    .agent_def(agent)
                    .and_then(|d| d.resume_args(&s.session_id).map(|a| format!("{} {}", d.command, a.join(" "))))
                    .unwrap_or_else(|| s.session_id.clone()),
                None => format!("claude --resume {}", s.session_id),
            }),
            Panel::Agents => self.selected_agent().map(|a| a.agent_id.clone()),
            Panel::Tasks => self
                .selected_task()
                .map(|t| t.output_file.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| t.id.clone())),
            Panel::Usage => self.selected_day().map(|(d, t)| format!("{d}: {} tokens", t.total())),
        };
        let Some(text) = text else { return };
        match set_clipboard(&text) {
            Ok(()) => self.flash(tr!("copiado: {text}", "copied: {text}")),
            Err(e) => self.flash(tr!("falha ao copiar: {e}", "copy failed: {e}")),
        }
    }

    /// `o`: sessão encerrada -> retoma numa aba nova; em background -> attach;
    /// task -> abre a saída; agente -> pasta da sessão.
    fn open(&mut self, ctl: &Sender<Ctl>) {
        match self.focus {
            Panel::Sessions => {
                let Some(s) = self.selected_session().cloned() else { return };
                if self.is_external(&s.session_id) {
                    self.flash(tr!("{} roda aqui no painel: Enter entra no chat", "{} runs here in the panel: Enter goes into the chat", s.name));
                    return;
                }
                if s.alive && s.bg_id.is_none() {
                    self.flash(tr!("{} já está aberta em outro terminal (pid {})", "{} is already open in another terminal (pid {})", s.name, s.pid.unwrap_or(0)));
                    return;
                }
                if let Some(agent) = &s.agent {
                    // Conversa encerrada de outro agente: retoma numa aba nova.
                    if s.cwd.is_empty() {
                        self.flash(t("não sei o diretório desta sessão", "this session's directory is unknown"));
                        return;
                    }
                    match self.agent_resume(agent, &s.session_id) {
                        Ok((prog, args)) => {
                            let a = Action::OpenAgent { program: prog.to_string_lossy().into(), args, cwd: s.cwd.clone(), title: s.name.clone() };
                            let _ = ctl.send(Ctl::Act(a));
                            self.flash(tr!("abrindo {}…", "opening {}…", s.name));
                        }
                        Err(e) => self.flash(e),
                    }
                    return;
                }
                if s.cwd.is_empty() {
                    self.flash(t("não sei o diretório desta sessão", "this session's directory is unknown"));
                    return;
                }
                let a = match (&s.bg_id, s.alive || s.status == "pendente") {
                    (Some(id), true) => Action::Attach { id: id.clone(), cwd: s.cwd.clone(), name: s.name.clone() },
                    _ => Action::Resume { session_id: s.session_id.clone(), cwd: s.cwd.clone(), name: s.name.clone() },
                };
                let _ = ctl.send(Ctl::Act(a));
                self.flash(tr!("abrindo {}…", "opening {}…", s.name));
            }
            Panel::Agents => self.explore(),
            Panel::Tasks => self.launch_viewer(),
            Panel::Usage => {}
        }
    }

    fn explore(&mut self) {
        let sid = match self.focus {
            Panel::Sessions => self.selected_session().map(|s| s.session_id.clone()),
            Panel::Agents => self.selected_agent().map(|a| a.session_id.clone()),
            Panel::Tasks => self.selected_task().map(|t| t.session_id.clone()),
            Panel::Usage => None,
        };
        let Some(cwd) = sid.and_then(|id| self.snap.sessions.iter().find(|s| s.session_id == id)).map(|s| s.cwd.clone()) else {
            self.flash(t("nada para abrir aqui", "nothing to open here"));
            return;
        };
        match Command::new("explorer").arg(&cwd).stdout(Stdio::null()).stderr(Stdio::null()).spawn() {
            Ok(_) => self.flash(tr!("abrindo {cwd}", "opening {cwd}")),
            Err(e) => self.flash(tr!("falha ao abrir: {e}", "open failed: {e}")),
        }
    }

    fn launch_viewer(&mut self) {
        let target: Option<(String, PathBuf)> =
            self.selected_task().and_then(|t| t.output_file.clone()).map(|p| ("notepad".to_string(), p));
        let Some((prog, path)) = target else {
            self.flash(t("nada para abrir aqui", "nothing to open here"));
            return;
        };
        match Command::new(&prog).arg(&path).stdout(Stdio::null()).stderr(Stdio::null()).spawn() {
            Ok(_) => self.flash(tr!("abrindo {}", "opening {}", path.display())),
            Err(e) => self.flash(tr!("falha ao abrir: {e}", "open failed: {e}")),
        }
    }

    // ---------- chat embutido ----------

    pub fn term_idx(&self, key: &str) -> Option<usize> {
        self.terms.iter().position(|t| t.key == key)
    }

    fn spawn_chat(&mut self, key: String, title: String, cwd: &str, args: &[String]) {
        match Term::spawn(key, title, cwd, args, self.chat_size.0, self.chat_size.1) {
            Ok(mut t) => {
                t.restorable = args.first().map(String::as_str) != Some("attach");
                self.adopt_chat(t);
            }
            Err(e) => self.flash(tr!("falha ao abrir o chat: {e}", "could not open the chat: {e}")),
        }
    }

    /// Abre outro agente (Antigravity, Codex…) num chat embutido. `program` é o
    /// caminho já achado no PATH.
    /// `conversation`: id da conversa retomada; None = conversa nova, que
    /// ganha o id certo quando o coletor a descobrir (`rekey_terms`).
    fn spawn_agent_chat(&mut self, agent: &str, program: &Path, title: String, cwd: &str, args: &[String], conversation: Option<&str>) {
        let key = match conversation {
            Some(id) => id.to_string(),
            None => format!("{}-{}", agent.to_lowercase(), term::new_uuid()),
        };
        let prog = program.to_string_lossy();
        match Term::spawn_program(&prog, key, title, cwd, args, self.chat_size.0, self.chat_size.1) {
            Ok(mut t) => {
                // Só dá para reabrir depois de saber o id da conversa.
                t.restorable = conversation.is_some();
                t.agent = Some(agent.to_string());
                let name = Path::new(&*prog).file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                t.command_line = std::iter::once(name).chain(args.iter().cloned()).collect::<Vec<_>>().join(" ");
                self.adopt_chat(t);
            }
            Err(e) => self.flash(tr!("falha ao abrir o {agent}: {e}", "failed to open {agent}: {e}")),
        }
    }

    /// Passa a mostrar um chat recém-aberto e põe o teclado nele.
    fn adopt_chat(&mut self, t: Term) {
        let (key, title) = (t.key.clone(), t.title.clone());
        self.terms.push(t);
        self.save_open_chats();
        self.merge_external();
        self.focus = Panel::Sessions;
        self.tab = 3;
        self.sel_keys[0] = Some(key.clone());
        self.pending_sel = Some(key.clone());
        self.chat_key = Some(key);
        self.chat_focus = true;
        self.resync();
        self.flash(tr!("chat {title} aberto · F12 volta ao painel", "chat {title} open · F12 returns to the panel"));
    }

    fn agent_def(&self, name: &str) -> Option<&AgentDef> {
        self.agents.iter().find(|a| a.name.eq_ignore_ascii_case(name))
    }

    /// Chat de outro agente aberto aqui no painel com esta chave.
    fn is_external(&self, session_id: &str) -> bool {
        self.terms.iter().any(|t| t.key == session_id && t.agent.is_some())
    }

    /// Como o agente retoma a conversa `id` (programa achado no PATH + args).
    fn agent_resume(&self, agent: &str, id: &str) -> Result<(PathBuf, Vec<String>), String> {
        let def = self.agent_def(agent).ok_or_else(|| tr!("{agent} não está na lista de agentes", "{agent} is not in the agent list"))?;
        let args = def.resume_args(id).ok_or_else(|| tr!("{agent} não tem como retomar uma conversa pelo id", "{agent} cannot resume a conversation by id"))?;
        let prog = agents::which(&def.command).ok_or_else(|| tr!("{} não está no PATH", "{} is not on the PATH", def.command))?;
        Ok((prog, args))
    }

    /// Retoma aqui no painel uma conversa de outro agente.
    fn resume_agent_chat(&mut self, agent: &str, id: &str, title: String, cwd: &str) {
        match self.agent_resume(agent, id) {
            Ok((prog, args)) => self.spawn_agent_chat(agent, &prog, title, cwd, &args, Some(id)),
            Err(e) => self.flash(e),
        }
    }

    /// `Enter` numa sessão: entra no chat dela (abrindo aqui, se preciso).
    fn enter_chat(&mut self) {
        let Some(s) = self.selected_session().cloned() else { return };
        if self.term_idx(&s.session_id).is_some() {
            self.chat_key = Some(s.session_id.clone());
            self.chat_focus = true;
            self.tab = 3;
            return;
        }
        if let Some(agent) = &s.agent {
            if s.alive {
                self.flash(tr!(
                    "{} está aberta em outro terminal (pid {}). Para conversar aqui: x encerra, depois Enter retoma no painel",
                    "{} is open in another terminal (pid {}). To chat here: x ends it, then Enter resumes it in the panel",
                    s.name,
                    s.pid.unwrap_or(0)
                ));
            } else if s.cwd.is_empty() {
                self.flash(t("não sei o diretório desta sessão", "this session's directory is unknown"));
            } else {
                self.resume_agent_chat(agent, &s.session_id, s.name.clone(), &s.cwd);
            }
            return;
        }
        let waiting = s.status == "pendente";
        if s.alive && s.bg_id.is_none() {
            self.flash(tr!(
                "{} está aberta em outro terminal (pid {}). Para conversar aqui: x encerra, depois Enter retoma no painel",
                "{} is open in another terminal (pid {}). To chat here: x ends it, then Enter resumes it in the panel",
                s.name,
                s.pid.unwrap_or(0)
            ));
            return;
        }
        if s.cwd.is_empty() {
            self.flash(t("não sei o diretório desta sessão", "this session's directory is unknown"));
            return;
        }
        let args: Vec<String> = match &s.bg_id {
            Some(id) if s.alive || waiting => vec!["attach".into(), id.clone()],
            _ => vec!["--resume".into(), s.session_id.clone()],
        };
        self.spawn_chat(s.session_id.clone(), s.name.clone(), &s.cwd, &args);
    }

    fn chat_key_event(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let shift = k.modifiers.contains(KeyModifiers::SHIFT);
        let leave = matches!(k.code, KeyCode::F(12))
            || (ctrl && !term::is_altgr(&k) && matches!(k.code, KeyCode::Char(']') | KeyCode::Char('5')));
        if leave {
            self.chat_focus = false;
            return;
        }
        // Com texto selecionado, Ctrl+C copia (em vez de interromper o Claude).
        if self.chat_sel.as_ref().is_some_and(|s| !s.dragging)
            && ctrl
            && !term::is_altgr(&k)
            && matches!(k.code, KeyCode::Char('c') | KeyCode::Char('C'))
        {
            self.copy_chat_sel();
            self.chat_sel = None;
            return;
        }
        // Qualquer outra tecla desfaz a seleção.
        self.chat_sel = None;
        let Some(i) = self.chat_key.as_ref().and_then(|key| self.term_idx(key)) else {
            self.chat_focus = false;
            return;
        };
        let page = self.terms[i].rows.saturating_sub(2).max(1) as isize;
        match k.code {
            KeyCode::PageUp if shift => self.terms[i].scroll_by(page),
            KeyCode::PageDown if shift => self.terms[i].scroll_by(-page),
            _ => {
                if let Some(b) = term::key_bytes(&k) {
                    self.terms[i].send(&b);
                }
            }
        }
    }

    /// Muitas teclas de uma vez = texto colado: vai como um bloco só, para
    /// as quebras de linha não enviarem a mensagem pela metade.
    pub fn chat_paste(&mut self, keys: &[KeyEvent]) {
        let Some(i) = self.chat_key.as_ref().and_then(|key| self.term_idx(key)) else { return };
        let text: String = keys
            .iter()
            .filter_map(|k| match k.code {
                KeyCode::Char(c) => Some(c),
                KeyCode::Enter => Some('\r'),
                KeyCode::Tab => Some('\t'),
                _ => None,
            })
            .collect();
        self.terms[i].paste(&text);
    }

    /// Remove os chats cujo processo terminou.
    pub fn reap_terms(&mut self) {
        let mut i = 0;
        while i < self.terms.len() {
            if self.terms[i].exited() {
                let t = self.terms.remove(i);
                self.save_open_chats();
                if self.pending_sel.as_deref() == Some(&t.key) {
                    self.pending_sel = None;
                }
                if self.chat_key.as_deref() == Some(&t.key) {
                    self.chat_focus = false;
                    self.chat_key = None;
                    if self.focus == Panel::Sessions && self.tab == 3 {
                        self.tab = 0;
                    }
                }
                let last = t.last_line();
                if t.agent.is_some() {
                    self.merge_external();
                    self.resync();
                }
                let last = if last.is_empty() { String::new() } else { format!(": {last}") };
                self.flash(tr!("chat {} encerrado{last}", "chat {} ended{last}", t.title));
            } else {
                i += 1;
            }
        }
    }

    fn ask_quit(&mut self) {
        if self.terms.is_empty() {
            self.quit = true;
            return;
        }
        let n = self.terms.len();
        let mut body: Vec<String> = self
            .terms
            .iter()
            .map(|t| match &t.agent {
                Some(a) => format!("• {} ({a})", t.title),
                None => format!("• {}", t.title),
            })
            .collect();
        body.push(t("Os chats abertos aqui dentro fecham junto com o dashboard.", "The chats open in here close together with the dashboard.").into());
        body.push(
            t(
                "As conversas ficam salvas; ao abrir o dashboard de novo ele oferece reabri-las.",
                "The conversations stay saved; the dashboard offers to reopen them next time it opens.",
            )
            .into(),
        );
        if self.terms.iter().any(|t| t.agent.is_some() && !t.restorable) {
            body.push(
                t(
                    "Chats de outro agente que ainda não têm conversa (ou cujo agente não retoma conversas) não são reabertos.",
                    "Chats of another agent that have no conversation yet (or whose agent cannot resume) are not reopened.",
                )
                .into(),
            );
        }
        let s = if n == 1 { "" } else { "s" };
        self.confirm = Some(Confirm {
            title: tr!("Sair e fechar {n} chat{s}", "Quit and close {n} chat{s}"),
            body,
            action: None,
            quit: true,
            hide: Vec::new(),
            restore: Vec::new(),
            kill_chat: None,
        });
    }

    // ---------- ações ----------

    fn ask(&mut self, title: impl Into<String>, body: Vec<String>, action: Action) {
        self.confirm = Some(Confirm {
            title: title.into(),
            body,
            action: Some(action),
            quit: false,
            hide: Vec::new(),
            restore: Vec::new(),
            kill_chat: None,
        });
    }

    /// `x`: parar a task / encerrar a sessão selecionada (sempre com confirmação).
    fn stop(&mut self) {
        match self.focus {
            Panel::Sessions => {
                let Some(s) = self.selected_session().cloned() else { return };
                if self.is_external(&s.session_id) {
                    self.confirm = Some(Confirm {
                        title: tr!("Fechar o chat {}", "Close the chat {}", s.name),
                        body: vec![
                            tr!(
                                "Finaliza o {} que roda aqui no painel (pid {}).",
                                "Kills the {} running here in the panel (pid {}).",
                                s.agent.as_deref().unwrap_or(&s.name),
                                s.pid.unwrap_or(0)
                            ),
                            t(
                                "O dashboard não guarda a conversa de outros agentes; se o agente salva histórico, ele fica onde o agente guarda.",
                                "The dashboard does not keep other agents' conversations; if the agent saves history, it stays where the agent keeps it.",
                            )
                            .into(),
                        ],
                        action: None,
                        quit: false,
                        hide: Vec::new(),
                        restore: Vec::new(),
                        kill_chat: Some(s.session_id.clone()),
                    });
                    return;
                }
                if let (Some(agent), true) = (&s.agent, s.alive) {
                    // Outro agente aberto em outro terminal: encerra o processo dele.
                    let mut body = vec![
                        tr!("Finaliza o processo {} ({agent}) e os filhos dele ({} processos).", "Kills process {} ({agent}) and its children ({} processes).", s.pid.unwrap_or(0), s.procs.len()),
                        t("O que estiver sendo feito agora é interrompido.", "Whatever it is doing right now is interrupted.").into(),
                        t(
                            "A conversa fica salva: depois, `a` mostra as encerradas e Enter ou `o` retoma.",
                            "The conversation stays saved: later, `a` shows the ended ones and Enter or `o` resumes it.",
                        )
                        .into(),
                    ];
                    if s.status == "busy" {
                        body.insert(0, t("⚠ A sessão está ocupada neste momento.", "⚠ The session is busy right now.").into());
                    }
                    self.ask(tr!("Encerrar a sessão {}", "End the session {}", s.name), body, Action::KillAgent { session_id: s.session_id.clone() });
                    return;
                }
                if s.status == "pendente" {
                    self.flash(tr!(
                        "{} não está rodando: está parada esperando sua resposta. o reabre, d remove",
                        "{} is not running: it stopped waiting for your reply. o reopens, d removes",
                        s.name
                    ));
                    return;
                }
                if !s.alive {
                    self.flash(tr!("{} já está encerrada", "{} has already ended", s.name));
                    return;
                }
                if let (Some(id), None) = (&s.bg_id, s.pid) {
                    self.ask(
                        tr!("Parar a sessão em background {}", "Stop the background session {}", s.name),
                        vec![
                            tr!("Roda `claude stop {id}`.", "Runs `claude stop {id}`."),
                            t("A conversa fica salva: `o` reabre depois.", "The conversation stays saved: `o` reopens it later.").into(),
                        ],
                        Action::StopBg { id: id.clone(), name: s.name.clone() },
                    );
                } else {
                    let (pid, n) = (s.pid.unwrap_or(0), s.procs.len());
                    let mut body = vec![
                        tr!("Finaliza o processo {pid} e os filhos dele ({n} processos).", "Kills process {pid} and its children ({n} processes)."),
                        t("O que estiver sendo feito agora é interrompido.", "Whatever it is doing right now is interrupted.").into(),
                        t(
                            "A conversa fica salva: depois, `a` mostra as encerradas e `o` retoma.",
                            "The conversation stays saved: later, `a` shows the ended ones and `o` resumes it.",
                        )
                        .into(),
                    ];
                    if s.status == "busy" {
                        body.insert(0, t("⚠ A sessão está ocupada neste momento.", "⚠ The session is busy right now.").into());
                    }
                    if s.agents_running + s.tasks_running > 0 {
                        let (ar, tr_) = (s.agents_running, s.tasks_running);
                        body.insert(0, tr!("⚠ {ar} agentes e {tr_} tasks rodando serão interrompidos.", "⚠ {ar} running agents and {tr_} tasks will be interrupted."));
                    }
                    self.ask(tr!("Encerrar a sessão {}", "End the session {}", s.name), body, Action::KillSession { session_id: s.session_id.clone() });
                }
            }
            Panel::Tasks => {
                let Some(task) = self.selected_task().cloned() else { return };
                if task.status != TaskStatus::Running {
                    self.flash(t("esta task não está rodando (use d para limpar da lista)", "this task is not running (d clears it from the list)"));
                    return;
                }
                match task.kind {
                    TaskKind::Shell | TaskKind::Monitor => {
                        let mut body = vec![tr!("Sessão: {}", "Session: {}", task.session_name)];
                        if let Some(c) = &task.command {
                            body.push(tr!("Comando: {}", "Command: {}", c.chars().take(200).collect::<String>()));
                        }
                        body.push(
                            t(
                                "Finaliza o processo do shell e os filhos dele; a sessão recebe o aviso de que a task terminou.",
                                "Kills the shell process and its children; the session is told the task finished.",
                            )
                            .into(),
                        );
                        self.ask(
                            tr!("Parar {} {}", "Stop {} {}", task.kind.label(), task.id),
                            body,
                            Action::StopTask { session_id: task.session_id.clone(), task_id: task.id.clone() },
                        );
                    }
                    _ => self.flash(tr!(
                        "{} roda dentro do processo da sessão {}: pare pela própria sessão (TaskStop) ou encerre a sessão no painel 1",
                        "{} runs inside the process of session {}: stop it from the session itself (TaskStop) or end the session in panel 1",
                        task.kind.label(),
                        task.session_name
                    )),
                }
            }
            Panel::Agents => {
                let Some(a) = self.selected_agent() else { return };
                if a.running {
                    let msg = tr!(
                        "subagentes rodam dentro do processo da sessão {}: não dá para parar só um daqui. Encerre a sessão no painel 1 (x)",
                        "subagents run inside the process of session {}: one alone cannot be stopped from here. End the session in panel 1 (x)",
                        a.session_name
                    );
                    self.flash(msg);
                } else {
                    self.flash(t("este agente já terminou (use d para limpar da lista)", "this agent has already finished (d clears it from the list)"));
                }
            }
            Panel::Usage => {}
        }
    }

    /// `d`: limpa o item selecionado da lista; `D`: todos os terminados do painel.
    fn dismiss(&mut self, all: bool) {
        let keys: Vec<String> = match (self.focus, all) {
            (Panel::Sessions, false) => {
                let Some(s) = self.selected_session().cloned() else { return };
                if s.alive {
                    self.flash(t("sessão ativa: encerre antes com x", "active session: end it first with x"));
                    return;
                }
                if let Some(id) = &s.bg_id {
                    self.ask(
                        tr!("Remover a sessão em background {}", "Remove the background session {}", s.name),
                        vec![
                            tr!(
                                "Roda `claude rm {id}`: apaga a sessão em background (e a worktree dela, quando for seguro).",
                                "Runs `claude rm {id}`: deletes the background session (and its worktree, when safe)."
                            ),
                            t("Não dá para desfazer.", "This cannot be undone.").into(),
                        ],
                        Action::RemoveBg { id: id.clone(), name: s.name.clone() },
                    );
                    if let Some(c) = self.confirm.as_mut() {
                        c.hide = vec![format!("s:{}", s.session_id)];
                    }
                    return;
                }
                vec![format!("s:{}", s.session_id)]
            }
            (Panel::Sessions, true) => self.sessions().iter().filter(|s| !s.alive).map(|s| format!("s:{}", s.session_id)).collect(),
            (Panel::Agents, false) => match self.selected_agent() {
                Some(a) if a.running => {
                    self.flash(t("agente rodando: não dá para limpar", "agent running: cannot clear it"));
                    return;
                }
                Some(a) => vec![format!("a:{}", a.agent_id)],
                None => return,
            },
            (Panel::Agents, true) => self.agents().iter().filter(|a| !a.running).map(|a| format!("a:{}", a.agent_id)).collect(),
            (Panel::Tasks, false) => match self.selected_task() {
                Some(task) if task.status == TaskStatus::Running => {
                    self.flash(t("task rodando: pare antes com x", "task running: stop it first with x"));
                    return;
                }
                Some(t) => vec![format!("t:{}:{}", t.session_id, t.id)],
                None => return,
            },
            (Panel::Tasks, true) => self
                .tasks()
                .iter()
                .filter(|t| t.status != TaskStatus::Running)
                .map(|t| format!("t:{}:{}", t.session_id, t.id))
                .collect(),
            (Panel::Usage, _) => return,
        };
        if keys.is_empty() {
            self.flash(t("nada terminado para limpar", "nothing finished to clear"));
            return;
        }
        let n = keys.len();
        self.hidden.extend(keys.iter().cloned());
        self.last_hidden = keys;
        self.save_hidden();
        if self.show_all {
            self.show_all = false;
        }
        self.resync();
        let what = if n == 1 { t("item limpo", "item cleared") } else { t("itens limpos", "items cleared") };
        self.flash(tr!("{n} {what} da lista (u desfaz · a mostra tudo)", "{n} {what} from the list (u undoes · a shows all)"));
    }

    fn undo_dismiss(&mut self) {
        if self.last_hidden.is_empty() {
            self.flash(t("nada para desfazer", "nothing to undo"));
            return;
        }
        let n = self.last_hidden.len();
        for k in std::mem::take(&mut self.last_hidden) {
            self.hidden.remove(&k);
        }
        self.save_hidden();
        self.resync();
        self.flash(tr!("{n} itens de volta na lista", "{n} items back in the list"));
    }

    fn open_launcher(&mut self) {
        let repo = self.selected_session().map(|s| s.cwd.clone()).unwrap_or_default();
        // O Claude está sempre disponível (o dashboard depende dele); os outros
        // são procurados no PATH a cada vez que o formulário abre.
        let installed = self.agents.iter().map(|a| if a.is_claude() { Some(PathBuf::from("claude")) } else { agents::which(&a.command) }).collect();
        self.launcher = Some(LaunchForm {
            field: F_REPO,
            repo,
            sug: 0,
            agent: 0,
            installed,
            model: 0,
            mode: 0,
            name: String::new(),
            prompt: String::new(),
            error: None,
        });
    }

    fn launcher_key(&mut self, k: KeyEvent, ctl: &Sender<Ctl>) {
        let repos = self.snap.repos.clone();
        let agent_defs = self.agents.clone();
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let Some(f) = self.launcher.as_mut() else { return };
        f.error = None;
        let n_sug = f.suggestions(&repos).len();
        let cycle = |cur: usize, n: usize, fwd: bool| if n == 0 { 0 } else if fwd { (cur + 1) % n } else { (cur + n - 1) % n };
        match k.code {
            KeyCode::Esc => {
                self.launcher = None;
                return;
            }
            KeyCode::Tab => f.field = (f.field + 1) % LAUNCH_FIELDS,
            KeyCode::BackTab => f.field = (f.field + LAUNCH_FIELDS - 1) % LAUNCH_FIELDS,
            KeyCode::Down if f.field == F_REPO && n_sug > 0 => f.sug = (f.sug + 1).min(n_sug - 1),
            KeyCode::Up if f.field == F_REPO => f.sug = f.sug.saturating_sub(1),
            KeyCode::Down => f.field = (f.field + 1) % LAUNCH_FIELDS,
            KeyCode::Up => f.field = (f.field + LAUNCH_FIELDS - 1) % LAUNCH_FIELDS,
            KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') if matches!(f.field, F_AGENT | F_MODEL | F_MODE) => {
                let fwd = !matches!(k.code, KeyCode::Left);
                match f.field {
                    F_AGENT => {
                        f.agent = cycle(f.agent, agent_defs.len(), fwd);
                        // Cada agente tem os seus modelos; e só o Claude roda em background.
                        f.model = 0;
                        f.mode = f.mode.min(f.modes(&agent_defs) - 1);
                    }
                    F_MODEL => {
                        let n = agent_defs.get(f.agent).map_or(1, |a| a.model_choices().len());
                        f.model = cycle(f.model, n, fwd);
                    }
                    _ => f.mode = cycle(f.mode, f.modes(&agent_defs), fwd),
                }
            }
            KeyCode::Char('u') if ctrl => {
                if let Some(t) = text_field(f) {
                    t.clear();
                }
                f.sug = 0;
            }
            KeyCode::Backspace => {
                if let Some(t) = text_field(f) {
                    t.pop();
                }
                f.sug = 0;
            }
            KeyCode::Char(c) if !ctrl || term::is_altgr(&k) => {
                if let Some(t) = text_field(f) {
                    t.push(c);
                }
                f.sug = 0;
            }
            KeyCode::Enter => {
                let Some(cwd) = f.chosen(&repos) else {
                    f.error = Some(t("escolha um repositório da lista ou digite um caminho que exista", "pick a repository from the list or type a path that exists").into());
                    return;
                };
                let Some(def) = agent_defs.get(f.agent).cloned() else { return };
                let model = def.model_choices().get(f.model).cloned().filter(|_| f.model > 0);
                if !def.is_claude() {
                    let Some(program) = f.installed.get(f.agent).cloned().flatten() else {
                        f.error = Some(match &def.install {
                            Some(i) => tr!("{} não está no PATH. Para instalar: {i}", "{} is not on the PATH. To install: {i}", def.command),
                            None => tr!("{} não está no PATH", "{} is not on the PATH", def.command),
                        });
                        return;
                    };
                    let prompt = Some(f.prompt.clone()).filter(|p| !p.trim().is_empty());
                    if prompt.as_deref().is_some_and(|p| agents::unsafe_for_batch(&program, p)) {
                        f.error = Some(
                            t(
                                "o prompt tem \" % & | < > ^, que o atalho .cmd não passa com segurança: abra sem prompt e mande a mensagem no chat",
                                "the prompt has \" % & | < > ^, which the .cmd shim cannot pass safely: open without a prompt and send the message in the chat",
                            )
                            .into(),
                        );
                        return;
                    }
                    let args = def.launch_args(model.as_deref(), prompt.as_deref());
                    let title = Some(f.name.trim().to_string()).filter(|n| !n.is_empty()).unwrap_or_else(|| {
                        let dir = Path::new(&cwd).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                        format!("{} {dir}", def.name.to_lowercase()).trim().to_string()
                    });
                    let in_panel = f.mode == 0;
                    self.launcher = None;
                    if in_panel {
                        self.spawn_agent_chat(&def.name, &program, title, &cwd, &args, None);
                    } else {
                        let _ = ctl.send(Ctl::Act(Action::OpenAgent { program: program.to_string_lossy().into(), args, cwd: cwd.clone(), title }));
                        self.flash(tr!("abrindo {} numa aba nova em {cwd}…", "opening {} in a new tab at {cwd}…", def.name));
                    }
                    return;
                }
                let spec = LaunchSpec {
                    cwd: cwd.clone(),
                    model,
                    name: Some(f.name.clone()).filter(|n| !n.trim().is_empty()),
                    prompt: Some(f.prompt.clone()).filter(|p| !p.trim().is_empty()),
                    background: f.mode == 2,
                };
                if f.mode == 0 {
                    let sid = term::new_uuid();
                    let mut args = vec!["--session-id".to_string(), sid.clone()];
                    args.extend(crate::actions::launch_args(&spec));
                    let title = spec.name.clone().unwrap_or_else(|| {
                        Path::new(&cwd).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "claude".into())
                    });
                    self.launcher = None;
                    self.spawn_chat(sid, title, &cwd, &args);
                    return;
                }
                let modo = if spec.background { t("em background", "in background") } else { t("numa aba nova", "in a new tab") };
                self.launcher = None;
                let _ = ctl.send(Ctl::Act(Action::Launch(spec)));
                self.flash(tr!("iniciando instância {modo} em {cwd}…", "starting instance {modo} in {cwd}…"));
                return;
            }
            _ => {}
        }
    }

    /// Últimas linhas de um arquivo de saída (lê só o final; cacheado por tamanho).
    pub fn tail_lines(&mut self, path: &Path) -> &[String] {
        let len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        let fresh = self.tail.as_ref().is_some_and(|t| t.path == path && t.len == len);
        if !fresh {
            let lines = read_tail(path, len).unwrap_or_else(|e| vec![tr!("(não consegui ler: {e})", "(could not read: {e})")]);
            self.tail = Some(TailCache { path: path.to_path_buf(), len, lines });
        }
        &self.tail.as_ref().unwrap().lines
    }
}

fn text_field(f: &mut LaunchForm) -> Option<&mut String> {
    match f.field {
        F_REPO => Some(&mut f.repo),
        F_NAME => Some(&mut f.name),
        F_PROMPT => Some(&mut f.prompt),
        _ => None,
    }
}

fn read_tail(path: &Path, len: u64) -> std::io::Result<Vec<String>> {
    const MAX: u64 = 256 * 1024;
    let mut f = File::open(path)?;
    let start = len.saturating_sub(MAX);
    f.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    let text = String::from_utf8_lossy(&buf);
    let clean = ansi_re().replace_all(&text, "");
    let mut lines: Vec<String> = clean
        .split('\n')
        .map(|l| {
            let l = l.trim_end_matches('\r');
            // Barras de progresso reescrevem a linha com \r: fica só o último trecho.
            l.rsplit('\r').next().unwrap_or(l).replace('\t', "    ")
        })
        .collect();
    if start > 0 && !lines.is_empty() {
        lines.remove(0); // primeira linha provavelmente cortada no meio
    }
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    Ok(lines)
}

/// Grava texto na área de transferência do Windows (CF_UNICODETEXT).
///
/// Direto pela API: o `clip.exe` não serve. Com BOM, o BOM (U+FEFF) entra
/// no texto colado; sem BOM, textos curtos com acento viram lixo.
#[cfg(windows)]
pub fn set_clipboard(text: &str) -> std::io::Result<()> {
    use std::ffi::c_void;
    #[link(name = "user32")]
    unsafe extern "system" {
        fn OpenClipboard(owner: *mut c_void) -> i32;
        fn EmptyClipboard() -> i32;
        fn SetClipboardData(format: u32, mem: *mut c_void) -> *mut c_void;
        fn CloseClipboard() -> i32;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GlobalAlloc(flags: u32, bytes: usize) -> *mut c_void;
        fn GlobalLock(mem: *mut c_void) -> *mut c_void;
        fn GlobalUnlock(mem: *mut c_void) -> i32;
        fn GlobalFree(mem: *mut c_void) -> *mut c_void;
    }
    const CF_UNICODETEXT: u32 = 13;
    const GMEM_MOVEABLE: u32 = 0x0002;
    let err = |what: &str| std::io::Error::new(std::io::ErrorKind::Other, format!("{what}: {}", std::io::Error::last_os_error()));
    // Quebras de linha no padrão do Windows, terminado em NUL.
    let wide: Vec<u16> = text.replace("\r\n", "\n").replace('\n', "\r\n").encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        // Outro programa pode estar com a área de transferência aberta: tenta algumas vezes.
        let mut opened = false;
        for _ in 0..10 {
            if OpenClipboard(std::ptr::null_mut()) != 0 {
                opened = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        if !opened {
            return Err(err(t("área de transferência ocupada", "clipboard busy")));
        }
        let result = (|| {
            if EmptyClipboard() == 0 {
                return Err(err("EmptyClipboard"));
            }
            let mem = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2);
            if mem.is_null() {
                return Err(err("GlobalAlloc"));
            }
            let dst = GlobalLock(mem) as *mut u16;
            if dst.is_null() {
                GlobalFree(mem);
                return Err(err("GlobalLock"));
            }
            std::ptr::copy_nonoverlapping(wide.as_ptr(), dst, wide.len());
            GlobalUnlock(mem);
            // Deu certo: a memória passa a ser do sistema.
            if SetClipboardData(CF_UNICODETEXT, mem).is_null() {
                GlobalFree(mem);
                return Err(err("SetClipboardData"));
            }
            Ok(())
        })();
        CloseClipboard();
        result
    }
}

#[cfg(not(windows))]
pub fn set_clipboard(_text: &str) -> std::io::Result<()> {
    Err(std::io::Error::new(std::io::ErrorKind::Unsupported, t("área de transferência só no Windows", "clipboard is Windows-only")))
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn key(app: &mut App, code: KeyCode, ctl: &Sender<Ctl>) {
        app.on_key(KeyEvent::new(code, KeyModifiers::NONE), ctl);
    }

    /// Fluxo completo com um agente falso (um `.cmd`, como os atalhos que o npm
    /// instala): abre pelo formulário `n`, o chat mostra o que ele imprime, a
    /// sessão entra na lista, e `x` + `y` fecha.
    #[test]
    fn agente_externo_abre_aparece_e_fecha() {
        let dir = std::env::temp_dir().join(format!("agent-dash-agente-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fake = dir.join("agentefalso.cmd");
        std::fs::write(&fake, "@echo off\r\necho AGENTE-FALSO %*\r\npause >nul\r\n").unwrap();

        let (ctl, _rx) = mpsc::channel();
        let mut app = App::new();
        app.agents.push(AgentDef {
            name: "Falso".into(),
            command: fake.display().to_string(),
            args: vec!["--fixo".into()],
            models: vec!["m1".into()],
            model_flag: Some("--model".into()),
            prompt_flag: Some("--prompt".into()),
            install: None,
            resume_flag: None,
        });
        let n_agents = app.agents.len();

        key(&mut app, KeyCode::Char('n'), &ctl);
        app.launcher.as_mut().unwrap().repo = dir.display().to_string();
        key(&mut app, KeyCode::Tab, &ctl); // Agente
        for _ in 0..n_agents - 1 {
            key(&mut app, KeyCode::Right, &ctl);
        }
        assert_eq!(app.launcher.as_ref().unwrap().agent, n_agents - 1);
        key(&mut app, KeyCode::Tab, &ctl); // Modelo
        key(&mut app, KeyCode::Right, &ctl); // m1
        key(&mut app, KeyCode::Tab, &ctl); // Modo (aqui no painel)
        key(&mut app, KeyCode::Right, &ctl);
        key(&mut app, KeyCode::Right, &ctl); // só 2 modos: volta para "aqui no painel"
        assert_eq!(app.launcher.as_ref().unwrap().mode, 0);
        key(&mut app, KeyCode::Tab, &ctl); // Nome
        key(&mut app, KeyCode::Tab, &ctl); // Prompt
        for c in "oi".chars() {
            key(&mut app, KeyCode::Char(c), &ctl);
        }
        key(&mut app, KeyCode::Enter, &ctl);
        assert!(app.launcher.is_none(), "erro no formulário: {:?}", app.status);
        assert_eq!(app.terms.len(), 1, "chat não abriu: {:?}", app.status);
        assert!(app.chat_focus);

        // O agente imprime os argumentos que recebeu.
        let t0 = Instant::now();
        while !app.terms[0].contents().contains("AGENTE-FALSO") && t0.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(50));
        }
        let screen = app.terms[0].contents();
        assert!(screen.contains("AGENTE-FALSO --fixo --model m1 --prompt oi"), "tela: {screen}");

        // Aparece na lista de sessões, mesmo depois de um snapshot novo do coletor.
        app.set_snapshot(Snapshot::default());
        let s = app.selected_session().cloned().expect("sessão selecionada");
        assert_eq!(s.kind, EXTERNAL_KIND);
        assert_eq!(s.status, "aberta");
        assert_eq!(s.agent.as_deref(), Some("Falso"));
        assert!(s.alive);
        assert_eq!(s.name, dir.file_name().map(|n| format!("falso {}", n.to_string_lossy())).unwrap());

        // A tela desenha sem quebrar, com a sessão e o chat.
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(160, 45)).unwrap();
        term.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
        let buf = term.backend().buffer().clone();
        let text: String = (0..buf.area.height)
            .map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>() + "\n")
            .collect();
        assert!(text.contains("◆"), "{text}");
        assert!(text.contains("AGENTE-FALSO"), "{text}");
        assert!(text.contains(crate::i18n::t("o dashboard ainda não lê os tokens deste agente", "the dashboard does not read this agent's tokens yet")), "{text}");

        // F12 devolve o teclado; x pede confirmação; y fecha o chat.
        key(&mut app, KeyCode::F(12), &ctl);
        key(&mut app, KeyCode::Char('x'), &ctl);
        assert!(app.confirm.as_ref().is_some_and(|c| c.kill_chat.is_some()));
        key(&mut app, KeyCode::Char('y'), &ctl);
        let t0 = Instant::now();
        while !app.terms.is_empty() && t0.elapsed() < Duration::from_secs(10) {
            app.reap_terms();
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(app.terms.is_empty(), "o chat não fechou");
        assert!(app.snap.sessions.iter().all(|s| s.kind != EXTERNAL_KIND));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Sessão de outro agente como o coletor entrega (ex.: Antigravity).
    fn agent_session(id: &str, cwd: &str, pid: Option<u32>) -> SessionInfo {
        SessionInfo {
            pid,
            session_id: id.into(),
            name: format!("conversa {id}"),
            cwd: cwd.into(),
            status: if pid.is_some() { "idle".into() } else { "encerrada".into() },
            kind: "antigravity".into(),
            agent: Some("Falso".into()),
            bg_id: None,
            pending: None,
            version: String::new(),
            started_at: None,
            alive: pid.is_some(),
            title: None,
            last_prompt: None,
            usage: Usage::default(),
            agents_usage: Tokens::default(),
            events: Vec::new(),
            transcript: None,
            cpu: 0.0,
            mem: 0,
            procs: Vec::new(),
            agents_total: 0,
            agents_running: 0,
            tasks_running: 0,
        }
    }

    /// Sessões de outro agente vistas pelo coletor: Enter retoma uma encerrada
    /// aqui no painel, a sessão do chat não aparece duplicada, um chat novo
    /// ganha o id da conversa quando ela aparece, e `x` numa sessão de outro
    /// terminal pede para encerrar o processo dela.
    #[test]
    fn sessoes_de_outro_agente_como_as_do_claude() {
        let dir = std::env::temp_dir().join(format!("agent-dash-retomar-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fake = dir.join("agenteretoma.cmd");
        std::fs::write(&fake, "@echo off\r\necho AGENTE-RETOMA %*\r\npause >nul\r\n").unwrap();
        let cwd = dir.display().to_string();

        let (ctl, rx) = mpsc::channel();
        let mut app = App::new();
        app.agents.push(AgentDef {
            name: "Falso".into(),
            command: fake.display().to_string(),
            args: Vec::new(),
            models: Vec::new(),
            model_flag: None,
            prompt_flag: None,
            install: None,
            resume_flag: Some("--retomar".into()),
        });

        // 1) Conversa encerrada: Enter retoma aqui, com a flag de retomar.
        let snap = |sessions| Snapshot { sessions, ..Default::default() };
        app.set_snapshot(snap(vec![agent_session("conv-1", &cwd, None)]));
        app.show_all = true; // encerradas só aparecem com `a`
        app.resync();
        key(&mut app, KeyCode::Enter, &ctl);
        assert_eq!(app.terms.len(), 1, "não retomou: {:?}", app.status);
        assert_eq!(app.terms[0].key, "conv-1");
        assert!(app.terms[0].restorable);
        let t0 = Instant::now();
        while !app.terms[0].contents().contains("AGENTE-RETOMA") && t0.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(app.terms[0].contents().contains("AGENTE-RETOMA --retomar conv-1"), "{}", app.terms[0].contents());

        // 2) O coletor passa a ver a conversa viva (mesmo pid): uma entrada só.
        let pid = app.terms[0].pid();
        app.set_snapshot(snap(vec![agent_session("conv-1", &cwd, pid)]));
        assert_eq!(app.snap.sessions.len(), 1, "sessão duplicada");
        // Fica anotada para reabrir quando o dashboard abrir de novo.
        let saved: Vec<SavedChat> = serde_json::from_str(&std::fs::read_to_string(open_chats_path().unwrap()).unwrap()).unwrap();
        assert!(saved.iter().any(|c| c.session_id == "conv-1" && c.agent.as_deref() == Some("Falso")));

        // 3) Chat novo (sem conversa): quando o coletor acha a conversa pelo pid,
        //    o chat passa a ter o id dela e vira "reabrível".
        app.terms[0].kill();
        let t0 = Instant::now();
        while !app.terms.is_empty() && t0.elapsed() < Duration::from_secs(10) {
            app.reap_terms();
            std::thread::sleep(Duration::from_millis(50));
        }
        app.chat_focus = false;
        let fake_path = fake.clone();
        app.spawn_agent_chat("Falso", &fake_path, "novo".into(), &cwd, &[], None);
        assert!(!app.terms[0].restorable);
        let pid = app.terms[0].pid();
        app.set_snapshot(snap(vec![agent_session("conv-2", &cwd, pid)]));
        assert_eq!(app.terms[0].key, "conv-2");
        assert!(app.terms[0].restorable);
        assert_eq!(app.snap.sessions.len(), 1);
        assert_eq!(app.chat_key.as_deref(), Some("conv-2"));
        app.terms[0].kill();

        // 4) Viva em outro terminal: x pede para encerrar o processo dela.
        app.terms.clear();
        app.chat_focus = false;
        app.set_snapshot(snap(vec![agent_session("conv-3", &cwd, Some(424242))]));
        app.states[0].select(Some(0));
        key(&mut app, KeyCode::Char('x'), &ctl);
        let c = app.confirm.take().expect("pediu confirmação");
        assert!(matches!(&c.action, Some(Action::KillAgent { session_id }) if session_id == "conv-3"));
        // Enter nela não abre outro processo na mesma conversa.
        key(&mut app, KeyCode::Enter, &ctl);
        assert!(app.terms.is_empty());
        drop(rx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Agente que não está no PATH: o formulário não fecha e diz como instalar.
    #[test]
    fn agente_nao_instalado_mostra_como_instalar() {
        let (ctl, _rx) = mpsc::channel();
        let mut app = App::new();
        app.agents.push(AgentDef {
            name: "Fantasma".into(),
            command: "agente-que-nao-existe-xyz".into(),
            args: Vec::new(),
            models: Vec::new(),
            model_flag: None,
            prompt_flag: None,
            install: Some("npm install -g fantasma".into()),
            resume_flag: None,
        });
        let n_agents = app.agents.len();
        key(&mut app, KeyCode::Char('n'), &ctl);
        app.launcher.as_mut().unwrap().repo = std::env::temp_dir().display().to_string();
        key(&mut app, KeyCode::Tab, &ctl);
        for _ in 0..n_agents - 1 {
            key(&mut app, KeyCode::Right, &ctl);
        }
        key(&mut app, KeyCode::Enter, &ctl);
        let err = app.launcher.as_ref().and_then(|f| f.error.clone()).unwrap_or_default();
        assert!(err.contains("npm install -g fantasma"), "{err}");
        assert!(app.terms.is_empty());
    }

    fn screen(app: &mut App) -> String {
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(160, 45)).unwrap();
        term.draw(|f| crate::ui::draw(f, app)).unwrap();
        let buf = term.backend().buffer().clone();
        (0..buf.area.height).map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>() + "\n").collect()
    }

    /// Uso separado por agente: tokens da sessão do Antigravity, cabeçalho e
    /// tabela por agente, e `v` filtrando um agente só.
    #[test]
    fn uso_separado_por_agente() {
        let (ctl, _rx) = mpsc::channel();
        let today = Local::now().date_naive();
        let tok = |input, cache_read, output| Tokens { input, cache_write: 0, cache_read, output };
        let (claude, agy) = (tok(1_000, 2_000_000, 50_000), tok(10_126, 0, 170));
        let mut s = agent_session("conv-agy", "C:\\proj", Some(4242));
        s.agent = Some(crate::antigravity::AGENT.into());
        s.usage = Usage { tokens: agy, last_turn: agy, turns: 1, model: Some("gemini-teste".into()), ..Default::default() };
        let mut snap = Snapshot { sessions: vec![s], ..Default::default() };
        let mut total = claude;
        total.add(&agy);
        snap.daily.insert(today, total);
        snap.daily_by_agent.insert(today, [(CLAUDE.to_string(), claude), ("Antigravity".to_string(), agy)].into());
        snap.daily_by_model.insert(
            today,
            [(CLAUDE.to_string(), [("claude-opus-5-5".to_string(), claude)].into()), ("Antigravity".to_string(), [("gemini-teste".to_string(), agy)].into())].into(),
        );
        snap.per_minute = vec![0; 60];
        let mut app = App::new();
        app.set_snapshot(snap);
        app.resync();

        // Sessão do Antigravity com tokens: coluna e resumo.
        let text = screen(&mut app);
        let k = crate::i18n::t;
        assert!(text.contains("10.3k"), "{text}");
        assert!(text.contains(k("Contexto atual", "Current context")), "{text}");
        assert!(text.contains("gemini-teste"), "{text}");
        // Cabeçalho com um número por agente.
        assert!(text.contains("Claude 2.1M · Antigravity 10.3k"), "{text}");

        // Painel de uso: modelos agrupados por agente.
        key(&mut app, KeyCode::Char('4'), &ctl);
        let text = screen(&mut app);
        assert!(text.contains("  claude-opus-5-5") && text.contains("  gemini-teste"), "{text}");
        assert!(text.contains("■ Claude") && text.contains("■ Antigravity"), "{text}");

        // `v`: Claude, depois só Antigravity, depois todos de novo.
        key(&mut app, KeyCode::Char('v'), &ctl);
        assert_eq!(app.usage_filter.as_deref(), Some(CLAUDE));
        key(&mut app, KeyCode::Char('v'), &ctl);
        assert_eq!(app.usage_filter.as_deref(), Some("Antigravity"));
        let text = screen(&mut app);
        assert!(!text.contains("claude-opus-5-5"), "{text}");
        assert!(text.contains("gemini-teste") && text.contains("Antigravity 10.3k"), "{text}");
        assert_eq!(app.days(), vec![(today, agy)]);
        key(&mut app, KeyCode::Char('v'), &ctl);
        assert_eq!(app.usage_filter, None);
    }
}
