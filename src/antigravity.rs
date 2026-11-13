//! Sessões do Antigravity CLI (`agy`), lidas de `~/.gemini/antigravity-cli`.
//!
//! O que o `agy` deixa em disco e é usado aqui:
//! - `conversations/<id>.db`: uma conversa (SQLite em modo WAL). A data de
//!   modificação dele e do `-wal` diz quando houve atividade. Da tabela `steps`
//!   sai o consumo de tokens de cada resposta (ver [`turn_usage`]) e da
//!   `gen_metadata` o nome do modelo.
//! - `presence/<id>.lock`: fica travado enquanto um `agy` está com a conversa
//!   aberta. Quem trava é descoberto pela Restart Manager do Windows, que só
//!   consulta (não abre nem trava o arquivo).
//! - `cache/conversation_metadata.json`: título, nº de passos e pasta de
//!   trabalho das conversas (atualizado pelo `agy` com atraso; a conversa que
//!   está aberta agora costuma não estar lá ainda).
//! - `history.jsonl`: os prompts digitados, com `workspace`, `timestamp` e,
//!   às vezes, `conversationId`.
//!
//! O consumo fica em protobuf sem esquema publicado: os campos foram
//! identificados pelos números (ver [`turn_usage`]) e cada resposta é
//! conferida (saída = raciocínio + resposta). Se a conferência falhar, o
//! formato mudou e a conversa fica sem tokens em vez de mostrar números errados.

use crate::model::{Event, EventKind, Tokens};
use crate::sqlite;
use crate::scan::one_line;
use chrono::{DateTime, Local, TimeZone};
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

/// Nome mostrado no dashboard; o mesmo `nome` do agente em `agents.rs`.
pub const AGENT: &str = "Antigravity";
/// Conversas sem atividade há mais tempo que isto não aparecem (igual ao Claude).
const RECENT: Duration = Duration::from_secs(24 * 3600);
/// Gravou no `.db` há menos que isto: está trabalhando.
const BUSY_WINDOW: Duration = Duration::from_secs(8);
/// De quanto em quanto tempo perguntar quem trava cada conversa.
const OWNERS_EVERY: Duration = Duration::from_secs(5);
/// Teto de arquivos de presença consultados por vez (mais recentes primeiro).
const MAX_LOCKS: usize = 60;
/// Id das sessões que ainda não têm conversa: a conversa só nasce na
/// primeira mensagem, e até lá o `agy` aberto aparece pelo processo.
pub const NEW_PREFIX: &str = "agy-pid-";

#[derive(Debug, Clone)]
pub struct AgyConv {
    pub id: String,
    pub title: Option<String>,
    pub cwd: Option<String>,
    pub pid: Option<u32>,
    pub busy: bool,
    pub created: Option<DateTime<Local>>,
    pub updated: Option<DateTime<Local>>,
    pub prompts: Vec<Event>,
}

#[derive(Default, Clone)]
struct Meta {
    title: Option<String>,
    cwd: Option<String>,
}

#[derive(Clone)]
struct Prompt {
    ts: DateTime<Local>,
    text: String,
    workspace: Option<String>,
    conv: Option<String>,
}

pub struct Antigravity {
    dir: PathBuf,
    /// conversa -> processo `agy` que está com ela aberta.
    owners: HashMap<String, u32>,
    last_owners: Option<Instant>,
    /// PIDs de `agy` vistos na última consulta (mudou = consulta de novo já).
    agy_pids: Vec<u32>,
    /// PIDs de `agy` rodando agora (atualizado a cada tick).
    live: Vec<u32>,
    meta: (Option<SystemTime>, HashMap<String, Meta>),
    history: (Option<(SystemTime, u64)>, Vec<Prompt>),
    /// conversa -> respostas lidas do `.db` (refeito quando o arquivo muda).
    usage: HashMap<String, ConvUsage>,
    /// id interno do modelo -> nome (aprendido da `gen_metadata` das conversas).
    model_names: HashMap<u64, String>,
}

/// Data e tamanho do `.db` e do `-wal`: mudou, relê.
type Sig = (Option<SystemTime>, u64, Option<SystemTime>, u64);

struct ConvUsage {
    sig: Sig,
    /// `None`: o banco não pôde ser lido ou o formato não foi reconhecido.
    turns: Option<Vec<AgyTurn>>,
}

