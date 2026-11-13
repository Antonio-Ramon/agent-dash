//! Agentes de linha de comando que podem ser abertos pelo dashboard.
//!
//! O Claude Code é o agente principal: é o único monitorado (sessões, tokens,
//! subagentes, tasks), porque o agent-dash lê o `~/.claude`. Os outros
//! (Antigravity, Codex…) só rodam no chat embutido ou numa aba nova; o
//! dashboard não lê o histórico deles.
//!
//! A lista embutida pode ser estendida ou sobrescrita em
//! `%LOCALAPPDATA%\agent-dash\agentes.json` (mesmo `nome` = substitui).

use serde::Deserialize;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct AgentDef {
    /// Nome mostrado no formulário e na lista de sessões.
    #[serde(rename = "nome")]
    pub name: String,
    /// Programa a rodar (procurado no PATH, como no terminal).
    #[serde(rename = "comando")]
    pub command: String,
    /// Argumentos fixos, antes dos de modelo e prompt.
    #[serde(rename = "args", default)]
    pub args: Vec<String>,
    /// Modelos oferecidos além de "padrão" (que não passa flag nenhuma).
    #[serde(rename = "modelos", default)]
    pub models: Vec<String>,
    /// Flag que escolhe o modelo (ex.: `--model`). Sem ela, não há escolha de modelo.
    #[serde(rename = "flag_modelo", default)]
    pub model_flag: Option<String>,
    /// Flag do prompt inicial. Vazia = o prompt vai como argumento solto.
    #[serde(rename = "flag_prompt", default)]
    pub prompt_flag: Option<String>,
    /// Como instalar, mostrado quando o programa não está no PATH.
    #[serde(rename = "instalar", default)]
    pub install: Option<String>,
    /// Flag que retoma uma conversa pelo id (ex.: `--conversation`). Sem ela,
    /// conversas encerradas não podem ser reabertas pelo dashboard.
    #[serde(rename = "flag_retomar", default)]
    pub resume_flag: Option<String>,
}

impl AgentDef {
    pub fn is_claude(&self) -> bool {
        self.command.eq_ignore_ascii_case("claude")
    }

    /// Argumentos para retomar a conversa `id`, se o agente sabe retomar.
    pub fn resume_args(&self, id: &str) -> Option<Vec<String>> {
        let flag = self.resume_flag.as_ref().filter(|f| !f.is_empty())?;
        let mut a = self.args.clone();
        a.extend([flag.clone(), id.to_string()]);
        Some(a)
    }

    /// Opções do campo Modelo: "padrão" e os modelos configurados.
    pub fn model_choices(&self) -> Vec<String> {
        let mut v = vec!["padrão".to_string()];
        if self.model_flag.is_some() || self.is_claude() {
            v.extend(self.models.iter().cloned());
        }
        v
    }

    /// Argumentos para abrir este agente (não vale para o Claude, que tem os seus).
    pub fn launch_args(&self, model: Option<&str>, prompt: Option<&str>) -> Vec<String> {
        let mut a = self.args.clone();
        if let (Some(flag), Some(m)) = (&self.model_flag, model.map(str::trim).filter(|m| !m.is_empty())) {
            a.extend([flag.clone(), m.to_string()]);
        }
        if let Some(p) = prompt.map(str::trim).filter(|p| !p.is_empty()) {
            if let Some(flag) = self.prompt_flag.as_ref().filter(|f| !f.is_empty()) {
                a.push(flag.clone());
            }
            a.push(p.to_string());
        }
        a
    }
}

/// Agentes conhecidos de fábrica. O Claude vem primeiro e é o padrão.
pub fn builtin() -> Vec<AgentDef> {
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    vec![
        AgentDef {
            name: "Claude".into(),
            command: "claude".into(),
            args: Vec::new(),
            models: s(&crate::actions::MODELS[1..]),
            model_flag: Some("--model".into()),
            prompt_flag: None,
            install: None,
            resume_flag: Some("--resume".into()),
        },
        // Antigravity CLI (`agy`). Modelos de `agy models` (nov/2026); a lista
        // muda com o tempo e pode ser trocada no agentes.json.
        AgentDef {
            name: "Antigravity".into(),
            command: "agy".into(),
            args: Vec::new(),
            models: s(&[
                "gemini-3.8-flash-high",
                "gemini-3.8-flash-medium",
                "gemini-3.8-flash-low",
                "claude-sonnet-4-6",
                "claude-opus-4-6-thinking",
                "gpt-oss-120b-medium",
            ]),
            model_flag: Some("--model".into()),
            // Roda o prompt e continua no modo interativo.
            prompt_flag: Some("--prompt-interactive".into()),
            install: None,
            resume_flag: Some("--conversation".into()),
        },
        AgentDef {
            name: "Codex".into(),
            command: "codex".into(),
            args: Vec::new(),
            models: Vec::new(),
            model_flag: Some("--model".into()),
            prompt_flag: None,
            install: Some("npm install -g @openai/codex".into()),
            resume_flag: None,
        },
    ]
}

