//! Thread de coleta: lê `~/.claude/sessions/*.json`, verifica processos,
//! acompanha os transcripts e envia um `Snapshot` para a UI a cada tick.

use crate::actions::{self, Action, ProcCand};
use crate::model::*;
use crate::scan::{one_line, FileState, UsageSink};
use chrono::{DateTime, Local, NaiveDate, TimeZone};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

pub enum Ctl {
    Refresh,
    Act(Action),
    Quit,
}

/// Sessão em background (`claude --bg`), listada por `claude agents --json --all`.
#[derive(Clone, Debug)]
pub struct BgSession {
    pub id: String,
    pub session_id: String,
    pub cwd: String,
    pub name: String,
    pub state: String,
    pub started_at: Option<DateTime<Local>>,
    /// O que a sessão está esperando do usuário (`jobs/<id>/state.json`).
    pub needs: Option<String>,
}

impl BgSession {
    /// Só está viva se trabalhando ou com worker de pé. `blocked` sem worker é uma
    /// sessão que o serviço aposentou e que só espera resposta: não há o que parar.
    pub fn alive(&self, workers: &str) -> bool {
        matches!(self.state.as_str(), "working" | "running") || (!self.id.is_empty() && workers.contains(&self.id))
    }

    pub fn pending(&self) -> bool {
        self.state == "blocked"
    }
}

/// Workers vivos do serviço de background (`daemon/roster.json`), como texto
/// para busca por id. Vazio se o supervisor não está rodando.
fn daemon_workers(claude_dir: &Path, sys: &System) -> String {
    let Ok(txt) = fs::read_to_string(claude_dir.join("daemon").join("roster.json")) else { return String::new() };
    let Ok(v) = serde_json::from_str::<Value>(&txt) else { return String::new() };
    let sup_alive = v
        .get("supervisorPid")
        .and_then(|p| p.as_u64())
        .is_some_and(|p| sys.process(Pid::from_u32(p as u32)).is_some());
    if !sup_alive {
        return String::new();
    }
    v.get("workers").map(|w| w.to_string()).unwrap_or_default()
}

fn job_needs(claude_dir: &Path, id: &str) -> Option<String> {
    let txt = fs::read_to_string(claude_dir.join("jobs").join(id).join("state.json")).ok()?;
    let v: Value = serde_json::from_str(&txt).ok()?;
    v.get("needs").and_then(|n| n.as_str()).map(|n| one_line(n, 300)).filter(|n| !n.is_empty())
}

fn fetch_bg(claude_dir: &Path) -> Option<Vec<BgSession>> {
    let out = actions::claude_cli(&["agents".into(), "--json".into(), "--all".into()], None).ok()?;
    let v: Value = serde_json::from_str(&out).ok()?;
    Some(
        v.as_array()?
            .iter()
            .filter(|x| x.get("kind").and_then(|k| k.as_str()) == Some("background"))
            .filter_map(|x| {
                let s = |k: &str| x.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
                let id = x.get("id")?.as_str()?.to_string();
                Some(BgSession {
                    needs: job_needs(claude_dir, &id),
                    id,
                    session_id: s("sessionId"),
                    cwd: s("cwd"),
                    name: s("name"),
                    state: s("state"),
                    started_at: ms_to_local(x.get("startedAt")),
                })
            })
            .collect(),
    )
}

const SHELLS: [&str; 6] = ["bash.exe", "sh.exe", "pwsh.exe", "powershell.exe", "cmd.exe", "zsh.exe"];

/// Mata a árvore de processos a partir de `root` (filhos primeiro).
fn kill_tree(sys: &System, children: &HashMap<Pid, Vec<Pid>>, root: Pid) -> usize {
    let mut order = Vec::new();
    let mut stack = vec![(root, 0usize)];
    while let Some((pid, d)) = stack.pop() {
        order.push((pid, d));
        if d < 12
            && let Some(ch) = children.get(&pid) {
                stack.extend(ch.iter().map(|c| (*c, d + 1)));
            }
    }
    order.sort_by(|a, b| b.1.cmp(&a.1));
    order.iter().filter(|(pid, _)| sys.process(*pid).is_some_and(|p| p.kill())).count()
}

fn children_map(sys: &System) -> HashMap<Pid, Vec<Pid>> {
    let mut children: HashMap<Pid, Vec<Pid>> = HashMap::new();
    for (pid, p) in sys.processes() {
        if let Some(pp) = p.parent() {
            children.entry(pp).or_default().push(*pid);
        }
    }
    children
}

const RECENT: Duration = Duration::from_secs(24 * 3600);