/// Consumo de uma resposta do modelo (um passo da conversa).
#[derive(Debug, Clone, PartialEq)]
pub struct AgyTurn {
    /// Posição do passo na conversa (estável: serve de id).
    pub idx: i64,
    pub ts: Option<DateTime<Local>>,
    pub model: String,
    pub tokens: Tokens,
}

fn to_local(t: SystemTime) -> Option<DateTime<Local>> {
    let d = t.duration_since(SystemTime::UNIX_EPOCH).ok()?;
    Local.timestamp_opt(d.as_secs() as i64, d.subsec_nanos()).single()
}

/// `file:///C:/Users/x` -> `C:\Users\x`.
fn uri_to_path(u: &str) -> String {
    let p = u.strip_prefix("file:///").unwrap_or(u);
    let p = p.replace("%20", " ");
    if cfg!(windows) { p.replace('/', "\\") } else { format!("/{p}") }
}

fn norm(p: &str) -> String {
    p.trim_end_matches(['\\', '/']).to_lowercase()
}

/// Pasta de trabalho do processo, sem a barra do fim (`C:\` fica como está).
fn proc_cwd(sys: &System, pid: u32) -> Option<String> {
    let s = sys.process(Pid::from_u32(pid))?.cwd()?.display().to_string();
    let s = if s.len() > 3 { s.trim_end_matches(['\\', '/']).to_string() } else { s };
    (!s.is_empty()).then_some(s)
}

/// Tamanho e data do `-wal` ao lado do banco (zero e `None` se não existe).
fn wal_of(db: &Path) -> (u64, Option<SystemTime>) {
    let mut p = db.as_os_str().to_owned();
    p.push("-wal");
    fs::metadata(&p).map_or((0, None), |m| (m.len(), m.modified().ok()))
}

/// Campo de primeiro nível de uma mensagem protobuf.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Pb<'a> {
    Int(u64),
    Bytes(&'a [u8]),
}

fn pb_varint(b: &[u8], i: &mut usize) -> Option<u64> {
    let mut v: u64 = 0;
    for shift in (0..64).step_by(7) {
        let x = *b.get(*i)?;
        *i += 1;
        v |= ((x & 0x7f) as u64) << shift;
        if x < 0x80 {
            return Some(v);
        }
    }
    None
}

/// Campos de primeiro nível (número, valor). `None` se não for protobuf válido.
fn pb_fields(b: &[u8]) -> Option<Vec<(u64, Pb<'_>)>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let key = pb_varint(b, &mut i)?;
        let (field, wire) = (key >> 3, key & 7);
        if field == 0 {
            return None;
        }
        let v = match wire {
            0 => Pb::Int(pb_varint(b, &mut i)?),
            1 => {
                i += 8;
                Pb::Bytes(b.get(i - 8..i)?)
            }
            2 => {
                let n = usize::try_from(pb_varint(b, &mut i)?).ok()?;
                i += n;
                Pb::Bytes(b.get(i - n..i)?)
            }
            5 => {
                i += 4;
                Pb::Bytes(b.get(i - 4..i)?)
            }
            _ => return None,
        };
        out.push((field, v));
    }
    Some(out)
}

fn pb_int(f: &[(u64, Pb<'_>)], n: u64) -> Option<u64> {
    f.iter().find_map(|(k, v)| match v {
        Pb::Int(x) if *k == n => Some(*x),
        _ => None,
    })
}

fn pb_bytes<'a>(f: &[(u64, Pb<'a>)], n: u64) -> Option<&'a [u8]> {
    f.iter().find_map(|(k, v)| match v {
        Pb::Bytes(x) if *k == n => Some(*x),
        _ => None,
    })
}