/// Junta a lista de fábrica com a do usuário: mesmo nome substitui, nome novo
/// entra no fim. O Claude continua sempre em primeiro.
pub fn merge(mut base: Vec<AgentDef>, user: Vec<AgentDef>) -> Vec<AgentDef> {
    for u in user {
        if u.name.trim().is_empty() || u.command.trim().is_empty() {
            continue;
        }
        match base.iter_mut().find(|b| b.name.eq_ignore_ascii_case(&u.name)) {
            Some(b) if b.is_claude() => {} // o Claude é a base do dashboard; não dá para trocar
            // Fica o nome de fábrica: o coletor e os chats comparam por ele.
            Some(b) => *b = AgentDef { name: std::mem::take(&mut b.name), ..u },
            None => base.push(u),
        }
    }
    base
}

pub fn config_path() -> Option<PathBuf> {
    dirs::data_local_dir().map(|d| d.join("agent-dash").join("agentes.json"))
}

/// Lista de agentes: a de fábrica mais a do `agentes.json`, se houver.
/// Devolve também o erro de leitura do arquivo, para avisar na tela.
pub fn load() -> (Vec<AgentDef>, Option<String>) {
    let Some(p) = config_path() else { return (builtin(), None) };
    let Ok(txt) = std::fs::read_to_string(&p) else { return (builtin(), None) };
    match serde_json::from_str::<Vec<AgentDef>>(&txt) {
        Ok(user) => (merge(builtin(), user), None),
        Err(e) => (builtin(), Some(crate::i18n::tr!("{} ignorado: {e}", "{} ignored: {e}", p.display()))),
    }
}

/// Procura `name` como o terminal faria: caminho direto, ou cada pasta do
/// `path` com cada extensão do `pathext` (no Windows, `codex` acha `codex.cmd`).
pub fn find_program(name: &str, path: Option<&OsStr>, pathext: Option<&OsStr>) -> Option<PathBuf> {
    let direct = Path::new(name);
    if direct.components().count() > 1 || direct.is_absolute() {
        return direct.is_file().then(|| direct.to_path_buf());
    }
    let exts: Vec<String> = match pathext {
        Some(pe) => pe.to_string_lossy().split(';').filter(|e| !e.is_empty()).map(|e| e.to_lowercase()).collect(),
        None => Vec::new(),
    };
    for dir in std::env::split_paths(path?) {
        let base = dir.join(name);
        if Path::new(name).extension().is_some() && base.is_file() {
            return Some(base);
        }
        for e in &exts {
            let cand = dir.join(format!("{name}{e}"));
            if cand.is_file() {
                return Some(cand);
            }
        }
        if cfg!(not(windows)) && base.is_file() {
            return Some(base);
        }
    }
    None
}

/// `find_program` com o PATH e o PATHEXT deste processo.
pub fn which(name: &str) -> Option<PathBuf> {
    let pathext = std::env::var_os("PATHEXT").or_else(|| cfg!(windows).then(|| ".COM;.EXE;.BAT;.CMD".into()));
    find_program(name, std::env::var_os("PATH").as_deref(), pathext.as_deref())
}