#[derive(Default)]
struct Global {
    /// id da resposta -> (dia, modelo, tokens, minuto)
    msgs: HashMap<String, (NaiveDate, String, Tokens, i64)>,
    daily: BTreeMap<NaiveDate, Tokens>,
    daily_model: BTreeMap<NaiveDate, BTreeMap<String, Tokens>>,
    daily_sessions: BTreeMap<NaiveDate, HashSet<String>>,
    per_minute: BTreeMap<i64, u64>,
}

impl UsageSink for Global {
    fn report(&mut self, sid: &str, id: &str, ts: Option<DateTime<Local>>, model: &str, t: Tokens) {
        let Some(ts) = ts else { return };
        let day = ts.date_naive();
        let minute = ts.timestamp().div_euclid(60);
        if let Some((d, m, old, min)) = self.msgs.get(id).cloned() {
            if old == t {
                return; // mesma resposta já contada (ex.: sessão retomada copia o histórico)
            }
            if let Some(x) = self.daily.get_mut(&d) {
                x.sub(&old);
            }
            if let Some(x) = self.daily_model.get_mut(&d).and_then(|mm| mm.get_mut(&m)) {
                x.sub(&old);
            }
            if let Some(x) = self.per_minute.get_mut(&min) {
                *x = x.saturating_sub(old.total());
            }
        }
        self.daily.entry(day).or_default().add(&t);
        self.daily_model.entry(day).or_default().entry(model.to_string()).or_default().add(&t);
        self.daily_sessions.entry(day).or_default().insert(sid.to_string());
        *self.per_minute.entry(minute).or_default() += t.total();
        self.msgs.insert(id.to_string(), (day, model.to_string(), t, minute));
    }
}

#[derive(Clone, Debug)]
struct Disc {
    path: PathBuf,
    session_id: String,
    agent_id: Option<String>,
    workflow: Option<String>,
    mtime: SystemTime,
    len: u64,
}

#[derive(Clone, Debug, Default)]
struct Meta {
    pid: u32,
    session_id: String,
    cwd: String,
    started_at: Option<DateTime<Local>>,
    version: String,
    kind: String,
    name: String,
    status: String,
    /// Início do processo em segundos unix (de `procStart`, FILETIME do Windows).
    proc_start: Option<u64>,
}

/// FILETIME (100 ns desde 1601) -> segundos unix.
fn filetime_to_unix(v: &Value) -> Option<u64> {
    let ft: u64 = match v {
        Value::String(s) => s.parse().ok()?,
        Value::Number(n) => n.as_u64()?,
        _ => return None,
    };
    ft.checked_sub(116_444_736_000_000_000).map(|x| x / 10_000_000)
}

pub struct Collector {
    claude_dir: PathBuf,
    temp_claude: PathBuf,
    sys: System,
    files: HashMap<PathBuf, FileState>,
    global: Global,
    disc: Vec<Disc>,
    last_disc: Option<Instant>,
    task_dirs: HashMap<String, PathBuf>,
    history_pending: bool,
    metas: HashMap<String, Meta>,
    live: HashSet<String>,
    bg: Arc<Mutex<Option<Vec<BgSession>>>>,
    bg_poke: Option<Sender<()>>,
    notices: Arc<Mutex<Vec<String>>>,
    repos_cache: Option<(Instant, Vec<String>)>,
}

fn ms_to_local(v: Option<&Value>) -> Option<DateTime<Local>> {
    v.and_then(|x| x.as_i64()).and_then(|ms| Local.timestamp_millis_opt(ms).single())
}

fn read_metas(dir: &Path) -> Vec<Meta> {
    let mut out = Vec::new();
    let Ok(rd) = fs::read_dir(dir) else { return out };
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        let Ok(txt) = fs::read_to_string(&p) else { continue };
        let Ok(v) = serde_json::from_str::<Value>(&txt) else { continue };
        let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
        let Some(pid) = v.get("pid").and_then(|x| x.as_u64()) else { continue };
        out.push(Meta {
            pid: pid as u32,
            session_id: s("sessionId"),
            cwd: s("cwd"),
            started_at: ms_to_local(v.get("startedAt")),
            version: s("version"),
            kind: s("kind"),
            name: s("name"),
            status: s("status"),
            proc_start: v.get("procStart").and_then(filetime_to_unix),
        });
    }
    out
}

