//! Ações que mexem em processos: parar shells/monitors, encerrar sessões,
//! abrir, retomar e iniciar instâncias do Claude Code.

use crate::i18n::tr;
use std::path::Path;
use std::process::{Command, Stdio};

/// Modelos oferecidos ao abrir uma instância (aliases do `claude --model`).
pub const MODELS: [&str; 5] = ["padrão", "opus", "sonnet", "haiku", "fable"];

#[derive(Debug, Clone, Default)]
pub struct LaunchSpec {
    pub cwd: String,
    /// None = modelo padrão da configuração do usuário.
    pub model: Option<String>,
    pub name: Option<String>,
    pub prompt: Option<String>,
    /// true = `claude --bg` (sem janela); false = aba nova do terminal.
    pub background: bool,
}

#[derive(Debug, Clone)]
pub enum Action {
    /// Mata o processo (e filhos) de um shell/monitor em background.
    StopTask { session_id: String, task_id: String },
    /// Finaliza o processo de uma sessão interativa (e seus filhos).
    KillSession { session_id: String },
    /// `claude stop <id>`
    StopBg { id: String, name: String },
    /// `claude rm <id>`
    RemoveBg { id: String, name: String },
    Launch(LaunchSpec),
    /// `claude --resume <sessionId>` numa aba nova.
    Resume { session_id: String, cwd: String, name: String },
    /// `claude attach <id>` numa aba nova (sessão em background).
    Attach { id: String, cwd: String, name: String },
    /// Encerra o processo de uma sessão de outro agente (ex.: `agy`) aberta
    /// em qualquer terminal.
    KillAgent { session_id: String },
    /// Outro agente (Antigravity, Codex…) numa aba nova. `program` é o caminho já
    /// resolvido no PATH: o `wt` não acha sozinho os atalhos `.cmd` do npm.
    OpenAgent { program: String, args: Vec<String>, cwd: String, title: String },
}

/// Desliga os modos que o Claude Code liga no terminal e que ficam ligados se
/// ele é finalizado à força: captura de mouse (senão o shell passa a receber
/// `[<35;10;5M` a cada movimento), bracketed paste, foco, teclado estendido,
/// tela alternativa e cursor escondido.
pub const TERM_RESET: &str = "\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1005l\x1b[?1006l\x1b[?1015l\x1b[?1004l\x1b[?2004l\x1b[?9001l\x1b[<u\x1b[>4;0m\x1b[0m\x1b[?25h";

/// Escreve `TERM_RESET` no console do processo `pid` (o shell que sobrou na aba
/// depois que a sessão foi encerrada). Roda num processo auxiliar, porque um
/// processo só pode estar ligado a um console por vez.
#[cfg(windows)]
pub fn reset_console(pid: u32) -> Result<(), String> {
    use std::ffi::c_void;
    use std::io::Write;
    use std::os::windows::io::AsRawHandle;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn FreeConsole() -> i32;
        fn AttachConsole(pid: u32) -> i32;
        fn GetConsoleMode(h: *mut c_void, mode: *mut u32) -> i32;
        fn SetConsoleMode(h: *mut c_void, mode: u32) -> i32;
    }
    // Espera a sessão morrer de vez e parar de escrever na tela.
    std::thread::sleep(std::time::Duration::from_millis(400));
    unsafe {
        FreeConsole();
        if AttachConsole(pid) == 0 {
            return Err(format!("não consegui me ligar ao console do processo {pid}"));
        }
    }
    let mut out = std::fs::OpenOptions::new().read(true).write(true).open("CONOUT$").map_err(|e| e.to_string())?;
    unsafe {
        let h = out.as_raw_handle() as *mut c_void;
        let mut mode = 0u32;
        if GetConsoleMode(h, &mut mode) != 0 {
            SetConsoleMode(h, mode | 0x0004); // ENABLE_VIRTUAL_TERMINAL_PROCESSING
        }
    }
    out.write_all(TERM_RESET.as_bytes()).map_err(|e| e.to_string())?;
    if std::env::var_os("AGENT_DASH_RESET_MARK").is_some() {
        let _ = out.write_all(b"[reset aplicado]"); // só para o teste enxergar
    }
    out.flush().map_err(|e| e.to_string())
}

#[cfg(not(windows))]
pub fn reset_console(_pid: u32) -> Result<(), String> {
    Err("só no Windows".into())
}

