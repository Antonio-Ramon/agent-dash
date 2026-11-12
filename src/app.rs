//! Estado da UI e tratamento de teclas.

use crate::actions::{Action, LaunchSpec, MODELS};
use crate::collector::Ctl;
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
        match self {
            Panel::Sessions => &["Resumo", "Atividade", "Processos", "Chat"],
            Panel::Agents => &["Resumo", "Atividade"],
            Panel::Tasks => &["Saída", "Detalhes"],
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
}

/// Chat embutido que estava aberto; fica em disco enquanto o chat existir,
/// para ser oferecido de volta se o dashboard fechar com ele aberto.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct SavedChat {
    pub session_id: String,
    pub title: String,
    pub cwd: String,
}

/// Formulário "nova instância".
pub struct LaunchForm {
    /// 0 repositório · 1 modelo · 2 modo · 3 nome · 4 prompt inicial
    pub field: usize,
    pub repo: String,
    pub sug: usize,
    pub model: usize,
    /// 0 aqui no painel · 1 aba nova do terminal · 2 background
    pub mode: usize,
    pub name: String,
    pub prompt: String,
    pub error: Option<String>,
}

pub const LAUNCH_FIELDS: usize = 5;

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
}

fn hidden_path() -> Option<PathBuf> {
    dirs::data_local_dir().map(|d| d.join("agent-dash").join("hidden.json"))
}