/// Consumo de um passo, a partir da coluna `metadata` da tabela `steps`.
///
/// Campos identificados comparando conversas reais do `agy` 1.1:
/// - 1: quando o passo começou (`Timestamp`: 1 = segundos, 2 = nanos);
/// - 9: uso do modelo nesta resposta: 1 = id do modelo, 2 = entrada,
///   3 = saída, 4 = escrita de cache, 5 = leitura de cache,
///   9 = saída de raciocínio, 10 = saída da resposta.
///
/// `Ok(None)`: passo sem resposta do modelo. `Err(())`: o formato não bate
/// (a saída não é raciocínio + resposta) e nada da conversa deve ser usado.
pub fn turn_usage(metadata: &[u8]) -> Result<Option<(Option<DateTime<Local>>, u64, Tokens)>, ()> {
    if metadata.is_empty() {
        return Ok(None);
    }
    let f = pb_fields(metadata).ok_or(())?;
    let Some(usage) = pb_bytes(&f, 9) else { return Ok(None) };
    let u = pb_fields(usage).ok_or(())?;
    let get = |n| pb_int(&u, n).unwrap_or(0);
    let tokens = Tokens { input: get(2), cache_write: get(4), cache_read: get(5), output: get(3) };
    if tokens.output != get(9) + get(10) || pb_int(&u, 1).is_none() {
        return Err(());
    }
    if tokens.total() == 0 {
        return Ok(None);
    }
    let ts = pb_bytes(&f, 1).and_then(pb_fields).and_then(|t| {
        let secs = i64::try_from(pb_int(&t, 1)?).ok()?;
        Local.timestamp_opt(secs, pb_int(&t, 2).unwrap_or(0) as u32).single()
    });
    Ok(Some((ts, get(1), tokens)))
}

/// Nome do modelo principal da conversa na `gen_metadata`: campo 3, com
/// 1 = id do modelo e 28 = nome (ex.: `gemini-3.8-flash-high`).
fn model_name(data: &[u8]) -> Option<(u64, String)> {
    let f = pb_fields(data)?;
    let cfg = pb_fields(pb_bytes(&f, 3)?)?;
    let id = pb_int(&cfg, 1)?;
    let name = std::str::from_utf8(pb_bytes(&cfg, 28)?).ok()?.trim();
    (!name.is_empty() && name.is_ascii()).then(|| (id, name.to_string()))
}

/// Respostas da conversa, com o nome do modelo resolvido. `None` se o banco
/// não pôde ser lido ou se algum passo não bate com o formato conhecido.
fn read_turns(db: &sqlite::Db, names: &mut HashMap<u64, String>) -> Option<Vec<AgyTurn>> {
    for (_, cols) in db.table("gen_metadata").unwrap_or_default() {
        if let Some((id, name)) = cols.get(1).and_then(|d| model_name(d.as_bytes())) {
            names.insert(id, name);
        }
    }
    let mut out = Vec::new();
    // Colunas de `steps`: idx (rowid), step_type, status, has_subtrajectory, metadata, …
    for (idx, cols) in db.table("steps")? {
        let metadata = cols.get(4).map_or(&[][..], |v| v.as_bytes());
        let Some((ts, model, tokens)) = turn_usage(metadata).ok()? else { continue };
        let model = names.get(&model).cloned().unwrap_or_else(|| format!("antigravity #{model}"));
        out.push(AgyTurn { idx, ts, model, tokens });
    }
    Some(out)
}

impl Antigravity {
    pub fn new(home: &Path) -> Self {
        Self::with_dir(home.join(".gemini").join("antigravity-cli"))
    }

    pub fn with_dir(dir: PathBuf) -> Self {
        Self {
            dir,
            owners: HashMap::new(),
            last_owners: None,
            agy_pids: Vec::new(),
            live: Vec::new(),
            meta: (None, HashMap::new()),
            history: (None, Vec::new()),
            usage: HashMap::new(),
            model_names: HashMap::new(),
        }
    }

    /// Processo `agy` que está com a conversa `id` aberta agora. Vale também
    /// para as sessões ainda sem conversa (`agy-pid-<pid>`).
    pub fn owner(&self, id: &str) -> Option<u32> {
        if let Some(pid) = id.strip_prefix(NEW_PREFIX).and_then(|p| p.parse::<u32>().ok()) {
            return self.live.contains(&pid).then_some(pid);
        }
        self.owners.get(id).copied()
    }

