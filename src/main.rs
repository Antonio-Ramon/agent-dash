mod actions;
mod app;
mod collector;
mod model;
mod scan;
mod term;
mod ui;
mod update;

use ratatui::crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind, MouseEventKind};
use ratatui::crossterm::execute;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const HELP: &str = "agent-dash — monitor do Claude Code (estilo lazygit)

Uso:
  agent-dash                  abre o dashboard
  agent-dash --dump           imprime um resumo em texto (sem TUI) e sai
  agent-dash --stop-task ID   para um shell/monitor em background pelo id
  agent-dash --fix-terminal   conserta a aba atual se ela ficou imprimindo códigos ao mexer o mouse
  agent-dash --frame [LxA]    desenha as telas em memória e imprime (depuração)
  agent-dash --chat-test      abre um chat embutido, manda uma mensagem e imprime (depuração)
  agent-dash --version        mostra a versão
  agent-dash --update         procura e instala uma versão mais nova, sem abrir a interface
  agent-dash --help           esta mensagem

Lê ~/.claude (ou $CLAUDE_CONFIG_DIR). Dentro do dashboard, aperte ? para ver os atalhos.

Ao abrir, o dashboard procura uma versão mais nova no GitHub e se atualiza sozinho.
Para desligar isso, defina a variável de ambiente AGENT_DASH_NO_UPDATE=1.";

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{HELP}");
        return Ok(());
    }
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("agent-dash {}", update::VERSION);
        return Ok(());
    }
    if let Some(i) = args.iter().position(|a| a == "--install-from") {
        // Depuração: troca este executável por outro já baixado (mesmo caminho da atualização automática).
        let (new, ver) = args.get(i + 1).zip(args.get(i + 2)).ok_or_else(|| anyhow::anyhow!("uso: --install-from <exe> <versão>"))?;
        let exe = std::env::current_exe()?;
        update::install(std::path::Path::new(new), &exe, ver).map_err(|e| anyhow::anyhow!(e))?;
        println!("instalado em {}", exe.display());
        return Ok(());
    }
    if args.iter().any(|a| a == "--update") {
        // Mesma checagem feita ao abrir o dashboard, sem abrir a interface.
        if let Some(code) = update::auto() {
            std::process::exit(code);
        }
        println!("agent-dash {}: nenhuma atualização aplicada", update::VERSION);
        return Ok(());
    }
    if args.iter().any(|a| a == "--dump") {
        return dump();
    }
    if let Some(i) = args.iter().position(|a| a == "--stop-task") {
        let id = args.get(i + 1).ok_or_else(|| anyhow::anyhow!("uso: agent-dash --stop-task <id>"))?;
        return stop_task(id);
    }
    if let Some(i) = args.iter().position(|a| a == "--reset-console") {
        // Auxiliar interno: limpa o terminal de outro processo (ver actions::reset_console).
        let pid: u32 = args.get(i + 1).and_then(|p| p.parse().ok()).ok_or_else(|| anyhow::anyhow!("uso: --reset-console <pid>"))?;
        return actions::reset_console(pid).map_err(|e| anyhow::anyhow!(e));
    }
    if args.iter().any(|a| a == "--fix-terminal") {
        // Para rodar numa aba que ficou imprimindo lixo ao mexer o mouse.
        print!("{}", actions::TERM_RESET);
        println!("terminal restaurado");
        return Ok(());
    }
    if args.iter().any(|a| a == "--reset-test") {
        return reset_test();
    }
    if args.iter().any(|a| a == "--chat-test") {
        return chat_test();
    }
    if let Some(i) = args.iter().position(|a| a == "--frame") {
        // Depuração: renderiza cada painel em memória e imprime (não precisa de TTY).
        let size = args.get(i + 1).cloned().unwrap_or_else(|| "160x45".into());
        return frame(&size);
    }
    run_tui()
}

