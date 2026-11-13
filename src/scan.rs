//! Leitura incremental dos transcripts `.jsonl` do Claude Code.
//!
//! Cada arquivo guarda um offset em bytes; a cada tick só as linhas novas e
//! completas são lidas. Uma linha sem `\n` no final (arquivo sendo escrito)
//! fica para o próximo tick.
//!
//! Detalhes do formato que importam aqui:
//! - Uma resposta da API vira várias linhas `assistant` (uma por bloco de
//!   conteúdo) com o mesmo `message.id` e o mesmo `usage`. Por isso o uso é
//!   deduplicado por id (e substituído, caso o valor mude durante o streaming).
//! - `api-request-blob` repete conteúdo de mensagens e é ignorado.
//! - Tasks em background começam num `toolUseResult` com `backgroundTaskId`
//!   (Bash), `taskId` (Monitor/Workflow) ou `isAsync`+`agentId` (Agent), e
//!   terminam numa `<task-notification>` com `<task-id>` e `<status>`.

use crate::model::*;
use chrono::{DateTime, Local};
use regex::Regex;
use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::OnceLock;

pub const MAX_EVENTS: usize = 400;

/// Recebe cada resposta (id único) para totais globais, deduplicados entre arquivos.
pub trait UsageSink {
    fn report(&mut self, session_id: &str, msg_id: &str, ts: Option<DateTime<Local>>, model: &str, t: Tokens);
}

#[derive(Debug, Clone)]
pub struct TaskRec {
    pub kind: TaskKind,
    pub description: String,
    pub command: Option<String>,
    pub started: Option<DateTime<Local>>,
    pub status: Option<String>,
    pub ended: Option<DateTime<Local>>,
    pub output_file: Option<PathBuf>,
    /// Monitors expiram sozinhos (`timeoutMs`), às vezes sem notificação no transcript.
    pub expires: Option<DateTime<Local>>,
    /// Comando sem cortes (para achar o processo da task).
    pub raw_command: Option<String>,
    /// Quando a ferramenta foi chamada (o processo nasce entre isto e `started`).
    pub requested: Option<DateTime<Local>>,
}

impl TaskRec {
    /// Status efetivo: o notificado, ou "expirado" se o prazo do monitor já passou.
    pub fn effective_status(&self, now: DateTime<Local>) -> Option<String> {
        if let Some(s) = &self.status {
            return Some(s.clone());
        }
        match self.expires {
            Some(e) if e < now => Some("expirado".into()),
            _ => None,
        }
    }
}

#[derive(Clone, Default)]
struct ToolUse {
    name: String,
    desc: String,
    cmd: Option<String>,
    raw_cmd: Option<String>,
    ts: Option<DateTime<Local>>,
}

pub struct FileState {
    pub path: PathBuf,
    pub session_id: String,
    /// Modo completo (eventos, tasks, ferramentas) ou leve (só tokens).
    pub full: bool,
    pub offset: u64,
    msg_tokens: HashMap<String, (String, Tokens)>,
    pub usage: Usage,
    pub events: VecDeque<Event>,
    tool_uses: HashMap<String, ToolUse>,
    pub tasks: BTreeMap<String, TaskRec>,
    /// agentId -> status final informado pelo pai (agentes em primeiro plano).
    pub agent_done: HashMap<String, String>,
    pub title: Option<String>,
    pub last_prompt: Option<String>,
    pub cwd: Option<String>,
}

#[derive(Deserialize)]
struct RawUsage {
    input_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
    output_tokens: Option<u64>,
}

impl RawUsage {
    fn tokens(&self) -> Tokens {
        Tokens {
            input: self.input_tokens.unwrap_or(0),
            cache_write: self.cache_creation_input_tokens.unwrap_or(0),
            cache_read: self.cache_read_input_tokens.unwrap_or(0),
            output: self.output_tokens.unwrap_or(0),
        }
    }
}