fn discover(projects: &Path) -> Vec<Disc> {
    let mut out = Vec::new();
    let Ok(rd) = fs::read_dir(projects) else { return out };
    for proj in rd.flatten() {
        let pp = proj.path();
        if !pp.is_dir() {
            continue;
        }
        let Ok(rd2) = fs::read_dir(&pp) else { continue };
        for e in rd2.flatten() {
            let p = e.path();
            let Ok(md) = e.metadata() else { continue };
            if md.is_file() && p.extension().and_then(|x| x.to_str()) == Some("jsonl") {
                let sid = p.file_stem().unwrap_or_default().to_string_lossy().to_string();
                out.push(Disc {
                    path: p,
                    session_id: sid,
                    agent_id: None,
                    workflow: None,
                    mtime: md.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                    len: md.len(),
                });
            } else if md.is_dir() {
                let sid = e.file_name().to_string_lossy().to_string();
                let sub = p.join("subagents");
                if sub.is_dir() {
                    walk_agents(&sub, &sid, None, &mut out, 0);
                }
            }
        }
    }
    out
}

fn walk_agents(dir: &Path, sid: &str, wf: Option<String>, out: &mut Vec<Disc>, depth: usize) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        let Ok(md) = e.metadata() else { continue };
        let name = e.file_name().to_string_lossy().to_string();
        if md.is_dir() && depth < 3 {
            let wf = if name.starts_with("wf_") { Some(name.clone()) } else { wf.clone() };
            walk_agents(&p, sid, wf, out, depth + 1);
        } else if md.is_file() && name.starts_with("agent-") && name.ends_with(".jsonl") {
            let aid = name["agent-".len()..name.len() - ".jsonl".len()].to_string();
            out.push(Disc {
                path: p,
                session_id: sid.to_string(),
                agent_id: Some(aid),
                workflow: wf.clone(),
                mtime: md.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                len: md.len(),
            });
        }
    }
}

fn find_task_dir(cache: &mut HashMap<String, PathBuf>, temp: &Path, sid: &str) -> Option<PathBuf> {
    if let Some(p) = cache.get(sid) {
        return Some(p.clone());
    }
    let rd = fs::read_dir(temp).ok()?;
    for e in rd.flatten() {
        let cand = e.path().join(sid).join("tasks");
        if cand.is_dir() {
            cache.insert(sid.to_string(), cand.clone());
            return Some(cand);
        }
    }
    None
}

/// A sessão está viva se o PID existe, é do Claude e (quando dá para conferir)
/// começou no mesmo instante registrado na sessão; isso descarta PID reaproveitado.
fn is_session_alive(sys: &System, m: &Meta) -> bool {
    sys.process(Pid::from_u32(m.pid))
        .map(|p| {
            let n = p.name().to_string_lossy().to_lowercase();
            let name_ok = n.contains("claude") || n.contains("node") || n.contains("bun");
            let start_ok = m.proc_start.is_none_or(|s| p.start_time().abs_diff(s) <= 3);
            name_ok && start_ok
        })
        .unwrap_or(false)
}

fn proc_tree(sys: &System, children: &HashMap<Pid, Vec<Pid>>, root: Pid) -> Vec<ProcInfo> {
    let mut out = Vec::new();
    let mut stack = vec![(root, 0usize)];
    while let Some((pid, depth)) = stack.pop() {
        if depth > 0 {
            let Some(p) = sys.process(pid) else { continue };
            let name = p.name().to_string_lossy().to_string();
            if name.eq_ignore_ascii_case("conhost.exe") {
                continue;
            }
            let cmd = p.cmd().iter().map(|s| s.to_string_lossy()).collect::<Vec<_>>().join(" ");
            out.push(ProcInfo {
                pid: pid.as_u32(),
                name,
                cpu: p.cpu_usage(),
                mem: p.memory(),
                cmd: one_line(&cmd, 300),
                depth,
            });
        }
        if depth < 6
            && let Some(ch) = children.get(&pid) {
                for c in ch.iter().rev() {
                    stack.push((*c, depth + 1));
                }
            }
    }
    out
}

fn trim_minutes(u: &Usage) -> Usage {
    let mut u = u.clone();
    let now_min = Local::now().timestamp().div_euclid(60);
    u.per_minute.retain(|m, _| *m > now_min - 60);
    u
}