/// Programas instalados pelo npm no Windows são atalhos `.cmd`, que passam
/// pelo `cmd.exe`. Lá, `"`, `%`, `&`, `|`, `<`, `>` e `^` num argumento podem
/// cortar o comando ou rodar outro. Nesses casos o prompt inicial é recusado
/// (dá para mandar a mesma mensagem depois que o chat abrir).
pub fn unsafe_for_batch(program: &Path, arg: &str) -> bool {
    let batch = program
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("cmd") || e.eq_ignore_ascii_case("bat"));
    batch && arg.chars().any(|c| matches!(c, '"' | '%' | '&' | '|' | '<' | '>' | '^' | '\n' | '\r'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn antigravity() -> AgentDef {
        builtin().into_iter().find(|a| a.name == "Antigravity").unwrap()
    }

    #[test]
    fn agentes_de_fabrica() {
        let names: Vec<String> = builtin().into_iter().map(|a| a.name).collect();
        assert_eq!(names, vec!["Claude", "Antigravity", "Codex"]);
    }

    #[test]
    fn args_do_antigravity() {
        let agy = antigravity();
        assert_eq!(agy.command, "agy");
        assert_eq!(agy.launch_args(None, None), Vec::<String>::new());
        assert_eq!(
            agy.launch_args(Some("gemini-3.8-flash-high"), Some("  explica o projeto ")),
            vec!["--model", "gemini-3.8-flash-high", "--prompt-interactive", "explica o projeto"]
        );
        assert_eq!(agy.launch_args(Some(""), Some("   ")), Vec::<String>::new());
    }

    #[test]
    fn prompt_solto_quando_nao_ha_flag() {
        let codex = builtin().into_iter().find(|a| a.name == "Codex").unwrap();
        assert_eq!(codex.launch_args(None, Some("oi")), vec!["oi"]);
        assert_eq!(codex.model_choices(), vec!["padrão"]);
    }

    #[test]
    fn sem_flag_de_modelo_so_tem_padrao() {
        let a = AgentDef { model_flag: None, models: vec!["x".into()], ..antigravity() };
        assert_eq!(a.model_choices(), vec!["padrão"]);
        assert_eq!(a.launch_args(Some("x"), None), Vec::<String>::new());
    }

    #[test]
    fn config_do_usuario() {
        let user: Vec<AgentDef> = serde_json::from_str(
            r#"[
                {"nome": "antigravity", "comando": "agy", "modelos": ["modelo-novo"], "flag_modelo": "-m"},
                {"nome": "Aider", "comando": "aider", "flag_prompt": "--message"},
                {"nome": "Claude", "comando": "outro"},
                {"nome": "", "comando": "x"}
            ]"#,
        )
        .unwrap();
        let v = merge(builtin(), user);
        let names: Vec<&str> = v.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, vec!["Claude", "Antigravity", "Codex", "Aider"]);
        assert_eq!(v[0].command, "claude");
        assert_eq!(v[1].model_choices(), vec!["padrão", "modelo-novo"]);
        assert_eq!(v[1].launch_args(Some("modelo-novo"), None), vec!["-m", "modelo-novo"]);
    }

    #[test]
    fn acha_cmd_pelo_pathext() {
        let dir = std::env::temp_dir().join(format!("agent-dash-teste-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("agentefalso.cmd"), "@echo off\r\n").unwrap();
        let path = std::env::join_paths([Path::new("C:\\nao\\existe"), &dir]).unwrap();
        let found = find_program("agentefalso", Some(&path), Some(OsStr::new(".EXE;.CMD")));
        assert_eq!(found, Some(dir.join("agentefalso.cmd")));
        assert_eq!(find_program("naoexiste", Some(&path), Some(OsStr::new(".EXE;.CMD"))), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Abre de verdade cada agente instalado (sem mandar prompt, então não
    /// gasta cota) e imprime a tela. Manual: `cargo test -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn agentes_reais_abrem_no_terminal_embutido() {
        let cwd = std::env::current_dir().unwrap().display().to_string();
        for a in builtin().into_iter().filter(|a| !a.is_claude()) {
            let Some(prog) = which(&a.command) else {
                println!("===== {}: não instalado", a.name);
                continue;
            };
            let mut t = crate::term::Term::spawn_program(&prog.to_string_lossy(), "k".into(), a.name.clone(), &cwd, &[], 30, 110).unwrap();
            std::thread::sleep(std::time::Duration::from_secs(12));
            println!("===== {} ({}) · terminou: {}", a.name, prog.display(), t.exited());
            for l in t.contents().lines().filter(|l| !l.trim().is_empty()).take(25) {
                println!("{l}");
            }
            t.kill();
        }
    }

    #[test]
    fn prompt_perigoso_para_cmd() {
        let cmd = Path::new("C:\\x\\agente.cmd");
        assert!(unsafe_for_batch(cmd, "faz isso & aquilo"));
        assert!(unsafe_for_batch(cmd, "100%"));
        assert!(!unsafe_for_batch(cmd, "explica o projeto, por favor?"));
        assert!(!unsafe_for_batch(Path::new("C:\\x\\codex.exe"), "a & b"));
    }
}