    fn load_meta(&mut self) {
        let p = self.dir.join("cache").join("conversation_metadata.json");
        let mtime = fs::metadata(&p).and_then(|m| m.modified()).ok();
        if mtime.is_some() && mtime == self.meta.0 {
            return;
        }
        let mut out = HashMap::new();
        if let Some(v) = fs::read_to_string(&p).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok())
            && let Some(convs) = v.get("conversations").and_then(|c| c.as_object())
        {
            for (id, c) in convs {
                let s = &c["summary"];
                let text = |k: &str| s.get(k).and_then(|x| x.as_str()).map(|x| one_line(x, 120)).filter(|x| !x.is_empty());
                out.insert(
                    id.clone(),
                    Meta {
                        title: text("Title").or_else(|| text("Preview")),
                        cwd: s.get("WorkspaceURIs").and_then(|w| w.get(0)).and_then(|w| w.as_str()).map(uri_to_path),
                    },
                );
            }
        }
        self.meta = (mtime, out);
    }

    fn load_history(&mut self) {
        let p = self.dir.join("history.jsonl");
        let stamp = fs::metadata(&p).ok().and_then(|m| Some((m.modified().ok()?, m.len())));
        if stamp.is_some() && stamp == self.history.0 {
            return;
        }
        let mut out = Vec::new();
        for line in fs::read_to_string(&p).unwrap_or_default().lines() {
            let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
            let s = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string);
            let Some(ts) = v.get("timestamp").and_then(|t| t.as_i64()).and_then(|ms| Local.timestamp_millis_opt(ms).single()) else { continue };
            let Some(text) = s("display").map(|d| one_line(&d, 300)).filter(|d| !d.is_empty()) else { continue };
            out.push(Prompt { ts, text, workspace: s("workspace"), conv: s("conversationId") });
        }
        self.history = (stamp, out);
    }

    /// Atualiza quem está com cada conversa aberta. Barato quando nenhum `agy`
    /// está rodando; senão consulta a Restart Manager a cada `OWNERS_EVERY`.
    fn refresh_owners(&mut self, sys: &System) {
        let is_agy = |p: &sysinfo::Process| {
            let n = p.name().to_string_lossy();
            n.eq_ignore_ascii_case("agy.exe") || n == "agy"
        };
        // Só o `agy` de cima: um filho com o mesmo nome não é outra sessão.
        let mut agy: Vec<u32> = sys
            .processes()
            .iter()
            .filter(|(_, p)| is_agy(p) && !p.parent().and_then(|pp| sys.process(pp)).is_some_and(is_agy))
            .map(|(pid, _)| pid.as_u32())
            .collect();
        agy.sort_unstable();
        self.live = agy.clone();
        if agy.is_empty() {
            self.owners.clear();
            self.agy_pids.clear();
            return;
        }
        // Dono que morreu sai na hora, sem esperar a próxima consulta.
        self.owners.retain(|_, pid| agy.contains(pid));
        let due = self.last_owners.is_none_or(|t| t.elapsed() >= OWNERS_EVERY) || agy != self.agy_pids;
        if !due {
            return;
        }
        self.last_owners = Some(Instant::now());
        self.agy_pids = agy.clone();
        let mut locks: Vec<(SystemTime, String, PathBuf)> = fs::read_dir(self.dir.join("presence"))
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let p = e.path();
                (p.extension().and_then(|x| x.to_str()) == Some("lock")).then_some(())?;
                let id = p.file_stem()?.to_string_lossy().to_string();
                Some((e.metadata().ok()?.modified().ok()?, id, p))
            })
            .collect();
        locks.sort_by(|a, b| b.0.cmp(&a.0));
        let mut owners = HashMap::new();
        for (_, id, p) in locks.into_iter().take(MAX_LOCKS) {
            if let Some(pid) = lock_holders(&p).into_iter().find(|pid| agy.contains(pid)) {
                owners.insert(id, pid);
            }
            // Todos os `agy` já têm conversa: não precisa olhar as outras.
            if owners.len() >= agy.len() {
                break;
            }
        }
        self.owners = owners;
    }

    /// Conversas vivas e as com atividade nas últimas 24h.
    pub fn scan(&mut self, sys: &mut System) -> Vec<AgyConv> {
        if !self.dir.is_dir() {
            return Vec::new();
        }
        self.refresh_owners(sys);
        self.load_meta();
        self.load_history();
        // Pasta de trabalho dos processos vivos (lida só deles, sob demanda).
        let pids: Vec<Pid> = self.live.iter().map(|p| Pid::from_u32(*p)).collect();
        if !pids.is_empty() {
            sys.refresh_processes_specifics(ProcessesToUpdate::Some(&pids), false, ProcessRefreshKind::nothing().with_cwd(UpdateKind::OnlyIfNotSet));
        }
        let now = SystemTime::now();
        let mut out = Vec::new();
        for e in fs::read_dir(self.dir.join("conversations")).into_iter().flatten().flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("db") {
                continue;
            }
            let Some(id) = p.file_stem().map(|s| s.to_string_lossy().to_string()) else { continue };
            let Ok(md) = e.metadata() else { continue };
            // Em modo WAL a escrita vai primeiro para o `-wal`: vale o mais novo dos dois.
            let modified = md.modified().unwrap_or(SystemTime::UNIX_EPOCH).max(wal_of(&p).1.unwrap_or(SystemTime::UNIX_EPOCH));
            let pid = self.owner(&id);
            let recent = now.duration_since(modified).map_or(true, |d| d < RECENT);
            if pid.is_none() && !recent {
                continue;
            }
            let meta = self.meta.1.get(&id).cloned().unwrap_or_default();
            let cwd = pid.and_then(|p| proc_cwd(sys, p)).or(meta.cwd.clone()).or_else(|| self.workspace_of(&id));
            let created = md.created().ok().and_then(to_local);
            let updated = to_local(modified);
            let prompts = self.prompts_of(&id, cwd.as_deref(), created, updated);
            let title = meta.title.clone().or_else(|| prompts.first().map(|e| one_line(&e.text, 80)));
            out.push(AgyConv {
                busy: pid.is_some() && now.duration_since(modified).is_ok_and(|d| d < BUSY_WINDOW),
                id,
                title,
                cwd,
                pid,
                created,
                updated,
                prompts,
            });
        }
        // `agy` aberto que ainda não tem conversa (nenhuma mensagem mandada).
        let owned: Vec<u32> = self.owners.values().copied().collect();
        for pid in self.live.iter().filter(|p| !owned.contains(p)) {
            let started = sys.process(Pid::from_u32(*pid)).and_then(|p| Local.timestamp_opt(p.start_time() as i64, 0).single());
            out.push(AgyConv {
                id: format!("{NEW_PREFIX}{pid}"),
                title: None,
                cwd: proc_cwd(sys, *pid),
                pid: Some(*pid),
                busy: false,
                created: started,
                updated: started,
                prompts: Vec::new(),
            });
        }
        out
    }

    /// Relê o consumo das conversas cujo `.db` mudou desde a última chamada
    /// (todas, não só as recentes: o histórico de uso por dia precisa delas).
    /// Devolve as conversas relidas com as respostas atuais de cada uma.
    pub fn refresh_usage(&mut self) -> Vec<(String, Vec<AgyTurn>)> {
        let mut changed = Vec::new();
        let mut present = Vec::new();
        for e in fs::read_dir(self.dir.join("conversations")).into_iter().flatten().flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("db") {
                continue;
            }
            let Some(id) = p.file_stem().map(|s| s.to_string_lossy().to_string()) else { continue };
            let Ok(md) = e.metadata() else { continue };
            let (wal_len, wal_time) = wal_of(&p);
            let sig: Sig = (md.modified().ok(), md.len(), wal_time, wal_len);
            present.push(id.clone());
            if self.usage.get(&id).is_some_and(|u| u.sig == sig) {
                continue;
            }
            let turns = sqlite::Db::open(&p).and_then(|db| read_turns(&db, &mut self.model_names));
            // Leitura no meio de uma escrita pode falhar: mantém o que já tinha
            // e tenta de novo na próxima mudança do arquivo.
            if turns.is_none()
                && let Some(u) = self.usage.get_mut(&id)
                && u.turns.is_some()
            {
                u.sig = sig;
                continue;
            }
            if let Some(t) = &turns {
                changed.push((id.clone(), t.clone()));
            }
            self.usage.insert(id, ConvUsage { sig, turns });
        }
        self.usage.retain(|id, _| present.contains(id));
        changed
    }

    /// Respostas já lidas da conversa `id` (`None`: sem dados de consumo).
    pub fn turns(&self, id: &str) -> Option<&[AgyTurn]> {
        self.usage.get(id)?.turns.as_deref()
    }

    /// Pasta da conversa pelo `last_conversations.json` (pasta -> última conversa).
    fn workspace_of(&self, id: &str) -> Option<String> {
        let txt = fs::read_to_string(self.dir.join("cache").join("last_conversations.json")).ok()?;
        let v: Value = serde_json::from_str(&txt).ok()?;
        v.as_object()?.iter().find(|(_, c)| c.as_str() == Some(id)).map(|(ws, _)| ws.clone())
    }

    /// Prompts da conversa: os marcados com o id dela; os sem id contam se
    /// foram na mesma pasta e dentro do período em que ela existiu.
    fn prompts_of(&self, id: &str, cwd: Option<&str>, created: Option<DateTime<Local>>, updated: Option<DateTime<Local>>) -> Vec<Event> {
        let slack = chrono::Duration::seconds(30);
        self.history
            .1
            .iter()
            .filter(|p| match &p.conv {
                Some(c) => c == id,
                None => {
                    let same_dir = matches!((cwd, &p.workspace), (Some(a), Some(b)) if norm(a) == norm(b));
                    let in_time = created.is_some_and(|c| p.ts >= c - slack) && updated.is_none_or(|u| p.ts <= u + slack);
                    same_dir && in_time
                }
            })
            .map(|p| Event { ts: Some(p.ts), kind: EventKind::Prompt, text: p.text.clone() })
            .collect()
    }
}