/// Dispara o auxiliar que limpa o terminal do processo `pid`.
pub fn spawn_console_reset(pid: u32) {
    let Ok(exe) = std::env::current_exe() else { return };
    let mut c = Command::new(exe);
    c.args(["--reset-console", &pid.to_string()]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0000_0008); // DETACHED_PROCESS: nasce sem console
    }
    let _ = c.spawn();
}

/// Processos filhos não devem herdar o console da TUI (nem o Ctrl+C dela).
fn no_console(c: &mut Command) -> &mut Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        c.creation_flags(CREATE_NO_WINDOW);
    }
    c.stdin(Stdio::null())
}

/// Roda `claude <args>` sem console e devolve a saída (ou o erro).
pub fn claude_cli(args: &[String], cwd: Option<&str>) -> Result<String, String> {
    let mut c = Command::new("claude");
    c.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
    if let Some(d) = cwd {
        c.current_dir(d);
    }
    no_console(&mut c);
    let out = c.output().map_err(|e| tr!("não consegui rodar `claude`: {e}", "could not run `claude`: {e}"))?;
    let so = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let se = String::from_utf8_lossy(&out.stderr).trim().to_string();
    let pick = |a: String, b: String| if a.is_empty() { b } else { a };
    if out.status.success() { Ok(pick(so, se)) } else { Err(pick(se, so)) }
}

/// Abre `claude <args>` numa aba nova do Windows Terminal (ou numa janela nova
/// do console, se o `wt` não existir), com `cwd` como diretório.
pub fn open_terminal(cwd: &str, title: &str, args: &[String]) -> Result<(), String> {
    open_program_terminal("claude", cwd, title, args)
}

/// Como `open_terminal`, para qualquer programa (outro agente de linha de comando).
pub fn open_program_terminal(program: &str, cwd: &str, title: &str, args: &[String]) -> Result<(), String> {
    if !Path::new(cwd).is_dir() {
        return Err(tr!("diretório não existe: {cwd}", "directory does not exist: {cwd}"));
    }
    // `;` separa comandos no wt, e ele descarta aspas duplas dos argumentos.
    let clean = |s: &str| s.replace(';', ",").replace('"', "'");
    let args: Vec<String> = args.iter().map(|a| clean(a)).collect();
    let title = clean(title);
    let mut wt = Command::new("wt");
    wt.args(["-w", "0", "nt", "-d", cwd, "--title", &title, program]).args(&args);
    wt.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    if wt.spawn().is_ok() {
        return Ok(());
    }
    let mut c = Command::new("cmd");
    c.args(["/c", "start", &title, "/D", cwd, program]).args(&args);
    c.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    c.spawn().map(|_| ()).map_err(|e| tr!("não consegui abrir um terminal: {e}", "could not open a terminal: {e}"))
}

/// Argumentos do `claude` para uma instância nova.
pub fn launch_args(spec: &LaunchSpec) -> Vec<String> {
    let mut a = Vec::new();
    if spec.background {
        a.push("--bg".to_string());
    }
    if let Some(m) = &spec.model {
        a.extend(["--model".to_string(), m.clone()]);
    }
    if let Some(n) = spec.name.as_ref().filter(|n| !n.trim().is_empty()) {
        a.extend(["-n".to_string(), n.trim().to_string()]);
    }
    if let Some(p) = spec.prompt.as_ref().filter(|p| !p.trim().is_empty()) {
        a.push(p.trim().to_string());
    }
    a
}

/// Processo candidato a ser o shell de uma task.
#[derive(Debug, Clone)]
pub struct ProcCand {
    pub pid: u32,
    pub parent: Option<u32>,
    /// Início, segundos unix.
    pub start: u64,
    /// Argumentos já separados e unidos por espaço (sem o escape do Windows).
    pub cmd: String,
}

/// O Claude Code roda `bash -c "… eval '<comando>' …"`; aspas simples viram `'\''`.
fn cmd_matches(cmd: &str, raw: &str) -> bool {
    let raw = raw.trim();
    !raw.is_empty() && (cmd.contains(&raw.replace('\'', "'\\''")) || cmd.contains(raw))
}