impl Collector {
    pub fn new() -> anyhow::Result<Self> {
        let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("não achei o diretório home"))?;
        let claude_dir = std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from).unwrap_or_else(|| home.join(".claude"));
        Ok(Self {
            claude_dir,
            temp_claude: std::env::temp_dir().join("claude"),
            sys: System::new(),
            files: HashMap::new(),
            global: Global::default(),
            disc: Vec::new(),
            last_disc: None,
            task_dirs: HashMap::new(),
            history_pending: true,
            metas: HashMap::new(),
            live: HashSet::new(),
            bg: Arc::new(Mutex::new(None)),
            bg_poke: None,
            notices: Arc::new(Mutex::new(Vec::new())),
            repos_cache: None,
        })
    }

    /// Lista as sessões em background agora (bloqueia ~1 s). Usado no `--dump`.
    pub fn refresh_bg_blocking(&mut self) {
        let v = fetch_bg(&self.claude_dir);
        *self.bg.lock().unwrap() = v;
    }

    /// Thread que atualiza a lista de sessões em background a cada 30 s
    /// (ou logo depois de uma ação).
    fn start_bg_poller(&mut self) {
        let (tx, rx) = mpsc::channel::<()>();
        self.bg_poke = Some(tx);
        let bg = self.bg.clone();
        let dir = self.claude_dir.clone();
        let _ = thread::Builder::new().name("bg-poller".into()).spawn(move || loop {
            let v = fetch_bg(&dir);
            *bg.lock().unwrap() = v;
            // `claude agents` sobe um node (~1 s de CPU): não dá para chamar toda hora.
            match rx.recv_timeout(Duration::from_secs(30)) {
                Err(RecvTimeoutError::Disconnected) => break,
                _ => {
                    // junta pedidos seguidos num só
                    while rx.try_recv().is_ok() {}
                }
            }
        });
    }

    fn notify(&self, msg: impl Into<String>) {
        self.notices.lock().unwrap().push(msg.into());
    }

    fn poke_bg(&self) {
        if let Some(tx) = &self.bg_poke {
            let _ = tx.send(());
        }
    }

    fn bg_list(&self) -> Vec<BgSession> {
        self.bg.lock().unwrap().clone().unwrap_or_default()
    }

    /// Executa uma ação. As lentas (CLI do claude) rodam numa thread à parte e
    /// o resultado chega como aviso num snapshot seguinte.
    pub fn act(&mut self, a: Action) {
        match a {
            Action::StopTask { session_id, task_id } => {
                let msg = self.stop_task(&session_id, &task_id);
                self.notify(msg);
            }
            Action::KillSession { session_id } => {
                let msg = self.kill_session(&session_id);
                self.notify(msg);
            }
            Action::Launch(spec) if !spec.background => {
                let title = spec.name.clone().filter(|n| !n.trim().is_empty()).unwrap_or_else(|| "claude".into());
                let msg = match actions::open_terminal(&spec.cwd, &title, &actions::launch_args(&spec)) {
                    Ok(()) => format!("nova instância aberta em {}", spec.cwd),
                    Err(e) => format!("falha ao abrir: {e}"),
                };
                self.notify(msg);
            }
            Action::Resume { session_id, cwd, name } => {
                let msg = match actions::open_terminal(&cwd, &name, &["--resume".into(), session_id]) {
                    Ok(()) => format!("retomando {name} numa aba nova"),
                    Err(e) => format!("falha ao retomar: {e}"),
                };
                self.notify(msg);
            }
            Action::Attach { id, cwd, name } => {
                let msg = match actions::open_terminal(&cwd, &name, &["attach".into(), id]) {
                    Ok(()) => format!("abrindo {name} numa aba nova"),
                    Err(e) => format!("falha ao abrir: {e}"),
                };
                self.notify(msg);
            }
            slow => {
                let notices = self.notices.clone();
                let poke = self.bg_poke.clone();
                let _ = thread::Builder::new().name("action".into()).spawn(move || {
                    let msg = match slow {
                        Action::Launch(spec) => match actions::claude_cli(&actions::launch_args(&spec), Some(&spec.cwd)) {
                            Ok(out) => format!("sessão em background iniciada: {}", one_line(&out, 120)),
                            Err(e) => format!("falha ao iniciar em background: {}", one_line(&e, 160)),
                        },
                        Action::StopBg { id, name } => match actions::claude_cli(&["stop".into(), id], None) {
                            Ok(_) => format!("sessão {name} parada"),
                            Err(e) => format!("falha ao parar {name}: {}", one_line(&e, 160)),
                        },
                        Action::RemoveBg { id, name } => match actions::claude_cli(&["rm".into(), id], None) {
                            Ok(_) => format!("sessão {name} removida"),
                            Err(e) => format!("falha ao remover {name}: {}", one_line(&e, 160)),
                        },
                        _ => return,
                    };
                    notices.lock().unwrap().push(msg);
                    if let Some(tx) = poke {
                        let _ = tx.send(());
                    }
                });
            }
        }
        self.poke_bg();
    }

    pub fn stop_task(&mut self, sid: &str, tid: &str) -> String {
        let Some(t) = self.files.values().filter(|f| f.session_id == sid).find_map(|f| f.tasks.get(tid)).cloned() else {
            return format!("task {tid} não encontrada");
        };
        if matches!(t.kind, TaskKind::Agent | TaskKind::Workflow) {
            return format!(
                "{} roda dentro do processo da sessão: não dá para parar só ele daqui. Use TaskStop na sessão ou encerre a sessão (painel 1, x)",
                t.kind.label()
            );
        }
        if t.effective_status(Local::now()).is_some() {
            return format!("task {tid} já terminou");
        }
        let children = children_map(&self.sys);
        // Candidatos: shells descendentes da sessão (ou de qualquer processo, se a
        // sessão está em background e não temos o PID dela).
        let root = self
            .metas
            .get(sid)
            .filter(|m| m.pid > 0 && is_session_alive(&self.sys, m))
            .map(|m| Pid::from_u32(m.pid));
        let mut pool: Vec<Pid> = Vec::new();
        match root {
            Some(r) => {
                let mut stack = vec![(r, 0usize)];
                while let Some((pid, d)) = stack.pop() {
                    if d > 0 {
                        pool.push(pid);
                    }
                    if d < 4
                        && let Some(ch) = children.get(&pid) {
                            stack.extend(ch.iter().map(|c| (*c, d + 1)));
                        }
                }
            }
            None => pool.extend(self.sys.processes().keys().copied()),
        }
        let cands: Vec<ProcCand> = pool
            .iter()
            .filter_map(|pid| {
                let p = self.sys.process(*pid)?;
                let name = p.name().to_string_lossy().to_lowercase();
                if !SHELLS.contains(&name.as_str()) {
                    return None;
                }
                Some(ProcCand {
                    pid: pid.as_u32(),
                    parent: p.parent().map(|x| x.as_u32()),
                    start: p.start_time(),
                    cmd: p.cmd().iter().map(|s| s.to_string_lossy()).collect::<Vec<_>>().join(" "),
                })
            })
            .collect();
        let pid = match actions::pick_task_process(
            &cands,
            t.raw_command.as_deref(),
            t.requested.map(|x| x.timestamp()),
            t.started.map(|x| x.timestamp()),
        ) {
            Ok(p) => p,
            Err(e) => return format!("não parei {tid}: {e}"),
        };
        let n = kill_tree(&self.sys, &children, Pid::from_u32(pid));
        format!("{} {tid} parado (pid {pid}, {n} processos finalizados)", t.kind.label())
    }

    fn kill_session(&mut self, sid: &str) -> String {
        let Some(m) = self.metas.get(sid).cloned() else {
            return "sessão sem processo conhecido".into();
        };
        if !is_session_alive(&self.sys, &m) {
            return format!("{} já não está rodando", m.name);
        }
        let children = children_map(&self.sys);
        // Não derrubar o próprio dashboard (se foi aberto de dentro desta sessão).
        let me = Pid::from_u32(std::process::id());
        let mut cur = self.sys.process(me).and_then(|p| p.parent());
        while let Some(pp) = cur {
            if pp.as_u32() == m.pid {
                return format!("o agent-dash está rodando dentro de {}: encerre-a pelo próprio terminal", m.name);
            }
            cur = self.sys.process(pp).and_then(|p| p.parent());
        }
        // O shell de onde o claude foi aberto continua na aba; ele herda o terminal
        // no estado em que o Claude deixou (mouse capturado etc.) e precisa de limpeza.
        let shell = self.sys.process(Pid::from_u32(m.pid)).and_then(|p| p.parent()).filter(|pp| self.sys.process(*pp).is_some());
        let n = kill_tree(&self.sys, &children, Pid::from_u32(m.pid));
        if let Some(pp) = shell {
            actions::spawn_console_reset(pp.as_u32());
        }
        format!("sessão {} encerrada (pid {}, {n} processos). Para retomar: `a` mostra as encerradas, `o` reabre", m.name, m.pid)
    }

    /// Diretórios conhecidos: os das sessões (mais recentes primeiro) e os
    /// projetos de `~/.claude.json`. Só os que ainda existem.
    fn repos(&mut self, sessions: &[SessionInfo]) -> Vec<String> {
        let fresh = self.repos_cache.as_ref().is_some_and(|(t, _)| t.elapsed() < Duration::from_secs(30));
        if !fresh {
            let cfg = self.claude_dir.parent().map(|h| h.join(".claude.json"));
            let mut v = Vec::new();
            if let Some(Ok(txt)) = cfg.map(fs::read_to_string)
                && let Ok(j) = serde_json::from_str::<Value>(&txt)
                && let Some(obj) = j.get("projects").and_then(|p| p.as_object()) {
                    v.extend(obj.keys().cloned());
                }
            self.repos_cache = Some((Instant::now(), v));
        }
        let mut out: Vec<String> = Vec::new();
        let mut seen = HashSet::new();
        let from_cfg = self.repos_cache.as_ref().map(|(_, v)| v.clone()).unwrap_or_default();
        for p in sessions.iter().map(|s| s.cwd.clone()).chain(from_cfg) {
            let p = p.replace('/', "\\");
            let p = p.trim_end_matches('\\').to_string();
            if p.is_empty() || !seen.insert(p.to_lowercase()) {
                continue;
            }
            if Path::new(&p).is_dir() {
                out.push(p);
            }
        }
        out
    }

    pub fn history_pending(&self) -> bool {
        self.history_pending
    }

    pub fn tick(&mut self) -> Snapshot {
        let t0 = Instant::now();
        let metas = read_metas(&self.claude_dir.join("sessions"));
        self.sys.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing().with_cpu().with_memory().with_cmd(UpdateKind::OnlyIfNotSet),
        );

        let meta_by_sid: HashMap<String, Meta> = metas
            .into_iter()
            .filter(|m| !m.session_id.is_empty())
            .map(|m| (m.session_id.clone(), m))
            .collect();
        let mut live: HashSet<String> = meta_by_sid
            .values()
            .filter(|m| is_session_alive(&self.sys, m))
            .map(|m| m.session_id.clone())
            .collect();
        let workers = daemon_workers(&self.claude_dir, &self.sys);
        live.extend(self.bg_list().into_iter().filter(|b| b.alive(&workers)).map(|b| b.session_id));
        self.metas = meta_by_sid.clone();
        self.live = live.clone();

        if self.last_disc.is_none_or(|t| t.elapsed() > Duration::from_secs(4)) {
            self.disc = discover(&self.claude_dir.join("projects"));
            self.last_disc = Some(Instant::now());
        }
        let now = SystemTime::now();
        let is_recent = |d: &Disc| now.duration_since(d.mtime).map_or(true, |x| x < RECENT);
        let disc = self.disc.clone();

        // 1) Sessões vivas (e seus subagentes): sempre completas, sem limite de tempo.
        // 2) Transcripts das últimas 24h: modo completo, com limite por tick.
        // 3) Histórico: modo leve (só tokens), com limite por tick.
        let budget = Duration::from_millis(300);
        let mut pending = false;
        for pass in 0..3 {
            for d in &disc {
                let is_live = live.contains(&d.session_id);
                let full = is_live || is_recent(d);
                let in_pass = match pass {
                    0 => is_live,
                    1 => full && !is_live,
                    _ => !full,
                };
                if !in_pass {
                    continue;
                }
                let st = self
                    .files
                    .entry(d.path.clone())
                    .or_insert_with(|| FileState::new(d.path.clone(), d.session_id.clone(), full));
                if full && !st.full {
                    *st = FileState::new(d.path.clone(), d.session_id.clone(), true);
                }
                if pass > 0 && st.offset >= d.len {
                    continue;
                }
                if pass > 0 && t0.elapsed() > budget {
                    pending = true;
                    continue;
                }
                let _ = st.ingest(&mut self.global);
            }
        }
        self.history_pending = pending;

        let history_total: u64 = disc.iter().map(|d| d.len).sum();
        let history_done: u64 = disc
            .iter()
            .map(|d| self.files.get(&d.path).map_or(0, |s| s.offset.min(d.len)))
            .sum();

        self.build(t0, &disc, &meta_by_sid, &live, history_done, history_total)
    }

    fn build(
        &mut self,
        _t0: Instant,
        disc: &[Disc],
        metas: &HashMap<String, Meta>,
        live: &HashSet<String>,
        history_done: u64,
        history_total: u64,
    ) -> Snapshot {
        let now = SystemTime::now();
        let is_recent = |d: &Disc| now.duration_since(d.mtime).map_or(true, |x| x < RECENT);

        // Transcript principal de cada sessão (o mais recente, se houver mais de um).
        let mut main_by_sid: HashMap<&str, &Disc> = HashMap::new();
        for d in disc.iter().filter(|d| d.agent_id.is_none()) {
            let e = main_by_sid.entry(&d.session_id).or_insert(d);
            if d.mtime > e.mtime {
                *e = d;
            }
        }

        let children = children_map(&self.sys);
        let bg_by_sid: HashMap<String, BgSession> = self.bg_list().into_iter().map(|b| (b.session_id.clone(), b)).collect();

        // Quais sessões aparecem: as que têm arquivo em sessions/ + as com transcript nas últimas 24h.
        let mut sids: Vec<String> = metas.keys().cloned().collect();
        for (sid, d) in &main_by_sid {
            if is_recent(d) && !metas.contains_key(*sid) {
                sids.push(sid.to_string());
            }
        }
        for sid in bg_by_sid.keys() {
            if !sids.contains(sid) {
                sids.push(sid.clone());
            }
        }

        let mut sessions = Vec::new();
        let mut tasks = Vec::new();
        for sid in &sids {
            let meta = metas.get(sid);
            let bg = bg_by_sid.get(sid);
            let alive = live.contains(sid);
            let meta_alive = alive && meta.is_some_and(|m| is_session_alive(&self.sys, m));
            let main = main_by_sid.get(sid.as_str()).copied();
            let fs_ = main.and_then(|d| self.files.get(&d.path));
            let title = fs_.and_then(|f| f.title.clone());
            let name = meta
                .map(|m| m.name.clone())
                .filter(|n| !n.is_empty())
                .or_else(|| bg.map(|b| b.name.clone()).filter(|n| !n.is_empty()))
                .or_else(|| title.clone())
                .unwrap_or_else(|| sid.chars().take(8).collect());
            let status = if meta_alive {
                meta.map(|m| m.status.clone()).filter(|s| !s.is_empty()).unwrap_or_else(|| "?".into())
            } else if alive && let Some(b) = bg {
                b.state.clone()
            } else if bg.is_some_and(|b| b.pending()) {
                "pendente".into()
            } else {
                "encerrada".into()
            };
            let (cpu, mem, procs) = match meta {
                Some(m) if meta_alive => {
                    let pid = Pid::from_u32(m.pid);
                    let p = self.sys.process(pid);
                    (
                        p.map_or(0.0, |p| p.cpu_usage()),
                        p.map_or(0, |p| p.memory()),
                        proc_tree(&self.sys, &children, pid),
                    )
                }
                _ => (0.0, 0, Vec::new()),
            };

            if let Some(f) = fs_ {
                let tdir = find_task_dir(&mut self.task_dirs, &self.temp_claude, sid);
                let now_l = Local::now();
                for (id, t) in &f.tasks {
                    let status = match t.effective_status(now_l) {
                        Some(s) => TaskStatus::Done(s),
                        None if alive => TaskStatus::Running,
                        None => TaskStatus::Orphan,
                    };
                    let output_file = t
                        .output_file
                        .clone()
                        .or_else(|| tdir.as_ref().map(|d| d.join(format!("{id}.output"))))
                        .filter(|p| p.exists());
                    tasks.push(TaskInfo {
                        id: id.clone(),
                        kind: t.kind,
                        description: t.description.clone(),
                        command: t.command.clone(),
                        status,
                        started: t.started,
                        ended: t.ended.or(t.expires.filter(|e| *e < now_l)),
                        output_file,
                        session_id: sid.clone(),
                        session_name: name.clone(),
                    });
                }
            }

            sessions.push(SessionInfo {
                pid: meta.map(|m| m.pid),
                session_id: sid.clone(),
                name,
                cwd: meta
                    .map(|m| m.cwd.clone())
                    .filter(|c| !c.is_empty())
                    .or_else(|| bg.map(|b| b.cwd.clone()).filter(|c| !c.is_empty()))
                    .or_else(|| fs_.and_then(|f| f.cwd.clone()))
                    .unwrap_or_default(),
                status,
                kind: meta
                    .map(|m| m.kind.clone())
                    .filter(|k| !k.is_empty())
                    .or_else(|| bg.map(|_| "background".to_string()))
                    .unwrap_or_default(),
                bg_id: bg.map(|b| b.id.clone()),
                pending: bg.and_then(|b| b.needs.clone()).filter(|_| bg.is_some_and(|b| b.pending())),
                version: meta.map(|m| m.version.clone()).unwrap_or_default(),
                started_at: meta
                    .and_then(|m| m.started_at)
                    .or_else(|| bg.and_then(|b| b.started_at))
                    .or_else(|| fs_.and_then(|f| f.usage.first_ts)),
                alive,
                title,
                last_prompt: fs_.and_then(|f| f.last_prompt.clone()),
                usage: fs_.map(|f| trim_minutes(&f.usage)).unwrap_or_default(),
                agents_usage: Tokens::default(),
                events: fs_.map(|f| f.events.iter().cloned().collect()).unwrap_or_default(),
                transcript: main.map(|d| d.path.clone()),
                cpu,
                mem,
                procs,
                agents_total: 0,
                agents_running: 0,
                tasks_running: 0,
            });
        }

        // Subagentes
        let mut agents = Vec::new();
        for d in disc.iter().filter(|d| d.agent_id.is_some()) {
            let Some(st) = self.files.get(&d.path).filter(|s| s.full) else { continue };
            let aid = d.agent_id.clone().unwrap_or_default();
            let Some(sess) = sessions.iter().find(|s| s.session_id == d.session_id) else { continue };
            let parent = main_by_sid.get(d.session_id.as_str()).and_then(|m| self.files.get(&m.path));

            let meta_path = d.path.with_file_name(format!("agent-{aid}.meta.json"));
            let meta: Value = fs::read_to_string(&meta_path)
                .ok()
                .and_then(|t| serde_json::from_str(&t).ok())
                .unwrap_or(Value::Null);
            let ms = |k: &str| meta.get(k).and_then(|x| x.as_str()).map(str::to_string);
            let finished = meta.get("finished").and_then(|x| x.as_bool()).unwrap_or(false);
            let async_task = parent.and_then(|p| p.tasks.get(&aid));
            let done = finished
                || parent.is_some_and(|p| p.agent_done.contains_key(&aid))
                || async_task.is_some_and(|t| t.status.is_some());
            let workflow_running = d.workflow.is_some()
                && parent.is_some_and(|p| p.tasks.values().any(|t| t.kind == TaskKind::Workflow && t.status.is_none()));
            let running = !done
                && sess.alive
                && (async_task.is_some() || workflow_running || (d.workflow.is_none() && !matches!(sess.status.as_str(), "idle" | "blocked")));

            let first_prompt = st.events.iter().find(|e| e.kind == EventKind::Prompt).map(|e| one_line(&e.text, 120));
            agents.push(AgentInfo {
                agent_id: aid,
                agent_type: ms("agentType").unwrap_or_else(|| "agente".into()),
                description: ms("description").or_else(|| ms("label")).or(first_prompt).unwrap_or_default(),
                workflow: d.workflow.clone(),
                session_id: d.session_id.clone(),
                session_name: sess.name.clone(),
                running,
                usage: trim_minutes(&st.usage),
                events: st.events.iter().cloned().collect(),
                last_activity: st.usage.last_ts,
            });
        }

        for s in sessions.iter_mut() {
            for a in agents.iter().filter(|a| a.session_id == s.session_id) {
                s.agents_total += 1;
                if a.running {
                    s.agents_running += 1;
                }
                s.agents_usage.add(&a.usage.tokens);
            }
            s.tasks_running = tasks
                .iter()
                .filter(|t| t.session_id == s.session_id && t.status == TaskStatus::Running)
                .count();
        }

        let last_activity = |s: &SessionInfo| s.usage.last_ts.or(s.started_at);
        sessions.sort_by(|a, b| b.alive.cmp(&a.alive).then_with(|| last_activity(b).cmp(&last_activity(a))));
        agents.sort_by(|a, b| b.running.cmp(&a.running).then_with(|| b.last_activity.cmp(&a.last_activity)));
        tasks.sort_by(|a, b| {
            let ra = a.status == TaskStatus::Running;
            let rb = b.status == TaskStatus::Running;
            rb.cmp(&ra).then_with(|| b.started.cmp(&a.started))
        });

        let now_min = Local::now().timestamp().div_euclid(60);
        self.global.per_minute.retain(|m, _| *m > now_min - 180);
        let per_minute = (0..60).map(|i| *self.global.per_minute.get(&(now_min - 59 + i)).unwrap_or(&0)).collect();

        let repos = self.repos(&sessions);
        let notices = std::mem::take(&mut *self.notices.lock().unwrap());
        Snapshot {
            repos,
            notices,
            sessions,
            agents,
            tasks,
            daily: self.global.daily.clone(),
            daily_by_model: self.global.daily_model.clone(),
            daily_sessions: self.global.daily_sessions.iter().map(|(d, s)| (*d, s.len())).collect(),
            per_minute,
            history_done,
            history_total,
            loaded: true,
        }
    }
}

pub fn run(tx: Sender<Snapshot>, rx: Receiver<Ctl>) {
    let mut c = match Collector::new() {
        Ok(c) => c,
        Err(_) => return,
    };
    c.start_bg_poller();
    loop {
        let snap = c.tick();
        if tx.send(snap).is_err() {
            break;
        }
        let wait = if c.history_pending() { Duration::from_millis(60) } else { Duration::from_millis(1000) };
        match rx.recv_timeout(wait) {
            Ok(Ctl::Quit) | Err(RecvTimeoutError::Disconnected) => break,
            Ok(Ctl::Act(a)) => c.act(a),
            Ok(Ctl::Refresh) => c.poke_bg(),
            Err(RecvTimeoutError::Timeout) => {}
        }
    }
}