fn frame(size: &str) -> anyhow::Result<()> {
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let (w, h) = size.split_once('x').and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?))).unwrap_or((160, 45));
    let mut c = collector::Collector::new()?;
    c.refresh_bg_blocking();
    let mut snap = c.tick();
    while c.history_pending() {
        snap = c.tick();
    }
    let mut app = app::App::new();
    app.set_snapshot(snap);
    let (ctl_tx, _ctl_rx) = mpsc::channel();
    let mut term = ratatui::Terminal::new(TestBackend::new(w, h))?;
    let keys: Vec<(&str, Vec<KeyCode>)> = vec![
        ("sessões", vec![]),
        ("tasks (aba saída)", vec![KeyCode::Char('3')]),
        ("tasks (detalhes)", vec![KeyCode::Char(']')]),
        ("uso", vec![KeyCode::Char('4')]),
        ("agentes (tudo)", vec![KeyCode::Char('2'), KeyCode::Char('a')]),
        ("sessão > processos", vec![KeyCode::Char('1'), KeyCode::Char(']'), KeyCode::Char(']')]),
        ("ajuda", vec![KeyCode::Char('?')]),
        ("nova instância", vec![KeyCode::Esc, KeyCode::Char('['), KeyCode::Char('n'), KeyCode::Tab, KeyCode::Right, KeyCode::Tab, KeyCode::Tab, KeyCode::Char('t'), KeyCode::Char('e'), KeyCode::Char('s'), KeyCode::Char('t'), KeyCode::Char('e')]),
        ("confirmar encerrar", vec![KeyCode::Esc, KeyCode::Char('x')]),
        ("aba chat", vec![KeyCode::Esc, KeyCode::Char(']'), KeyCode::Char(']')]),
    ];
    // Mouse sintético: a roda sobre Sessões rola a lista sem mexer na seleção;
    // o clique dá foco e seleciona a linha clicada.
    {
        use ratatui::crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        term.draw(|f| ui::draw(f, &mut app))?;
        let ev = |kind, column, row| MouseEvent { kind, column, row, modifiers: KeyModifiers::NONE };
        let r = app.rects;
        eprintln!("painéis da esquerda (y, altura): {:?}", r.iter().map(|r| (r.y, r.height)).collect::<Vec<_>>());
        let sel0 = app.states[0].selected();
        app.on_mouse(ev(MouseEventKind::ScrollDown, r[0].x + 2, r[0].y + 1));
        eprintln!("roda em Sessões: offset {} (seleção {:?} -> {:?})", app.offsets[0], sel0, app.states[0].selected());
        app.on_mouse(ev(MouseEventKind::ScrollUp, r[0].x + 2, r[0].y + 1));
        app.on_mouse(ev(MouseEventKind::Down(MouseButton::Left), r[0].x + 2, r[0].y + 2));
        eprintln!("clique na 2ª linha de Sessões: foco {:?}, seleção {:?}, offset {}", app.focus, app.states[0].selected(), app.offsets[0]);
        app.on_mouse(ev(MouseEventKind::Down(MouseButton::Left), r[0].x + 2, r[0].y + 1));
        // A seleção tem de continuar à vista depois de `G` e de `a` (lista muda).
        let h = r[0].height.saturating_sub(2) as usize;
        for k in [KeyCode::Char('G'), KeyCode::Char('a'), KeyCode::Char('a'), KeyCode::Char('g')] {
            app.on_key(KeyEvent::new(k, KeyModifiers::NONE), &ctl_tx);
            term.draw(|f| ui::draw(f, &mut app))?;
            let (sel, off) = (app.states[0].selected().unwrap_or(0), app.offsets[0]);
            eprintln!("após {k:?}: seleção {sel}, offset {off}, altura {h} -> {}", if sel >= off && sel < off + h { "à vista" } else { "FORA DA VISTA" });
        }
    }
    // Aviso de "reabrir sessões" (só desenha; não responde, para não mexer na lista real).
    app.offer_restore();
    if app.confirm.is_some() {
        term.draw(|f| ui::draw(f, &mut app))?;
        println!("===== reabrir sessões =====");
        let buf = term.backend().buffer().clone();
        for y in 0..buf.area.height {
            let line: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol().to_string()).collect();
            println!("{}", line.trim_end());
        }
        app.confirm = None;
    }
    for (name, ks) in keys {
        for k in ks {
            app.on_key(KeyEvent::new(k, KeyModifiers::NONE), &ctl_tx);
        }
        let td = Instant::now();
        term.draw(|f| ui::draw(f, &mut app))?;
        eprintln!("desenho de '{name}': {:?}", td.elapsed());
        println!("===== {name} =====");
        let buf = term.backend().buffer().clone();
        for y in 0..buf.area.height {
            let line: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol().to_string()).collect();
            println!("{}", line.trim_end());
        }
    }
    Ok(())
}

