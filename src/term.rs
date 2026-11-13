//! Terminal embutido: roda o `claude` (ou outro agente) num pseudo-terminal (ConPTY) e mantém
//! a tela dele num emulador VT, para ser desenhada dentro do painel.

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Ligado pela thread leitora quando chega saída nova de algum chat; o loop
/// da interface redesenha na hora em vez de esperar o próximo ciclo.
static DIRTY: AtomicBool = AtomicBool::new(false);

pub fn take_dirty() -> bool {
    DIRTY.swap(false, Ordering::Relaxed)
}

/// Linhas de histórico guardadas para rolar (Shift+PgUp).
const SCROLLBACK: usize = 5000;

pub struct Term {
    /// Id da sessão do Claude Code que roda aqui.
    pub key: String,
    pub title: String,
    /// Diretório onde o `claude` foi iniciado.
    pub cwd: String,
    /// false = attach numa sessão em background, que segue viva sem o dashboard.
    pub restorable: bool,
    /// Agente que roda aqui quando não é o Claude Code (ex.: "Antigravity").
    pub agent: Option<String>,
    /// Linha de comando usada para abrir (mostrada no resumo da sessão).
    pub command_line: String,
    pub started: chrono::DateTime<chrono::Local>,
    pub parser: Arc<Mutex<vt100::Parser>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    pub rows: u16,
    pub cols: u16,
    /// Linhas roladas para cima (0 = fim).
    pub scroll: usize,
}

impl Term {
    pub fn spawn(key: String, title: String, cwd: &str, args: &[String], rows: u16, cols: u16) -> Result<Term, String> {
        Self::spawn_program("claude", key, title, cwd, args, rows, cols)
    }

    pub fn pid(&self) -> Option<u32> {
        self.child.process_id()
    }

    pub fn spawn_program(program: &str, key: String, title: String, cwd: &str, args: &[String], rows: u16, cols: u16) -> Result<Term, String> {
        if !std::path::Path::new(cwd).is_dir() {
            return Err(crate::i18n::tr!("diretório não existe: {cwd}", "directory does not exist: {cwd}"));
        }
        let (rows, cols) = (rows.max(5), cols.max(20));
        let pair = native_pty_system()
            .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .map_err(|e| crate::i18n::tr!("não consegui criar o terminal: {e}", "could not create the terminal: {e}"))?;
        let mut cmd = CommandBuilder::new(program);
        cmd.args(args);
        cmd.cwd(cwd);
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        // Se o dashboard foi aberto de dentro de uma sessão do Claude, o chat
        // não deve se comportar como filho dela (senão não salva a conversa).
        cmd.env_remove("CLAUDE_CODE_CHILD_SESSION");
        cmd.env_remove("CLAUDECODE");
        let child = pair.slave.spawn_command(cmd).map_err(|e| crate::i18n::tr!("não consegui iniciar {program}: {e}", "could not start {program}: {e}"))?;
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().map_err(|e| e.to_string())?;
        let writer: Arc<Mutex<Box<dyn Write + Send>>> = Arc::new(Mutex::new(pair.master.take_writer().map_err(|e| e.to_string())?));
        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, SCROLLBACK)));

        let (p, w) = (parser.clone(), writer.clone());
        let _ = std::thread::Builder::new().name("pty-reader".into()).spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let chunk = &buf[..n];
                        // Perguntas que o console faz ao terminal e fica esperando.
                        let has = |pat: &[u8]| chunk.windows(pat.len()).any(|x| x == pat);
                        let mut reply: Vec<u8> = Vec::new();
                        if has(b"\x1b[6n") {
                            let pos = p.lock().unwrap().screen().cursor_position();
                            reply.extend(format!("\x1b[{};{}R", pos.0 + 1, pos.1 + 1).bytes());
                        }
                        if has(b"\x1b[c") || has(b"\x1b[0c") {
                            reply.extend(b"\x1b[?62;22c");
                        }
                        p.lock().unwrap().process(chunk);
                        DIRTY.store(true, Ordering::Relaxed);
                        if !reply.is_empty() {
                            let mut w = w.lock().unwrap();
                            let _ = w.write_all(&reply);
                            let _ = w.flush();
                        }
                    }
                }
            }
        });
        let command_line = std::iter::once(program.to_string()).chain(args.iter().cloned()).collect::<Vec<_>>().join(" ");
        Ok(Term {
            key,
            title,
            cwd: cwd.to_string(),
            restorable: true,
            agent: None,
            command_line,
            started: chrono::Local::now(),
            parser,
            writer,
            master: pair.master,
            child,
            rows,
            cols,
            scroll: 0,
        })
    }

    pub fn send(&mut self, bytes: &[u8]) {
        self.scroll = 0;
        let mut w = self.writer.lock().unwrap();
        let _ = w.write_all(bytes);
        let _ = w.flush();
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        let (rows, cols) = (rows.max(5), cols.max(20));
        if (rows, cols) == (self.rows, self.cols) {
            return;
        }
        if self.master.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 }).is_ok() {
            self.parser.lock().unwrap().set_size(rows, cols);
            self.rows = rows;
            self.cols = cols;
        }
    }

    pub fn exited(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)))
    }

    pub fn kill(&mut self) {
        let _ = self.child.kill();
    }

    pub fn scroll_by(&mut self, delta: isize) {
        self.scroll = (self.scroll as isize + delta).clamp(0, SCROLLBACK as isize) as usize;
    }

    /// Um passo da roda do mouse em (col, row), relativo ao canto do chat.
    /// Como num terminal de verdade: o Claude Code em tela cheia pede os
    /// eventos de mouse e rola a conversa sozinho, então a roda vai para ele.
    /// Os outros programas rolam no histórico guardado aqui. Devolve se a
    /// roda foi para o programa (aí a tela muda por conta dele).
    pub fn wheel(&mut self, up: bool, col: u16, row: u16) -> bool {
        let bytes = {
            let p = self.parser.lock().unwrap();
            let s = p.screen();
            wheel_bytes(s.mouse_protocol_mode(), s.mouse_protocol_encoding(), s.alternate_screen(), s.application_cursor(), up, col, row)
        };
        match bytes {
            Some(b) => {
                self.send(&b);
                true
            }
            None => {
                self.scroll_by(if up { 3 } else { -3 });
                false
            }
        }
    }

    /// Cola texto (com bracketed paste, se o programa pediu).
    pub fn paste(&mut self, text: &str) {
        let bracketed = self.parser.lock().unwrap().screen().bracketed_paste();
        if bracketed {
            self.send(format!("\x1b[200~{text}\x1b[201~").as_bytes());
        } else {
            self.send(text.as_bytes());
        }
    }

    pub fn contents(&self) -> String {
        self.parser.lock().unwrap().screen().contents()
    }

    /// Última linha com texto na tela (para dizer por que o processo saiu).
    pub fn last_line(&self) -> String {
        self.contents().lines().rev().map(str::trim).find(|l| !l.is_empty()).unwrap_or("").to_string()
    }
}