#[derive(Deserialize)]
struct LightMsg {
    id: Option<String>,
    model: Option<String>,
    usage: Option<RawUsage>,
}

#[derive(Deserialize)]
struct LightRec {
    #[serde(rename = "type")]
    t: Option<String>,
    timestamp: Option<String>,
    message: Option<LightMsg>,
}

pub fn parse_ts(s: &str) -> Option<DateTime<Local>> {
    DateTime::parse_from_rfc3339(s).ok().map(|d| d.with_timezone(&Local))
}

pub fn one_line(s: &str, max: usize) -> String {
    let mut out = String::with_capacity(max.min(s.len()));
    for (n, ch) in s.chars().map(|c| if c.is_control() { ' ' } else { c }).enumerate() {
        if n >= max {
            out.push('…');
            break;
        }
        out.push(ch);
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn notif_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?s)<task-notification>(.*?)</task-notification>").unwrap())
}

fn tag(body: &str, name: &str) -> Option<String> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let start = body.find(&open)? + open.len();
    let end = body[start..].find(&close)? + start;
    Some(body[start..end].trim().to_string())
}

fn all_tags(body: &str, name: &str) -> Vec<String> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let mut out = Vec::new();
    let mut rest = body;
    while let Some(i) = rest.find(&open) {
        let after = &rest[i + open.len()..];
        let Some(j) = after.find(&close) else { break };
        out.push(after[..j].trim().to_string());
        rest = &after[j + close.len()..];
    }
    out
}

fn tool_summary(input: &Value) -> String {
    for k in ["description", "command", "file_path", "pattern", "url", "query", "skill", "task_id", "prompt"] {
        if let Some(s) = input.get(k).and_then(|v| v.as_str()) {
            return one_line(s, 140);
        }
    }
    String::new()
}

impl FileState {
    pub fn new(path: PathBuf, session_id: String, full: bool) -> Self {
        Self {
            path,
            session_id,
            full,
            offset: 0,
            msg_tokens: HashMap::new(),
            usage: Usage::default(),
            events: VecDeque::new(),
            tool_uses: HashMap::new(),
            tasks: BTreeMap::new(),
            agent_done: HashMap::new(),
            title: None,
            last_prompt: None,
            cwd: None,
        }
    }

    fn reset(&mut self) {
        let fresh = FileState::new(self.path.clone(), self.session_id.clone(), self.full);
        *self = fresh;
    }

    /// Lê as linhas novas. Devolve quantos bytes foram consumidos.
    pub fn ingest(&mut self, sink: &mut dyn UsageSink) -> std::io::Result<u64> {
        let f = File::open(&self.path)?;
        let len = f.metadata()?.len();
        if len < self.offset {
            self.reset();
        }
        if len == self.offset {
            return Ok(0);
        }
        let start = self.offset;
        let mut r = BufReader::with_capacity(1 << 17, f);
        r.seek(SeekFrom::Start(self.offset))?;
        let mut buf = Vec::with_capacity(16 * 1024);
        loop {
            buf.clear();
            let n = r.read_until(b'\n', &mut buf)?;
            if n == 0 || buf.last() != Some(&b'\n') {
                break; // fim, ou linha incompleta ainda sendo escrita
            }
            self.offset += n as u64;
            if let Ok(s) = std::str::from_utf8(&buf) {
                let s = s.trim_end();
                if !s.is_empty() {
                    self.handle_line(s, sink);
                }
            }
        }
        Ok(self.offset - start)
    }