fn open_chats_path() -> Option<PathBuf> {
    dirs::data_local_dir().map(|d| d.join("agent-dash").join("open_chats.json"))
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
            zoom: false,
            help: false,
            tab: 0,
            scroll: 0,
            status: None,
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
            .map(|t| SavedChat { session_id: t.key.clone(), title: t.title.clone(), cwd: t.cwd.clone() })
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
        body.push("Estavam abertas aqui quando o dashboard fechou da última vez.".into());
        body.push("y retoma cada conversa de onde parou; n descarta esta lista.".into());
        self.confirm = Some(Confirm {
            title: if n == 1 { "Reabrir a sessão que ficou aberta".into() } else { format!("Reabrir as {n} sessões que ficaram abertas") },
            body,
            action: None,
            quit: false,
            hide: Vec::new(),
            restore: saved,
        });
    }

    fn restore_chats(&mut self, saved: &[SavedChat]) {
        let before = self.terms.len();
        let mut failed = Vec::new();
        for c in saved {
            // Ainda rodando em algum lugar (outra aba, outro dashboard): retomar
            // poria dois processos na mesma conversa.
            if self.snap.sessions.iter().any(|s| s.session_id == c.session_id && s.alive) {
                failed.push(format!("{} (ainda está rodando)", c.title));
                continue;
            }
            let n = self.terms.len();
            self.spawn_chat(c.session_id.clone(), c.title.clone(), &c.cwd, &["--resume".to_string(), c.session_id.clone()]);
            if self.terms.len() == n {
                failed.push(c.title.clone());
            }
        }
        // Reabrir não toma o teclado: Enter entra no chat que interessar.
        self.chat_focus = false;
        self.save_open_chats();
        let ok = self.terms.len() - before;
        if failed.is_empty() {
            self.flash(format!("{ok} sessão(ões) reaberta(s) · Enter entra no chat"));
        } else {
            self.flash(format!("{ok} reaberta(s); não consegui reabrir: {}", failed.join(", ")));
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
            .filter(|s| (self.show_all || s.alive || s.status == "pendente") && !self.is_hidden(&format!("s:{}", s.session_id)))
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

    pub fn days(&self) -> Vec<(NaiveDate, Tokens)> {
        self.snap.daily.iter().rev().map(|(d, t)| (*d, *t)).collect()
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
        self.resync();
    }

    /// O Claude Code troca o id da sessão em alguns momentos (`/clear`, por
    /// exemplo). O chat embutido é reconhecido pelo PID do processo, e a chave
    /// dele acompanha o id atual; assim ele não vira "sessão de fora".
    fn rekey_terms(&mut self) {
        for i in 0..self.terms.len() {
            let Some(pid) = self.terms[i].pid() else { continue };
            let Some(s) = self.snap.sessions.iter().find(|s| s.alive && s.pid == Some(pid)) else { continue };
            if !s.name.is_empty() {
                self.terms[i].title = s.name.clone();
            }
            if s.session_id == self.terms[i].key {
                continue;
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
            let what = if p == Panel::Agents { "nenhum agente" } else { "nenhuma task" };
            self.flash(format!("{what} no momento (a mostra os terminados nas últimas 24h)"));
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
                        self.flash("cancelado");
                    } else {
                        // Recusou: esquece a lista (grava a atual, vazia).
                        self.save_open_chats();
                        self.flash("sessões não reabertas · a mostra as encerradas, Enter retoma uma delas");
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
                self.flash(if self.show_all { "mostrando tudo (últimas 24h)" } else { "mostrando só o que está ativo/recente" });
            }
            KeyCode::Char('f') => {
                self.only_selected = !self.only_selected;
                self.resync();
                self.flash(if self.only_selected { "agentes/tasks: só da sessão selecionada" } else { "agentes/tasks: todas as sessões" });
            }
            KeyCode::Char('r') => {
                let _ = ctl.send(Ctl::Refresh);
                self.tail = None;
                self.flash("atualizando…");
            }
            KeyCode::Char('c') => self.copy(),
            KeyCode::Char('o') => self.open(ctl),
            KeyCode::Char('e') => self.explore(),
            KeyCode::Char('x') => self.stop(),
            KeyCode::Char('d') => self.dismiss(false),
            KeyCode::Char('D') => self.dismiss(true),
            KeyCode::Char('u') => self.undo_dismiss(),
            KeyCode::Char('n') => self.open_launcher(),
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
            self.flash("nada selecionado");
            return;
        }
        let n = text.lines().count();
        match set_clipboard(&text) {
            Ok(()) => self.flash(format!("{n} linha{} copiada{}", if n == 1 { "" } else { "s" }, if n == 1 { "" } else { "s" })),
            Err(e) => self.flash(format!("falha ao copiar: {e}")),
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
            Panel::Sessions => self.selected_session().map(|s| format!("claude --resume {}", s.session_id)),
            Panel::Agents => self.selected_agent().map(|a| a.agent_id.clone()),
            Panel::Tasks => self
                .selected_task()
                .map(|t| t.output_file.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| t.id.clone())),
            Panel::Usage => self.selected_day().map(|(d, t)| format!("{d}: {} tokens", t.total())),
        };
        let Some(text) = text else { return };
        match set_clipboard(&text) {
            Ok(()) => self.flash(format!("copiado: {text}")),
            Err(e) => self.flash(format!("falha ao copiar: {e}")),
        }
    }

    /// `o`: sessão encerrada -> retoma numa aba nova; em background -> attach;
    /// task -> abre a saída; agente -> pasta da sessão.
    fn open(&mut self, ctl: &Sender<Ctl>) {
        match self.focus {
            Panel::Sessions => {
                let Some(s) = self.selected_session().cloned() else { return };
                if s.alive && s.bg_id.is_none() {
                    self.flash(format!("{} já está aberta em outro terminal (pid {})", s.name, s.pid.unwrap_or(0)));
                    return;
                }
                if s.cwd.is_empty() {
                    self.flash("não sei o diretório desta sessão");
                    return;
                }
                let a = match (&s.bg_id, s.alive || s.status == "pendente") {
                    (Some(id), true) => Action::Attach { id: id.clone(), cwd: s.cwd.clone(), name: s.name.clone() },
                    _ => Action::Resume { session_id: s.session_id.clone(), cwd: s.cwd.clone(), name: s.name.clone() },
                };
                let _ = ctl.send(Ctl::Act(a));
                self.flash(format!("abrindo {}…", s.name));
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
            self.flash("nada para abrir aqui");
            return;
        };
        match Command::new("explorer").arg(&cwd).stdout(Stdio::null()).stderr(Stdio::null()).spawn() {
            Ok(_) => self.flash(format!("abrindo {cwd}")),
            Err(e) => self.flash(format!("falha ao abrir: {e}")),
        }
    }

    fn launch_viewer(&mut self) {
        let target: Option<(String, PathBuf)> =
            self.selected_task().and_then(|t| t.output_file.clone()).map(|p| ("notepad".to_string(), p));
        let Some((prog, path)) = target else {
            self.flash("nada para abrir aqui");
            return;
        };
        match Command::new(&prog).arg(&path).stdout(Stdio::null()).stderr(Stdio::null()).spawn() {
            Ok(_) => self.flash(format!("abrindo {}", path.display())),
            Err(e) => self.flash(format!("falha ao abrir: {e}")),
        }
    }

    // ---------- chat embutido ----------

    pub fn term_idx(&self, key: &str) -> Option<usize> {
        self.terms.iter().position(|t| t.key == key)
    }

    fn spawn_chat(&mut self, key: String, title: String, cwd: &str, args: &[String]) {
        match Term::spawn(key.clone(), title.clone(), cwd, args, self.chat_size.0, self.chat_size.1) {
            Ok(mut t) => {
                t.restorable = args.first().map(String::as_str) != Some("attach");
                self.terms.push(t);
                self.save_open_chats();
                self.focus = Panel::Sessions;
                self.tab = 3;
                self.sel_keys[0] = Some(key.clone());
                self.pending_sel = Some(key.clone());
                self.chat_key = Some(key);
                self.chat_focus = true;
                self.resync();
                self.flash(format!("chat {title} aberto · F12 volta ao painel"));
            }
            Err(e) => self.flash(format!("falha ao abrir o chat: {e}")),
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
        let waiting = s.status == "pendente";
        if s.alive && s.bg_id.is_none() {
            self.flash(format!(
                "{} está aberta em outro terminal (pid {}). Para conversar aqui: x encerra, depois Enter retoma no painel",
                s.name,
                s.pid.unwrap_or(0)
            ));
            return;
        }
        if s.cwd.is_empty() {
            self.flash("não sei o diretório desta sessão");
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
                self.flash(format!("chat {} encerrado{}", t.title, if last.is_empty() { String::new() } else { format!(": {last}") }));
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
        let mut body: Vec<String> = self.terms.iter().map(|t| format!("• {}", t.title)).collect();
        body.push("Os chats abertos aqui dentro fecham junto com o dashboard.".into());
        body.push("As conversas ficam salvas; ao abrir o dashboard de novo ele oferece reabri-las.".into());
        self.confirm = Some(Confirm {
            title: format!("Sair e fechar {n} chat{}", if n == 1 { "" } else { "s" }),
            body,
            action: None,
            quit: true,
            hide: Vec::new(),
            restore: Vec::new(),
        });
    }

    // ---------- ações ----------

    fn ask(&mut self, title: impl Into<String>, body: Vec<String>, action: Action) {
        self.confirm = Some(Confirm { title: title.into(), body, action: Some(action), quit: false, hide: Vec::new(), restore: Vec::new() });
    }

    /// `x`: parar a task / encerrar a sessão selecionada (sempre com confirmação).
    fn stop(&mut self) {
        match self.focus {
            Panel::Sessions => {
                let Some(s) = self.selected_session().cloned() else { return };
                if s.status == "pendente" {
                    self.flash(format!("{} não está rodando: está parada esperando sua resposta. o reabre, d remove", s.name));
                    return;
                }
                if !s.alive {
                    self.flash(format!("{} já está encerrada", s.name));
                    return;
                }
                if let (Some(id), None) = (&s.bg_id, s.pid) {
                    self.ask(
                        format!("Parar a sessão em background {}", s.name),
                        vec![format!("Roda `claude stop {id}`."), "A conversa fica salva: `o` reabre depois.".into()],
                        Action::StopBg { id: id.clone(), name: s.name.clone() },
                    );
                } else {
                    let mut body = vec![
                        format!("Finaliza o processo {} e os filhos dele ({} processos).", s.pid.unwrap_or(0), s.procs.len()),
                        "O que estiver sendo feito agora é interrompido.".into(),
                        "A conversa fica salva: depois, `a` mostra as encerradas e `o` retoma.".into(),
                    ];
                    if s.status == "busy" {
                        body.insert(0, "⚠ A sessão está ocupada neste momento.".into());
                    }
                    if s.agents_running + s.tasks_running > 0 {
                        body.insert(0, format!("⚠ {} agentes e {} tasks rodando serão interrompidos.", s.agents_running, s.tasks_running));
                    }
                    self.ask(format!("Encerrar a sessão {}", s.name), body, Action::KillSession { session_id: s.session_id.clone() });
                }
            }
            Panel::Tasks => {
                let Some(t) = self.selected_task().cloned() else { return };
                if t.status != TaskStatus::Running {
                    self.flash("esta task não está rodando (use d para limpar da lista)");
                    return;
                }
                match t.kind {
                    TaskKind::Shell | TaskKind::Monitor => {
                        let mut body = vec![format!("Sessão: {}", t.session_name)];
                        if let Some(c) = &t.command {
                            body.push(format!("Comando: {}", c.chars().take(200).collect::<String>()));
                        }
                        body.push("Finaliza o processo do shell e os filhos dele; a sessão recebe o aviso de que a task terminou.".into());
                        self.ask(
                            format!("Parar {} {}", t.kind.label(), t.id),
                            body,
                            Action::StopTask { session_id: t.session_id.clone(), task_id: t.id.clone() },
                        );
                    }
                    _ => self.flash(format!(
                        "{} roda dentro do processo da sessão {}: pare pela própria sessão (TaskStop) ou encerre a sessão no painel 1",
                        t.kind.label(),
                        t.session_name
                    )),
                }
            }
            Panel::Agents => {
                let Some(a) = self.selected_agent() else { return };
                if a.running {
                    let msg = format!(
                        "subagentes rodam dentro do processo da sessão {}: não dá para parar só um daqui. Encerre a sessão no painel 1 (x)",
                        a.session_name
                    );
                    self.flash(msg);
                } else {
                    self.flash("este agente já terminou (use d para limpar da lista)");
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
                    self.flash("sessão ativa: encerre antes com x");
                    return;
                }
                if let Some(id) = &s.bg_id {
                    self.ask(
                        format!("Remover a sessão em background {}", s.name),
                        vec![format!("Roda `claude rm {id}`: apaga a sessão em background (e a worktree dela, quando for seguro)."), "Não dá para desfazer.".into()],
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
                    self.flash("agente rodando: não dá para limpar");
                    return;
                }
                Some(a) => vec![format!("a:{}", a.agent_id)],
                None => return,
            },
            (Panel::Agents, true) => self.agents().iter().filter(|a| !a.running).map(|a| format!("a:{}", a.agent_id)).collect(),
            (Panel::Tasks, false) => match self.selected_task() {
                Some(t) if t.status == TaskStatus::Running => {
                    self.flash("task rodando: pare antes com x");
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
            self.flash("nada terminado para limpar");
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
        self.flash(format!("{n} {} da lista (u desfaz · a mostra tudo)", if n == 1 { "item limpo" } else { "itens limpos" }));
    }

    fn undo_dismiss(&mut self) {
        if self.last_hidden.is_empty() {
            self.flash("nada para desfazer");
            return;
        }
        let n = self.last_hidden.len();
        for k in std::mem::take(&mut self.last_hidden) {
            self.hidden.remove(&k);
        }
        self.save_hidden();
        self.resync();
        self.flash(format!("{n} itens de volta na lista"));
    }

    fn open_launcher(&mut self) {
        let repo = self.selected_session().map(|s| s.cwd.clone()).unwrap_or_default();
        self.launcher = Some(LaunchForm {
            field: 0,
            repo,
            sug: 0,
            model: 0,
            mode: 0,
            name: String::new(),
            prompt: String::new(),
            error: None,
        });
    }

    fn launcher_key(&mut self, k: KeyEvent, ctl: &Sender<Ctl>) {
        let repos = self.snap.repos.clone();
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let Some(f) = self.launcher.as_mut() else { return };
        f.error = None;
        let n_sug = f.suggestions(&repos).len();
        match k.code {
            KeyCode::Esc => {
                self.launcher = None;
                return;
            }
            KeyCode::Tab => f.field = (f.field + 1) % LAUNCH_FIELDS,
            KeyCode::BackTab => f.field = (f.field + LAUNCH_FIELDS - 1) % LAUNCH_FIELDS,
            KeyCode::Down if f.field == 0 && n_sug > 0 => f.sug = (f.sug + 1).min(n_sug - 1),
            KeyCode::Up if f.field == 0 => f.sug = f.sug.saturating_sub(1),
            KeyCode::Down => f.field = (f.field + 1) % LAUNCH_FIELDS,
            KeyCode::Up => f.field = (f.field + LAUNCH_FIELDS - 1) % LAUNCH_FIELDS,
            KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') if f.field == 1 || f.field == 2 => {
                let fwd = !matches!(k.code, KeyCode::Left);
                if f.field == 1 {
                    f.model = if fwd { (f.model + 1) % MODELS.len() } else { (f.model + MODELS.len() - 1) % MODELS.len() };
                } else {
                    f.mode = if fwd { (f.mode + 1) % 3 } else { (f.mode + 2) % 3 };
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
                    f.error = Some("escolha um repositório da lista ou digite um caminho que exista".into());
                    return;
                };
                let spec = LaunchSpec {
                    cwd: cwd.clone(),
                    model: if f.model == 0 { None } else { Some(MODELS[f.model].to_string()) },
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
                let modo = if spec.background { "em background" } else { "numa aba nova" };
                self.launcher = None;
                let _ = ctl.send(Ctl::Act(Action::Launch(spec)));
                self.flash(format!("iniciando instância {modo} em {cwd}…"));
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
            let lines = read_tail(path, len).unwrap_or_else(|e| vec![format!("(não consegui ler: {e})")]);
            self.tail = Some(TailCache { path: path.to_path_buf(), len, lines });
        }
        &self.tail.as_ref().unwrap().lines
    }
}

fn text_field(f: &mut LaunchForm) -> Option<&mut String> {
    match f.field {
        0 => Some(&mut f.repo),
        3 => Some(&mut f.name),
        4 => Some(&mut f.prompt),
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
            return Err(err("área de transferência ocupada"));
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
    Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "área de transferência só no Windows"))
}