fn run_tui() -> anyhow::Result<()> {
    use std::io::IsTerminal;
    if !std::io::stdout().is_terminal() {
        anyhow::bail!("o dashboard precisa de um terminal interativo; para saída em texto use `agent-dash --dump`");
    }
    // Versão mais nova publicada: baixa, troca o executável e relança.
    if let Some(code) = update::auto() {
        std::process::exit(code);
    }
    let (tx, rx) = mpsc::channel();
    let (ctl_tx, ctl_rx) = mpsc::channel();
    thread::Builder::new().name("collector".into()).spawn(move || collector::run(tx, ctl_rx))?;

    let mut terminal = ratatui::init();
    // Mouse: roda e clique nos painéis. Um panic também precisa soltar a
    // captura, senão a aba fica imprimindo códigos a cada movimento.
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(std::io::stdout(), DisableMouseCapture);
        hook(info);
    }));
    execute!(std::io::stdout(), EnableMouseCapture)?;
    let mut app = app::App::new();
    app.offer_restore();
    let result = (|| -> anyhow::Result<()> {
        // Só redesenha quando algo mudou (tecla, saída de um chat, snapshot novo)
        // ou a cada 250 ms, para relógios e avisos.
        let mut need_draw = true;
        let mut last_draw = Instant::now();
        loop {
            // Usa só o snapshot mais recente.
            let mut last = None;
            for s in rx.try_iter() {
                if let Some(n) = s.notices.last() {
                    app.flash(n.clone());
                }
                last = Some(s);
            }
            if let Some(s) = last {
                app.set_snapshot(s);
                need_draw = true;
            }
            let n_terms = app.terms.len();
            app.reap_terms();
            need_draw |= term::take_dirty() || app.terms.len() != n_terms;
            if need_draw || last_draw.elapsed() >= Duration::from_millis(250) {
                terminal.draw(|f| ui::draw(f, &mut app))?;
                need_draw = false;
                last_draw = Instant::now();
            }
            // Com chat aberto, acorda rápido para mostrar o que o Claude escreve.
            let wait = if app.terms.is_empty() { 100 } else { 3 };
            let mut keys = Vec::new();
            let mut mouse = Vec::new();
            if event::poll(Duration::from_millis(wait))? {
                loop {
                    match event::read()? {
                        Event::Key(k) if k.kind != KeyEventKind::Release => keys.push(k),
                        // Movimento chega aos montes e não muda nada: ignora sem redesenhar.
                        Event::Mouse(m) if !matches!(m.kind, MouseEventKind::Moved | MouseEventKind::Drag(_)) => mouse.push(m),
                        Event::Resize(..) => need_draw = true,
                        _ => {}
                    }
                    if !event::poll(Duration::ZERO)? {
                        break;
                    }
                }
            }
            need_draw |= !keys.is_empty();
            if app.chat_focus && keys.len() >= 8 {
                app.chat_paste(&keys);
            } else {
                for k in keys {
                    app.on_key(k, &ctl_tx);
                }
            }
            for m in mouse {
                need_draw |= app.on_mouse(m);
            }
            if app.quit {
                return Ok(());
            }
        }
    })();
    let _ = ctl_tx.send(collector::Ctl::Quit);
    let _ = execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}

/// Depuração: abre um `claude` num terminal embutido, manda uma mensagem e
/// imprime a tela (não precisa de TTY).
fn chat_test() -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?.display().to_string();
    let sid = term::new_uuid();
    let args: Vec<String> = ["--session-id", &sid, "--model", "haiku", "-n", "agent-dash-chat-test"].iter().map(|s| s.to_string()).collect();
    let mut t = term::Term::spawn(sid.clone(), "teste".into(), &cwd, &args, 30, 110).map_err(|e| anyhow::anyhow!(e))?;
    let show = |t: &term::Term, tag: &str| {
        println!("===== {tag} =====");
        for l in t.contents().lines().filter(|l| !l.trim().is_empty()) {
            println!("{l}");
        }
    };
    thread::sleep(Duration::from_secs(8));
    show(&t, "depois de abrir");
    // Atraso entre mandar uma tecla e ela aparecer na tela do Claude.
    for ch in ["a", "b", "c", "d", "e", "f"] {
        let before = t.contents();
        let t0 = Instant::now();
        t.send(ch.as_bytes());
        while t.contents() == before && t0.elapsed() < Duration::from_secs(2) {
            thread::sleep(Duration::from_millis(1));
        }
        println!("eco de '{ch}': {:?}", t0.elapsed());
        thread::sleep(Duration::from_millis(300));
    }
    t.send(&[0x15]); // Ctrl+U limpa a linha
    thread::sleep(Duration::from_millis(300));
    t.paste("responda apenas com a palavra: abacaxi");
    thread::sleep(Duration::from_millis(500));
    t.send(b"\r");
    thread::sleep(Duration::from_secs(15));
    show(&t, "depois da mensagem");
    t.resize(20, 80);
    thread::sleep(Duration::from_secs(2));
    show(&t, "depois de redimensionar para 80x20");
    {
        // O mesmo desenho que o painel faz, num terminal em memória.
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 20))?;
        let p = t.parser.lock().unwrap();
        term.draw(|f| f.render_widget(tui_term::widget::PseudoTerminal::new(p.screen()), f.area()))?;
        let buf = term.backend().buffer().clone();
        println!("===== desenhado pelo widget =====");
        for y in 0..buf.area.height {
            let line: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol().to_string()).collect();
            if !line.trim().is_empty() {
                println!("{}", line.trim_end());
            }
        }
    }
    println!("sessão: {sid} · processo terminou: {}", t.exited());
    t.send(b"/exit\r");
    thread::sleep(Duration::from_secs(3));
    println!("depois de /exit, terminou: {}", t.exited());
    Ok(())
}

