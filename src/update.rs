//! Atualização automática: ao abrir, confere o release mais recente no GitHub
//! e, se for mais novo, baixa o executável, troca o atual e relança.
//!
//! Sem crate de HTTP: usa o `curl` que já vem no Windows 10+. Qualquer falha
//! (sem rede, sem curl, download estranho) é silenciosa e o dashboard abre na
//! versão que está instalada.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
const REPO: &str = "https://github.com/Antonio-Ramon/agent-dash";
/// Nome do executável publicado em cada release.
const ASSET: &str = "agent-dash.exe";

/// "v1.2.3" ou "1.2.3" -> (1, 2, 3).
pub fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let mut it = s.trim().trim_start_matches('v').split('.');
    let v = (it.next()?.parse().ok()?, it.next()?.parse().ok()?, it.next()?.parse().ok()?);
    it.next().is_none().then_some(v)
}

/// Versão do release mais recente, lida do redirecionamento de
/// `/releases/latest` (não gasta o limite da API do GitHub).
fn latest_version() -> Option<String> {
    let out = Command::new("curl")
        .args(["-sI", "--max-time", "3", &format!("{REPO}/releases/latest")])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    tag_from_headers(&String::from_utf8_lossy(&out.stdout))
}

fn tag_from_headers(headers: &str) -> Option<String> {
    let loc = headers.lines().find(|l| l.to_ascii_lowercase().starts_with("location:"))?;
    let tag = loc.trim().rsplit_once("/releases/tag/")?.1.trim();
    parse_version(tag).map(|_| tag.trim_start_matches('v').to_string())
}

fn sibling(exe: &Path, suffix: &str) -> PathBuf {
    exe.with_file_name(format!("agent-dash.{suffix}.exe"))
}

/// Confere que `new` é um agent-dash que roda e se diz da versão `expected`
/// (pega download truncado, página de erro e DLL faltando) e o põe no lugar
/// de `exe`. O Windows não deixa sobrescrever um executável em uso, mas deixa
/// renomeá-lo: o atual vira `.old.exe` e é apagado na abertura seguinte.
pub fn install(new: &Path, exe: &Path, expected: &str) -> Result<(), String> {
    let out = Command::new(new)
        .arg("--version")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(|e| format!("o executável baixado não roda: {e}"))?;
    let said = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if said != format!("agent-dash {expected}") {
        return Err(format!("o executável baixado respondeu {said:?}, esperava a versão {expected}"));
    }
    let old = sibling(exe, "old");
    let _ = std::fs::remove_file(&old);
    std::fs::rename(exe, &old).map_err(|e| format!("não consegui mover o executável atual: {e}"))?;
    if let Err(e) = std::fs::rename(new, exe) {
        let _ = std::fs::rename(&old, exe);
        return Err(format!("não consegui instalar o executável novo: {e}"));
    }
    Ok(())
}

/// Relança o executável (já trocado) com os mesmos argumentos e devolve o
/// código de saída dele. A variável impede o filho de checar de novo.
fn relaunch(exe: &Path) -> i32 {
    Command::new(exe)
        .args(std::env::args_os().skip(1))
        .env("AGENT_DASH_UPDATED", "1")
        .status()
        .map(|s| s.code().unwrap_or(0))
        .unwrap_or(1)
}

/// Chamado ao abrir o dashboard. Devolve `Some(código)` se atualizou e a
/// versão nova já rodou até o fim (o chamador só precisa sair com o código).
pub fn auto() -> Option<i32> {
    let exe = std::env::current_exe().ok()?;
    // Sobra da atualização anterior.
    let _ = std::fs::remove_file(sibling(&exe, "old"));
    let _ = std::fs::remove_file(sibling(&exe, "new"));
    if std::env::var_os("AGENT_DASH_UPDATED").is_some() || std::env::var_os("AGENT_DASH_NO_UPDATE").is_some() {
        return None;
    }
    // Binário de desenvolvimento (`cargo run`, target\release): não mexe.
    if !cfg!(windows) || exe.components().any(|c| c.as_os_str() == "target") {
        return None;
    }
    let latest = latest_version()?;
    if parse_version(&latest)? <= parse_version(VERSION)? {
        return None;
    }
    println!("agent-dash {VERSION} -> {latest}: baixando a atualização…");
    let new = sibling(&exe, "new");
    let ok = Command::new("curl")
        .args(["-sfL", "--max-time", "120", "-o"])
        .arg(&new)
        .arg(format!("{REPO}/releases/download/v{latest}/{ASSET}"))
        .stdin(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !ok {
        let _ = std::fs::remove_file(&new);
        println!("não consegui baixar a atualização; abrindo a versão {VERSION}.");
        return None;
    }
    match install(&new, &exe, &latest) {
        Ok(()) => Some(relaunch(&exe)),
        Err(e) => {
            let _ = std::fs::remove_file(&new);
            println!("atualização não aplicada ({e}); abrindo a versão {VERSION}.");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versoes_comparam_como_numeros() {
        assert_eq!(parse_version("v0.2.0"), Some((0, 2, 0)));
        assert!(parse_version("0.10.0") > parse_version("0.9.9"));
        assert!(parse_version("1.0.0") > parse_version("0.99.99"));
        assert_eq!(parse_version("0.2"), None);
        assert_eq!(parse_version("0.2.0.1"), None);
        assert_eq!(parse_version("abc"), None);
    }

    #[test]
    fn tag_sai_do_redirecionamento() {
        let h = "HTTP/2 302\r\nserver: github.com\r\nLocation: https://github.com/x/y/releases/tag/v0.3.1\r\n\r\n";
        assert_eq!(tag_from_headers(h), Some("0.3.1".to_string()));
        // Sem release publicado o GitHub manda para /releases.
        assert_eq!(tag_from_headers("HTTP/2 302\r\nlocation: https://github.com/x/y/releases\r\n"), None);
        assert_eq!(tag_from_headers("HTTP/2 404\r\n"), None);
    }
}