/// Processos que estão com o arquivo `path` aberto, pela Restart Manager do
/// Windows. Só consulta. A DLL é carregada em tempo de execução (o toolchain
/// GNU não traz a biblioteca de importação dela).
#[cfg(windows)]
pub fn lock_holders(path: &Path) -> Vec<u32> {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct UniqueProcess {
        pid: u32,
        start_low: u32,
        start_high: u32,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct ProcessInfo {
        process: UniqueProcess,
        app_name: [u16; 256],
        service: [u16; 64],
        app_type: u32,
        status: u32,
        ts_session: u32,
        restartable: i32,
    }
    type Start = unsafe extern "system" fn(*mut u32, u32, *mut u16) -> u32;
    type End = unsafe extern "system" fn(u32) -> u32;
    type Register = unsafe extern "system" fn(u32, u32, *const *const u16, u32, *const c_void, u32, *const *const u16) -> u32;
    type GetList = unsafe extern "system" fn(u32, *mut u32, *mut u32, *mut ProcessInfo, *mut u32) -> u32;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn LoadLibraryW(name: *const u16) -> *mut c_void;
        fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
    }

    let wide = |s: &str| s.encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>();
    unsafe {
        let dll = LoadLibraryW(wide("rstrtmgr.dll").as_ptr());
        if dll.is_null() {
            return Vec::new();
        }
        let f = |n: &[u8]| GetProcAddress(dll, n.as_ptr());
        let (s, e, r, g) = (f(b"RmStartSession\0"), f(b"RmEndSession\0"), f(b"RmRegisterResources\0"), f(b"RmGetList\0"));
        if s.is_null() || e.is_null() || r.is_null() || g.is_null() {
            return Vec::new();
        }
        let (start, end, register, get_list): (Start, End, Register, GetList) =
            (std::mem::transmute(s), std::mem::transmute(e), std::mem::transmute(r), std::mem::transmute(g));

        let mut handle = 0u32;
        let mut key = [0u16; 33]; // CCH_RM_SESSION_KEY + 1
        if start(&mut handle, 0, key.as_mut_ptr()) != 0 {
            return Vec::new();
        }
        let file: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        let files = [file.as_ptr()];
        let mut out = Vec::new();
        if register(handle, 1, files.as_ptr(), 0, std::ptr::null(), 0, std::ptr::null()) == 0 {
            let empty = ProcessInfo {
                process: UniqueProcess { pid: 0, start_low: 0, start_high: 0 },
                app_name: [0; 256],
                service: [0; 64],
                app_type: 0,
                status: 0,
                ts_session: 0,
                restartable: 0,
            };
            let mut buf = vec![empty; 8];
            let (mut needed, mut n, mut reasons) = (0u32, buf.len() as u32, 0u32);
            // ERROR_MORE_DATA (234): mais de 8 processos; os 8 primeiros bastam.
            let rc = get_list(handle, &mut needed, &mut n, buf.as_mut_ptr(), &mut reasons);
            if rc == 0 || rc == 234 {
                out.extend(buf.iter().take(n as usize).map(|p| p.process.pid));
            }
        }
        end(handle);
        out
    }
}