/// Escolhe o processo de uma task entre os candidatos (shells descendentes da
/// sessão). Usa o comando e a janela de tempo em que o processo deve ter nascido.
/// Só devolve um PID quando a escolha não é ambígua.
pub fn pick_task_process(cands: &[ProcCand], raw_cmd: Option<&str>, requested: Option<i64>, started: Option<i64>) -> Result<u32, String> {
    let lo = requested.or(started).map(|t| t - 3);
    let hi = started.or(requested).map(|t| t + 3);
    let time_ok = |c: &ProcCand| lo.is_none_or(|lo| c.start as i64 >= lo) && hi.is_none_or(|hi| c.start as i64 <= hi);
    let by_cmd: Vec<&ProcCand> = match raw_cmd {
        Some(r) => cands.iter().filter(|c| cmd_matches(&c.cmd, r)).collect(),
        None => Vec::new(),
    };
    // Git Bash cria bash.exe -> usr/bin/bash.exe com a mesma linha: fica o de cima.
    let top = |v: Vec<&ProcCand>| -> Vec<u32> {
        let pids: Vec<u32> = v.iter().map(|c| c.pid).collect();
        v.iter().filter(|c| c.parent.is_none_or(|p| !pids.contains(&p))).map(|c| c.pid).collect()
    };
    let both = top(by_cmd.iter().copied().filter(|c| time_ok(c)).collect());
    if both.len() == 1 {
        return Ok(both[0]);
    }
    let cmd_only = top(by_cmd.clone());
    if cmd_only.len() == 1 {
        return Ok(cmd_only[0]);
    }
    if by_cmd.is_empty() {
        // PowerShell passa o script por variável de ambiente: só sobra o horário.
        let t = top(cands.iter().filter(|c| time_ok(c)).collect());
        if t.len() == 1 && (lo.is_some() || hi.is_some()) {
            return Ok(t[0]);
        }
        return Err(match t.len() {
            0 => crate::i18n::t("não achei o processo desta task (talvez já tenha terminado)", "process of this task not found (it may have finished)").into(),
            n => tr!("{n} processos possíveis; não dá para saber qual é com segurança", "{n} possible processes; cannot tell which one safely"),
        });
    }
    let n = cmd_only.len().max(both.len());
    Err(tr!("{n} processos com o mesmo comando; não dá para saber qual é com segurança", "{n} processes with the same command; cannot tell which one safely"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(pid: u32, parent: Option<u32>, start: u64, cmd: &str) -> ProcCand {
        ProcCand { pid, parent, start, cmd: cmd.into() }
    }

    #[test]
    fn acha_pelo_comando_e_horario() {
        let v = vec![
            c(10, Some(1), 1000, "bash -c source x && eval 'sleep 600' < /dev/null"),
            c(11, Some(10), 1000, "bash -c source x && eval 'sleep 600' < /dev/null"),
            c(12, Some(1), 1001, "bash -c source x && eval 'npm run dev' < /dev/null"),
        ];
        assert_eq!(pick_task_process(&v, Some("sleep 600"), Some(999), Some(1001)), Ok(10));
    }

    #[test]
    fn aspas_simples_escapadas() {
        let v = vec![c(10, Some(1), 1000, r#"bash -c eval 'echo '\''oi'\''' < /dev/null"#)];
        assert_eq!(pick_task_process(&v, Some("echo 'oi'"), None, None), Ok(10));
    }

    #[test]
    fn mesmo_comando_desempata_pelo_horario() {
        let v = vec![c(10, Some(1), 1000, "eval 'sleep 5'"), c(20, Some(1), 2000, "eval 'sleep 5'")];
        assert_eq!(pick_task_process(&v, Some("sleep 5"), Some(1999), Some(2000)), Ok(20));
        assert!(pick_task_process(&v, Some("sleep 5"), None, None).is_err());
    }

    #[test]
    fn powershell_so_pelo_horario() {
        let v = vec![c(10, Some(1), 1000, "cmd.exe /d /s /c pwsh -Command $__claudeCodeScript"), c(12, Some(1), 1500, "cmd.exe /c x")];
        assert_eq!(pick_task_process(&v, Some("Get-Process"), Some(999), Some(1000)), Ok(10));
        assert!(pick_task_process(&v, Some("Get-Process"), None, None).is_err());
    }

    #[test]
    fn args_de_lancamento() {
        let s = LaunchSpec {
            cwd: "C:\\x".into(),
            model: Some("opus".into()),
            name: Some(" minha ".into()),
            prompt: Some("".into()),
            background: true,
        };
        assert_eq!(launch_args(&s), vec!["--bg", "--model", "opus", "-n", "minha"]);
    }
}