/// Depuração: liga a captura de mouse num shell dentro de um pseudo-terminal e
/// confere se o auxiliar `--reset-console` a desliga.
fn reset_test() -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?.display().to_string();
    let mut t = term::Term::spawn_program("cmd.exe", "x".into(), "t".into(), &cwd, &[], 20, 80).map_err(|e| anyhow::anyhow!(e))?;
    thread::sleep(Duration::from_secs(1));
    let mode = |t: &term::Term| format!("{:?}", t.parser.lock().unwrap().screen().mouse_protocol_mode());
    println!("antes: {}", mode(&t));
    t.send(b"powershell -NoProfile -Command \"[Console]::Write([char]27+'[?1003h'+[char]27+'[?1006h')\"\r");
    thread::sleep(Duration::from_secs(4));
    println!("com captura ligada: {}", mode(&t));
    let pid = t.pid().ok_or_else(|| anyhow::anyhow!("sem pid"))?;
    unsafe { std::env::set_var("AGENT_DASH_RESET_MARK", "1") };
    actions::spawn_console_reset(pid);
    thread::sleep(Duration::from_secs(3));
    println!("depois do reset: {}", mode(&t));
    println!("tela: {:?}", t.contents().trim());
    Ok(())
}

/// Para uma task pelo id (procura em todas as sessões).
fn stop_task(id: &str) -> anyhow::Result<()> {
    let mut c = collector::Collector::new()?;
    let mut snap = c.tick();
    while c.history_pending() {
        snap = c.tick();
    }
    let Some(t) = snap.tasks.iter().find(|t| t.id == id) else {
        anyhow::bail!("task {id} não encontrada");
    };
    println!("{}", c.stop_task(&t.session_id.clone(), id));
    Ok(())
}

/// Modo sem TUI: roda o coletor até ler todo o histórico e imprime um resumo.
fn dump() -> anyhow::Result<()> {
    let t0 = Instant::now();
    let mut c = collector::Collector::new()?;
    c.refresh_bg_blocking();
    let mut snap = c.tick();
    let first = t0.elapsed();
    while c.history_pending() {
        snap = c.tick();
    }
    println!("primeiro tick: {:?} · histórico completo: {:?}", first, t0.elapsed());
    let t1 = Instant::now();
    for _ in 0..5 {
        snap = c.tick();
    }
    println!("tick em regime: {:?}", t1.elapsed() / 5);
    println!("histórico: {} / {} bytes", snap.history_done, snap.history_total);
    println!("\n== Sessões ({} vivas)", snap.sessions.iter().filter(|s| s.alive).count());
    for s in snap.sessions.iter().filter(|s| s.alive) {
        println!(
            "{:<16} {:<9} pid={:<6} turns={:<4} total={:<12} ctx={:<8} agentes={}/{} tasks={} procs={} cpu={:.1} mem={}MB  {}",
            s.name,
            s.status,
            s.pid.unwrap_or(0),
            s.usage.turns,
            s.usage.tokens.total(),
            s.usage.last_turn.context(),
            s.agents_running,
            s.agents_total,
            s.tasks_running,
            s.procs.len(),
            s.cpu,
            s.mem / 1_048_576,
            s.title.clone().unwrap_or_default()
        );
        if let Some(id) = &s.bg_id {
            println!("    background id={id} cwd={}", s.cwd);
        }
        for p in &s.procs {
            println!("    {}{} {} {}", "  ".repeat(p.depth), p.pid, p.name, p.cmd.chars().take(80).collect::<String>());
        }
    }
    println!("\n== Tasks (10 primeiras de {})", snap.tasks.len());
    for t in snap.tasks.iter().take(10) {
        println!("{:<10} {:<8} {:?} [{}] {} out={:?}", t.id, t.kind.label(), t.status, t.session_name, t.description, t.output_file.is_some());
    }
    println!("\n== Agentes (8 primeiros de {})", snap.agents.len());
    for a in snap.agents.iter().take(8) {
        println!("{} {:<16} {:<12} tok={} [{}] {}", if a.running { "●" } else { "○" }, a.agent_id, a.agent_type, a.usage.tokens.total(), a.session_name, a.description);
    }
    println!("\n== Últimos 10 dias");
    for (d, t) in snap.daily.iter().rev().take(10) {
        println!("{d}  total={:<14} out={:<10} sessões={}", t.total(), t.output, snap.daily_sessions.get(d).unwrap_or(&0));
    }
    Ok(())
}