#[cfg(not(windows))]
pub fn lock_holders(_path: &Path) -> Vec<u32> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caminho_do_uri() {
        if cfg!(windows) {
            assert_eq!(uri_to_path("file:///C:/Users/fulano/Meus%20Docs"), "C:\\Users\\fulano\\Meus Docs");
        }
    }

    /// Pasta falsa com o mesmo formato do `agy`: lê título, pasta, passos,
    /// prompts; sem processo `agy`, nada está vivo.
    #[test]
    fn le_conversas_de_uma_pasta_falsa() {
        let dir = std::env::temp_dir().join(format!("agent-dash-agy-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        for d in ["conversations", "presence", "cache"] {
            fs::create_dir_all(dir.join(d)).unwrap();
        }
        let (a, b) = ("aaaaaaaa-0000-0000-0000-000000000001", "bbbbbbbb-0000-0000-0000-000000000002");
        fs::write(dir.join("conversations").join(format!("{a}.db")), b"x").unwrap();
        fs::write(dir.join("conversations").join(format!("{b}.db")), b"x").unwrap();
        fs::write(dir.join("presence").join(format!("{a}.lock")), b"").unwrap();
        fs::write(
            dir.join("cache").join("conversation_metadata.json"),
            format!(r#"{{"conversations":{{"{a}":{{"summary":{{"Title":"","Preview":"Relatório mensal","NumSteps":7,"WorkspaceURIs":["file:///C:/proj"]}}}}}}}}"#),
        )
        .unwrap();
        fs::write(dir.join("cache").join("last_conversations.json"), format!(r#"{{"C:\\outro":"{b}"}}"#)).unwrap();
        let now_ms = Local::now().timestamp_millis();
        fs::write(
            dir.join("history.jsonl"),
            format!(
                "{{\"display\":\"faz o relatório\",\"timestamp\":{now_ms},\"workspace\":\"C:\\\\proj\",\"conversationId\":\"{a}\"}}\n\
                 {{\"display\":\"teste sem id\",\"timestamp\":{now_ms},\"workspace\":\"C:\\\\outro\"}}\n"
            ),
        )
        .unwrap();

        let mut sys = System::new();
        let mut agy = Antigravity::with_dir(dir.clone());
        let mut v = agy.scan(&mut sys);
        v.sort_by(|x, y| x.id.cmp(&y.id));
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].title.as_deref(), Some("Relatório mensal"));
        assert_eq!(v[0].prompts.len(), 1);
        assert_eq!(v[0].pid, None);
        assert!(!v[0].busy);
        if cfg!(windows) {
            assert_eq!(v[0].cwd.as_deref(), Some("C:\\proj"));
            // Sem metadados: pasta pelo last_conversations, título pelo prompt sem id.
            assert_eq!(v[1].cwd.as_deref(), Some("C:\\outro"));
            assert_eq!(v[1].title.as_deref(), Some("teste sem id"));
        }
        let _ = fs::remove_dir_all(&dir);
    }

    /// Restart Manager: um arquivo aberto por este processo tem este processo
    /// como dono; um arquivo fechado não tem dono.
    #[cfg(windows)]
    #[test]
    fn restart_manager_acha_quem_abriu() {
        let p = std::env::temp_dir().join(format!("agent-dash-rm-{}.lock", std::process::id()));
        fs::write(&p, b"").unwrap();
        assert!(!lock_holders(&p).contains(&std::process::id()));
        let f = fs::File::open(&p).unwrap();
        assert!(lock_holders(&p).contains(&std::process::id()));
        drop(f);
        let _ = fs::remove_file(&p);
    }

    /// Manual, contra o `~/.gemini` de verdade: `cargo test agy_real -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn agy_real() {
        let mut sys = System::new();
        sys.refresh_processes(ProcessesToUpdate::All, true);
        let mut agy = Antigravity::new(&dirs::home_dir().unwrap());
        for c in agy.scan(&mut sys) {
            println!(
                "{} pid={:?} busy={} cwd={:?} título={:?} prompts={} atualizado={:?}",
                &c.id[..8],
                c.pid,
                c.busy,
                c.cwd,
                c.title,
                c.prompts.len(),
                c.updated.map(|u| u.format("%d/%m %H:%M:%S").to_string())
            );
        }
        for (id, turns) in agy.refresh_usage() {
            let mut tot = Tokens::default();
            turns.iter().for_each(|t| tot.add(&t.tokens));
            println!("{} respostas={} {tot:?}", &id[..8], turns.len());
        }
    }

    fn fixture() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata").join("agy").join("conv.db")
    }

    fn soma(turns: &[AgyTurn]) -> Tokens {
        let mut t = Tokens::default();
        turns.iter().for_each(|x| t.add(&x.tokens));
        t
    }

    /// Conversa gerada por `testdata/agy/gerar.py`: 45 passos, 15 respostas,
    /// as 15 últimas linhas só no `-wal`.
    #[test]
    fn le_tokens_de_uma_conversa_com_wal() {
        let db = sqlite::Db::open(&fixture()).expect("abre a fixture");
        let mut names = HashMap::new();
        let turns = read_turns(&db, &mut names).expect("formato reconhecido");
        assert_eq!(turns.len(), 15);
        assert_eq!(soma(&turns), Tokens { input: 15330, cache_write: 0, cache_read: 1232, output: 2310 });
        assert!(turns.windows(2).all(|w| w[0].idx < w[1].idx), "ordem do rowid");
        assert!(turns.iter().any(|t| t.idx >= 30), "linhas que só existem no -wal");
        assert_eq!(names.get(&1318).map(String::as_str), Some("gemini-teste"));
        assert!(turns.iter().any(|t| t.model == "gemini-teste"));
        assert!(turns.iter().any(|t| t.model == "antigravity #1050"), "modelo sem nome");
        let first = &turns[0];
        assert_eq!((first.idx, first.tokens.input, first.tokens.output), (1, 1001, 101 + 11));
        assert_eq!(first.ts.map(|t| t.timestamp()), Some(1794510000 + 60));

        // Sem o -wal só aparecem as linhas já gravadas no arquivo principal.
        let main = std::fs::read(fixture()).unwrap();
        let db = sqlite::Db::from_bytes(main, Vec::new()).unwrap();
        let only_main = read_turns(&db, &mut HashMap::new()).unwrap();
        assert_eq!(only_main.len(), 10);
        assert!(only_main.iter().all(|t| t.idx < 30));
    }

    #[test]
    fn formato_desconhecido_nao_vira_numero() {
        fn int(n: u64, v: u64) -> Vec<u8> {
            let mut out = Vec::new();
            for mut x in [n << 3, v] {
                loop {
                    let b = (x & 0x7f) as u8;
                    x >>= 7;
                    out.push(if x > 0 { b | 0x80 } else { b });
                    if x == 0 {
                        break;
                    }
                }
            }
            out
        }
        let with_usage = |u: &[(u64, u64)]| {
            let body: Vec<u8> = u.iter().flat_map(|(k, v)| int(*k, *v)).collect();
            let mut m = vec![(9 << 3) | 2, body.len() as u8];
            m.extend(body);
            m
        };
        // saída = raciocínio + resposta: aceito.
        let ok = turn_usage(&with_usage(&[(1, 7), (2, 50), (3, 30), (9, 20), (10, 10)])).unwrap().unwrap();
        assert_eq!((ok.1, ok.2.input, ok.2.output), (7, 50, 30));
        // Não fecha a conta: o formato mudou, nada é usado.
        assert!(turn_usage(&with_usage(&[(1, 7), (2, 50), (3, 31), (9, 20), (10, 10)])).is_err());
        // Sem o id do modelo também não é o formato conhecido.
        assert!(turn_usage(&with_usage(&[(2, 50), (3, 30), (9, 20), (10, 10)])).is_err());
        // Passo sem uso (campo 9 ausente) e lixo.
        assert_eq!(turn_usage(&int(3, 4)), Ok(None));
        assert_eq!(turn_usage(&[]), Ok(None));
        assert!(turn_usage(&[0xff, 0xff]).is_err());
    }

    #[test]
    fn rele_so_quando_o_banco_muda() {
        let dir = std::env::temp_dir().join(format!("agent-dash-agy-uso-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("conversations")).unwrap();
        let id = "cccccccc-0000-0000-0000-000000000003";
        fs::copy(fixture(), dir.join("conversations").join(format!("{id}.db"))).unwrap();
        let mut agy = Antigravity::with_dir(dir.clone());
        let first = agy.refresh_usage();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].0, id);
        assert_eq!(agy.turns(id).map(<[AgyTurn]>::len), Some(10));
        assert!(agy.refresh_usage().is_empty(), "nada mudou");
        // O -wal aparece (a conversa continuou): relê e passa a ver as 15.
        let mut wal = fixture().into_os_string();
        wal.push("-wal");
        fs::copy(&wal, dir.join("conversations").join(format!("{id}.db-wal"))).unwrap();
        let again = agy.refresh_usage();
        assert_eq!(again.len(), 1);
        assert_eq!(agy.turns(id).map(<[AgyTurn]>::len), Some(15));
        // Conversa apagada sai do cache.
        fs::remove_dir_all(dir.join("conversations")).unwrap();
        fs::create_dir_all(dir.join("conversations")).unwrap();
        agy.refresh_usage();
        assert!(agy.turns(id).is_none());
        let _ = fs::remove_dir_all(&dir);
    }
}