impl Drop for Term {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Texto entre dois pontos da tela do chat, inclusive. Cada ponto é
/// (linha, coluna), com a linha contada a partir do topo da tela atual:
/// 0 é a primeira linha visível sem rolagem, negativos estão no histórico.
/// Assim a seleção continua no mesmo texto quando o chat rola.
pub fn text_between(p: &mut vt100::Parser, a: (i64, u16), b: (i64, u16)) -> String {
    let (s, e) = if a <= b { (a, b) } else { (b, a) };
    let (rows, cols) = p.screen().size();
    let mut out = String::new();
    for l in s.0..=e.0 {
        // Rola o mínimo para a linha `l` ficar visível.
        let sb = if l < 0 { (-l) as usize } else { 0 };
        p.set_scrollback(sb);
        if p.screen().scrollback() != sb {
            continue; // já saiu do histórico
        }
        let r = (l + sb as i64) as u16;
        if r >= rows {
            continue;
        }
        let c0 = if l == s.0 { s.1.min(cols) } else { 0 };
        let c1 = if l == e.0 { (e.1 + 1).min(cols) } else { cols };
        let text = if c0 < c1 { p.screen().rows(c0, c1 - c0).nth(r as usize).unwrap_or_default() } else { String::new() };
        if l != e.0 && p.screen().row_wrapped(r) {
            // Linha longa quebrada pelo terminal: continua na próxima sem \n.
            out.push_str(&text);
        } else {
            out.push_str(text.trim_end());
            if l != e.0 {
                out.push('\n');
            }
        }
    }
    p.set_scrollback(0);
    out
}

/// Bytes que um terminal (xterm) manda ao programa por um passo da roda, ou
/// `None` quando a roda deve rolar o histórico local. Com o mouse ligado pelo
/// programa vai o evento de mouse (botões 64/65); na tela alternativa sem
/// mouse vão três setas, como o "alternate scroll" do xterm.
fn wheel_bytes(
    mode: vt100::MouseProtocolMode,
    enc: vt100::MouseProtocolEncoding,
    alt: bool,
    app_cursor: bool,
    up: bool,
    col: u16,
    row: u16,
) -> Option<Vec<u8>> {
    use vt100::{MouseProtocolEncoding as E, MouseProtocolMode as M};
    let button: u32 = if up { 64 } else { 65 };
    let (x, y) = (col as u32 + 1, row as u32 + 1);
    if mode != M::None {
        return Some(match enc {
            E::Sgr => format!("\x1b[<{button};{x};{y}M").into_bytes(),
            E::Utf8 => {
                let ch = |v: u32| char::from_u32(32 + v.min(2015)).unwrap_or(' ');
                format!("\x1b[M{}{}{}", ch(button), ch(x), ch(y)).into_bytes()
            }
            // X10: um byte por valor, só alcança até a coluna 223.
            E::Default => vec![0x1b, b'[', b'M', (32 + button) as u8, (32 + x.min(223)) as u8, (32 + y.min(223)) as u8],
        });
    }
    if alt {
        let arrow: &[u8] = match (up, app_cursor) {
            (true, true) => b"\x1bOA",
            (true, false) => b"\x1b[A",
            (false, true) => b"\x1bOB",
            (false, false) => b"\x1b[B",
        };
        return Some(arrow.repeat(3));
    }
    None
}

/// AltGr no Windows chega como Ctrl+Alt com o caractere já resolvido pelo
/// layout (ex.: ABNT2 AltGr+Q = `/`, AltGr+W = `?`, AltGr+E = `°`). Ctrl+Alt
/// de verdade só faz sentido com letras ASCII, então qualquer outro caractere
/// com os dois modificadores é tratado como texto digitado.
pub fn is_altgr(k: &KeyEvent) -> bool {
    let ctrl_alt = k.modifiers.contains(KeyModifiers::CONTROL | KeyModifiers::ALT);
    matches!(k.code, KeyCode::Char(c) if ctrl_alt && !c.is_ascii_alphabetic())
}

/// Tecla -> bytes que um terminal mandaria para o programa.
pub fn key_bytes(k: &KeyEvent) -> Option<Vec<u8>> {
    if let (true, KeyCode::Char(c)) = (is_altgr(k), k.code) {
        return Some(c.to_string().into_bytes());
    }
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    let alt = k.modifiers.contains(KeyModifiers::ALT);
    let shift = k.modifiers.contains(KeyModifiers::SHIFT);
    // xterm: 1 + shift(1) + alt(2) + ctrl(4)
    let m = 1 + shift as u8 + 2 * alt as u8 + 4 * ctrl as u8;
    let csi = |fin: char| if m > 1 { format!("\x1b[1;{m}{fin}") } else { format!("\x1b[{fin}") };
    let tilde = |n: u8| if m > 1 { format!("\x1b[{n};{m}~") } else { format!("\x1b[{n}~") };
    let s: String = match k.code {
        KeyCode::Char(c) => {
            let mut out = String::new();
            if alt {
                out.push('\x1b');
            }
            if ctrl {
                let b = match c.to_ascii_lowercase() {
                    c @ 'a'..='z' => c as u8 - b'a' + 1,
                    ' ' | '@' | '2' => 0,
                    '[' | '3' => 0x1b,
                    '\\' | '4' => 0x1c,
                    ']' | '5' => 0x1d,
                    '^' | '6' => 0x1e,
                    '_' | '7' | '/' => 0x1f,
                    '8' | '?' => 0x7f,
                    _ => return None,
                };
                out.push(b as char);
            } else {
                out.push(c);
            }
            out
        }
        // Ctrl+Enter (ou Alt+Enter) = quebra de linha no prompt do Claude Code.
        KeyCode::Enter if ctrl || alt => "\x1b\r".into(),
        KeyCode::Enter => "\r".into(),
        KeyCode::Backspace if ctrl || alt => "\x1b\x7f".into(),
        KeyCode::Backspace => "\x7f".into(),
        KeyCode::Tab => "\t".into(),
        KeyCode::BackTab => "\x1b[Z".into(),
        KeyCode::Esc => "\x1b".into(),
        KeyCode::Up => csi('A'),
        KeyCode::Down => csi('B'),
        KeyCode::Right => csi('C'),
        KeyCode::Left => csi('D'),
        KeyCode::Home => csi('H'),
        KeyCode::End => csi('F'),
        KeyCode::Insert => tilde(2),
        KeyCode::Delete => tilde(3),
        KeyCode::PageUp => tilde(5),
        KeyCode::PageDown => tilde(6),
        KeyCode::F(n @ 1..=4) => {
            let fin = [b'P', b'Q', b'R', b'S'][n as usize - 1] as char;
            if m > 1 { format!("\x1b[1;{m}{fin}") } else { format!("\x1bO{fin}") }
        }
        KeyCode::F(n @ 5..=12) => tilde([15, 17, 18, 19, 20, 21, 23, 24][n as usize - 5]),
        _ => return None,
    };
    Some(s.into_bytes())
}

/// UUID v4 (o `claude --session-id` exige um UUID válido).
pub fn new_uuid() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let r = || {
        let mut h = RandomState::new().build_hasher();
        h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
        h.finish()
    };
    let (a, b) = (r(), r());
    format!(
        "{:08x}-{:04x}-4{:03x}-{:04x}-{:012x}",
        (a >> 32) as u32,
        (a >> 16) as u16,
        a as u16 & 0x0fff,
        (b >> 48) as u16 & 0x3fff | 0x8000,
        b & 0xffff_ffff_ffff
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(code: KeyCode, m: KeyModifiers) -> Option<Vec<u8>> {
        key_bytes(&KeyEvent::new(code, m))
    }

    #[test]
    fn teclas_basicas() {
        assert_eq!(k(KeyCode::Char('a'), KeyModifiers::NONE), Some(b"a".to_vec()));
        assert_eq!(k(KeyCode::Char('ç'), KeyModifiers::NONE), Some("ç".as_bytes().to_vec()));
        assert_eq!(k(KeyCode::Char('c'), KeyModifiers::CONTROL), Some(vec![3]));
        assert_eq!(k(KeyCode::Enter, KeyModifiers::NONE), Some(b"\r".to_vec()));
        assert_eq!(k(KeyCode::Enter, KeyModifiers::CONTROL), Some(b"\x1b\r".to_vec()));
        assert_eq!(k(KeyCode::Up, KeyModifiers::NONE), Some(b"\x1b[A".to_vec()));
        assert_eq!(k(KeyCode::Left, KeyModifiers::CONTROL), Some(b"\x1b[1;5D".to_vec()));
        assert_eq!(k(KeyCode::BackTab, KeyModifiers::SHIFT), Some(b"\x1b[Z".to_vec()));
        assert_eq!(k(KeyCode::F(5), KeyModifiers::NONE), Some(b"\x1b[15~".to_vec()));
    }

    #[test]
    fn altgr_vira_texto() {
        let altgr = KeyModifiers::CONTROL | KeyModifiers::ALT;
        assert_eq!(k(KeyCode::Char('/'), altgr), Some(b"/".to_vec()));
        assert_eq!(k(KeyCode::Char('?'), altgr), Some(b"?".to_vec()));
        assert_eq!(k(KeyCode::Char('°'), altgr), Some("°".as_bytes().to_vec()));
        assert_eq!(k(KeyCode::Char('@'), altgr), Some(b"@".to_vec()));
        // Ctrl+Alt com letra continua sendo atalho (ESC + controle).
        assert_eq!(k(KeyCode::Char('a'), altgr), Some(b"\x1b\x01".to_vec()));
    }

    #[test]
    fn selecao_pega_texto_do_historico() {
        let mut p = vt100::Parser::new(3, 10, 100);
        p.process(b"linha1\r\nlinha2\r\nlinha3\r\nlinha4\r\nlinha5");
        // Tela: linha3..linha5; linha1 e linha2 estão no histórico (-2, -1).
        assert_eq!(text_between(&mut p, (-2, 0), (0, 3)), "linha1\nlinha2\nlinh");
        // Arrastar de baixo para cima dá o mesmo texto.
        assert_eq!(text_between(&mut p, (0, 3), (-2, 0)), "linha1\nlinha2\nlinh");
        assert_eq!(text_between(&mut p, (2, 2), (2, 9)), "nha5");
        assert_eq!(p.screen().scrollback(), 0);
    }

    #[test]
    fn roda_vai_para_o_programa_que_pediu_mouse() {
        let mut p = vt100::Parser::new(10, 40, 100);
        let bytes = |p: &vt100::Parser, up| {
            let s = p.screen();
            wheel_bytes(s.mouse_protocol_mode(), s.mouse_protocol_encoding(), s.alternate_screen(), s.application_cursor(), up, 4, 2)
        };
        // Terminal comum: a roda rola o histórico local.
        assert_eq!(bytes(&p, true), None);
        // Tela alternativa sem mouse: setas.
        p.process(b"\x1b[?1049h");
        assert_eq!(bytes(&p, true), Some(b"\x1b[A\x1b[A\x1b[A".to_vec()));
        // Mouse em modo X10 (coordenadas a partir de 1, somadas a 32).
        p.process(b"\x1b[?1000h");
        assert_eq!(bytes(&p, false), Some(vec![0x1b, b'[', b'M', 32 + 65, 32 + 5, 32 + 3]));
        // Como o Claude Code em tela cheia: SGR.
        p.process(b"\x1b[?1003h\x1b[?1006h");
        assert_eq!(bytes(&p, true), Some(b"\x1b[<64;5;3M".to_vec()));
        assert_eq!(bytes(&p, false), Some(b"\x1b[<65;5;3M".to_vec()));
    }

    #[test]
    fn uuid_valido() {
        let u = new_uuid();
        assert_eq!(u.len(), 36);
        assert_eq!(u.as_bytes()[14], b'4');
        assert!(matches!(u.as_bytes()[19], b'8' | b'9' | b'a' | b'b'));
        assert_ne!(u, new_uuid());
    }
}