    fn handle_line(&mut self, line: &str, sink: &mut dyn UsageSink) {
        if !self.full {
            if line.contains("\"usage\"") && line.contains("\"type\":\"assistant\"")
                && let Ok(rec) = serde_json::from_str::<LightRec>(line)
                    && rec.t.as_deref() == Some("assistant")
                        && let Some(m) = rec.message {
                            let ts = rec.timestamp.as_deref().and_then(parse_ts);
                            if let (Some(id), Some(u)) = (m.id, m.usage) {
                                let model = m.model.unwrap_or_else(|| "?".into());
                                self.record_usage(&id, ts, &model, u.tokens(), sink);
                            }
                        }
            return;
        }

        // Registros grandes e irrelevantes: pula sem fazer parse.
        if line.contains("\"type\":\"api-request") || line.contains("\"type\":\"file-history-snapshot\"") {
            return;
        }
        if line.len() > 64 * 1024
            && !line.contains("\"type\":\"assistant\"")
            && !line.contains("backgroundTaskId")
            && !line.contains("\"taskId\"")
            && !line.contains("\"isAsync\"")
            && !line.contains("task-notification")
            && !line.contains("\"agentId\"")
        {
            return;
        }
        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => return,
        };
        let ts = v.get("timestamp").and_then(|t| t.as_str()).and_then(parse_ts);
        if self.cwd.is_none()
            && let Some(c) = v.get("cwd").and_then(|c| c.as_str()) {
                self.cwd = Some(c.to_string());
            }
        match v.get("type").and_then(|t| t.as_str()) {
            Some("assistant") => self.on_assistant(&v, ts, sink),
            Some("user") => self.on_user(&v, ts),
            Some("attachment") => {
                let att = &v["attachment"];
                if let Some(p) = att.get("prompt").and_then(|p| p.as_str())
                    && p.contains("<task-notification>") {
                        self.on_notifications(p, ts);
                    }
            }
            Some("ai-title") => {
                if let Some(t) = v.get("aiTitle").and_then(|t| t.as_str()) {
                    self.title = Some(t.to_string());
                }
            }
            Some("last-prompt") => {
                if let Some(t) = v.get("lastPrompt").and_then(|t| t.as_str()) {
                    self.last_prompt = Some(one_line(t, 300));
                }
            }
            _ => {}
        }
    }

    fn record_usage(&mut self, id: &str, ts: Option<DateTime<Local>>, model: &str, t: Tokens, sink: &mut dyn UsageSink) {
        if model == "<synthetic>" {
            return;
        }
        let prev = self.msg_tokens.get(id).cloned();
        if matches!(&prev, Some((_, old)) if *old == t) {
            return;
        }
        let u = &mut self.usage;
        match &prev {
            Some((old_model, old)) => {
                // Mesma resposta reescrita com valores finais: troca o antigo pelo novo.
                u.tokens.sub(old);
                if let Some(e) = u.by_model.get_mut(old_model) {
                    e.sub(old);
                }
                if let Some(ts) = ts {
                    let m = u.per_minute.entry(ts.timestamp().div_euclid(60)).or_default();
                    *m = m.saturating_sub(old.total());
                }
            }
            None => u.turns += 1,
        }
        u.by_model.entry(model.to_string()).or_default().add(&t);
        u.tokens.add(&t);
        if let Some(ts) = ts {
            *u.per_minute.entry(ts.timestamp().div_euclid(60)).or_default() += t.total();
            if u.first_ts.is_none_or(|f| ts < f) {
                u.first_ts = Some(ts);
            }
            if u.last_ts.is_none_or(|l| ts >= l) {
                u.last_ts = Some(ts);
                u.last_turn = t;
                u.model = Some(model.to_string());
            }
        } else {
            u.last_turn = t;
            u.model = Some(model.to_string());
        }
        self.msg_tokens.insert(id.to_string(), (model.to_string(), t));
        sink.report(&self.session_id, id, ts, model, t);
    }

    fn push_event(&mut self, ts: Option<DateTime<Local>>, kind: EventKind, text: String) {
        if text.is_empty() {
            return;
        }
        self.events.push_back(Event { ts, kind, text });
        while self.events.len() > MAX_EVENTS {
            self.events.pop_front();
        }
    }

    fn on_assistant(&mut self, v: &Value, ts: Option<DateTime<Local>>, sink: &mut dyn UsageSink) {
        let msg = &v["message"];
        let model = msg.get("model").and_then(|m| m.as_str()).unwrap_or("?").to_string();
        let id = msg
            .get("id")
            .and_then(|i| i.as_str())
            .or_else(|| v.get("requestId").and_then(|i| i.as_str()))
            .map(str::to_string);
        if let (Some(id), Some(u)) = (id, msg.get("usage"))
            && let Ok(raw) = RawUsage::deserialize(u) {
                self.record_usage(&id, ts, &model, raw.tokens(), sink);
            }
        let Some(content) = msg.get("content").and_then(|c| c.as_array()) else { return };
        for c in content {
            match c.get("type").and_then(|t| t.as_str()) {
                Some("tool_use") => {
                    let name = c.get("name").and_then(|n| n.as_str()).unwrap_or("?").to_string();
                    let input = c.get("input").cloned().unwrap_or(Value::Null);
                    let summary = tool_summary(&input);
                    let raw = input.get("command").and_then(|s| s.as_str());
                    let cmd = raw.map(|s| one_line(s, 400));
                    let raw_cmd = raw.map(|s| s.chars().take(4000).collect::<String>());
                    let desc = input
                        .get("description")
                        .and_then(|s| s.as_str())
                        .map(|s| one_line(s, 140))
                        .unwrap_or_else(|| summary.clone());
                    if let Some(tid) = c.get("id").and_then(|i| i.as_str()) {
                        self.tool_uses.insert(tid.to_string(), ToolUse { name: name.clone(), desc, cmd, raw_cmd, ts });
                    }
                    *self.usage.tool_counts.entry(name.clone()).or_default() += 1;
                    self.push_event(ts, EventKind::Tool, format!("{name}  {summary}"));
                }
                Some("text") => {
                    if let Some(t) = c.get("text").and_then(|t| t.as_str()) {
                        self.push_event(ts, EventKind::Text, one_line(t, 220));
                    }
                }
                _ => {}
            }
        }
    }

    fn on_user(&mut self, v: &Value, ts: Option<DateTime<Local>>) {
        let is_meta = v.get("isMeta").and_then(|m| m.as_bool()).unwrap_or(false);
        let content = &v["message"]["content"];
        if let Some(s) = content.as_str() {
            if s.contains("<task-notification>") {
                self.on_notifications(s, ts);
            } else if !is_meta {
                self.maybe_prompt(s, ts);
            }
        } else if let Some(arr) = content.as_array() {
            for c in arr {
                match c.get("type").and_then(|t| t.as_str()) {
                    Some("text") if !is_meta => {
                        if let Some(s) = c.get("text").and_then(|t| t.as_str()) {
                            if s.contains("<task-notification>") {
                                self.on_notifications(s, ts);
                            } else {
                                self.maybe_prompt(s, ts);
                            }
                        }
                    }
                    Some("tool_result") => {
                        let tuid = c.get("tool_use_id").and_then(|t| t.as_str()).unwrap_or("");
                        if let Some(r) = v.get("toolUseResult") {
                            self.on_tool_result(tuid, r, ts);
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    fn maybe_prompt(&mut self, s: &str, ts: Option<DateTime<Local>>) {
        let t = s.trim_start();
        if t.is_empty() || t.starts_with('<') || t.starts_with("Caveat:") || t.starts_with("[Request interrupted") {
            return;
        }
        let line = one_line(t, 300);
        self.last_prompt = Some(line.clone());
        self.push_event(ts, EventKind::Prompt, line);
    }

    fn on_tool_result(&mut self, tuid: &str, r: &Value, ts: Option<DateTime<Local>>) {
        let Some(obj) = r.as_object() else { return };
        let tu = self.tool_uses.get(tuid).cloned().unwrap_or_else(|| ToolUse { name: "?".into(), ..Default::default() });
        let (tool, desc, cmd) = (tu.name.clone(), tu.desc.clone(), tu.cmd.clone());
        let s = |k: &str| obj.get(k).and_then(|v| v.as_str()).map(str::to_string);

        let mut start: Option<(String, TaskKind, Option<PathBuf>)> = None;
        if let Some(id) = s("backgroundTaskId") {
            start = Some((id, TaskKind::Shell, None));
        } else if let Some(id) = s("taskId") {
            let kind = match tool.as_str() {
                "Workflow" => TaskKind::Workflow,
                "Agent" => TaskKind::Agent,
                "Bash" => TaskKind::Shell,
                _ => {
                    if s("taskType").as_deref() == Some("local_workflow") {
                        TaskKind::Workflow
                    } else {
                        TaskKind::Monitor
                    }
                }
            };
            start = Some((id, kind, None));
        } else if obj.get("isAsync").and_then(|b| b.as_bool()).unwrap_or(false)
            && let Some(id) = s("agentId") {
                start = Some((id, TaskKind::Agent, s("outputFile").map(PathBuf::from)));
            }

        if let Some((id, kind, out)) = start {
            let description = s("workflowName")
                .map(|w| match s("summary") {
                    Some(sum) => format!("{w}: {}", one_line(&sum, 100)),
                    None => w,
                })
                .unwrap_or_else(|| if desc.is_empty() { tool.clone() } else { desc.clone() });
            let persistent = obj.get("persistent").and_then(|b| b.as_bool()).unwrap_or(false);
            let expires = match (obj.get("timeoutMs").and_then(|t| t.as_i64()), ts) {
                (Some(ms), Some(ts)) if kind == TaskKind::Monitor && !persistent => {
                    Some(ts + chrono::Duration::milliseconds(ms))
                }
                _ => None,
            };
            self.tasks.entry(id.clone()).or_insert(TaskRec {
                kind,
                description: description.clone(),
                command: cmd,
                started: ts,
                status: None,
                ended: None,
                output_file: out,
                expires,
                raw_command: tu.raw_cmd.clone(),
                requested: tu.ts,
            });
            self.push_event(ts, EventKind::Notice, crate::i18n::tr!("▶ {} {id} iniciado: {description}", "▶ {} {id} started: {description}", kind.label()));
            return;
        }

        // Agente em primeiro plano terminou (o resultado chega direto no pai).
        if let (Some(aid), Some(st)) = (s("agentId"), s("status"))
            && st != "async_launched" {
                self.agent_done.insert(aid, st);
            }
        // TaskStop
        if tool == "TaskStop"
            && let Some(id) = s("task_id") {
                self.finish_task(&id, "killed", ts);
            }
    }

    fn on_notifications(&mut self, text: &str, ts: Option<DateTime<Local>>) {
        let bodies: Vec<String> = notif_re().captures_iter(text).map(|c| c[1].to_string()).collect();
        for body in bodies {
            let Some(status) = tag(&body, "status") else { continue };
            let st = status.to_lowercase();
            if st == "running" || st == "progress" || st == "started" {
                continue;
            }
            // Pode haver vários <task-id> (ex.: resumo de tasks órfãs de uma sessão anterior).
            let ids = all_tags(&body, "task-id");
            for id in ids.iter().filter(|i| !i.starts_with("__")) {
                self.finish_task(id, &st, ts);
            }
            if let (Some(first), Some(sum)) = (ids.first(), tag(&body, "summary")) {
                self.push_event(ts, EventKind::Notice, format!("■ {first} {st}: {}", one_line(&sum, 160)));
            }
        }
    }

    fn finish_task(&mut self, id: &str, status: &str, ts: Option<DateTime<Local>>) {
        if let Some(t) = self.tasks.get_mut(id)
            && t.status.is_none() {
                t.status = Some(status.to_string());
                t.ended = ts;
            }
        self.agent_done.entry(id.to_string()).or_insert_with(|| status.to_string());
    }
}
