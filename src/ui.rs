//! Renderização (ratatui). Layout no estilo lazygit: painéis numerados à
//! esquerda, painel principal com abas à direita, atalhos no rodapé.

use crate::app::{App, Panel};
use crate::model::*;
use chrono::{DateTime, Local};
use ratatui::layout::{Alignment, Constraint, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Bar, BarChart, BarGroup, Block, BorderType, Cell, Clear, List, ListItem, ListState, Paragraph, Row, Scrollbar,
    ScrollbarOrientation, ScrollbarState, Sparkline, Table, Wrap,
};
use ratatui::Frame;

const FOCUS: Color = Color::Green;
const DIM: Color = Color::DarkGray;
const ACCENT: Color = Color::Cyan;
const SEL_BG: Color = Color::Rgb(45, 50, 62);

// ---------- formatação ----------

pub fn fmt_tok(n: u64) -> String {
    let f = n as f64;
    if n < 1_000 {
        format!("{n}")
    } else if n < 1_000_000 {
        format!("{:.1}k", f / 1e3)
    } else if n < 1_000_000_000 {
        format!("{:.1}M", f / 1e6)
    } else {
        format!("{:.2}B", f / 1e9)
    }
}

fn fmt_dur(d: chrono::Duration) -> String {
    let s = d.num_seconds().max(0);
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else if s < 86400 {
        format!("{}h{:02}m", s / 3600, (s % 3600) / 60)
    } else {
        format!("{}d{}h", s / 86400, (s % 86400) / 3600)
    }
}

fn fmt_ago(t: Option<DateTime<Local>>) -> String {
    t.map(|t| fmt_dur(Local::now() - t)).unwrap_or_else(|| "-".into())
}

fn fmt_time(t: Option<DateTime<Local>>) -> String {
    t.map(|t| {
        if t.date_naive() == Local::now().date_naive() {
            t.format("%H:%M:%S").to_string()
        } else {
            t.format("%d/%m %H:%M").to_string()
        }
    })
    .unwrap_or_else(|| "--:--:--".into())
}

fn fmt_mb(b: u64) -> String {
    format!("{:.0} MB", b as f64 / 1_048_576.0)
}

/// Corta/preenche para `w` colunas (conta caracteres).
fn fit(s: &str, w: usize) -> String {
    let n = s.chars().count();
    if n <= w {
        format!("{s:<w$}")
    } else if w == 0 {
        String::new()
    } else {
        let mut out: String = s.chars().take(w - 1).collect();
        out.push('…');
        out
    }
}

fn weekday_pt(d: chrono::NaiveDate, long: bool) -> &'static str {
    use chrono::Datelike;
    let i = d.weekday().num_days_from_monday() as usize;
    if long {
        ["Segunda", "Terça", "Quarta", "Quinta", "Sexta", "Sábado", "Domingo"][i]
    } else {
        ["seg", "ter", "qua", "qui", "sex", "sáb", "dom"][i]
    }
}

fn spark_text(data: &[u64]) -> String {
    const B: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let max = data.iter().copied().max().unwrap_or(0);
    data.iter()
        .map(|&v| if max == 0 || v == 0 { ' ' } else { B[((v as f64 / max as f64) * 7.0).round() as usize] })
        .collect()
}

fn minutes_series(u: &Usage) -> Vec<u64> {
    let now_min = Local::now().timestamp().div_euclid(60);
    (0..60).map(|i| *u.per_minute.get(&(now_min - 59 + i)).unwrap_or(&0)).collect()
}

fn session_dot(s: &SessionInfo) -> Span<'static> {
    if s.status == "pendente" {
        return Span::styled("◐", Style::new().fg(Color::Magenta));
    }
    if !s.alive {
        return Span::styled("○", Style::new().fg(DIM));
    }
    match s.status.as_str() {
        "busy" | "working" | "running" => Span::styled("●", Style::new().fg(Color::Yellow)),
        "shell" => Span::styled("●", Style::new().fg(ACCENT)),
        "idle" => Span::styled("●", Style::new().fg(Color::Green)),
        "blocked" => Span::styled("●", Style::new().fg(Color::Magenta)),
        _ => Span::styled("●", Style::new().fg(Color::White)),
    }
}

fn status_label(s: &SessionInfo) -> &str {
    match s.status.as_str() {
        "busy" => "ocupada",
        "idle" => "ociosa",
        "shell" => "shell",
        "blocked" => "aguarda",
        "pendente" => "espera",
        "running" | "working" => "rodando",
        other => other,
    }
}

fn task_style(t: &TaskInfo) -> (&'static str, Style) {
    match &t.status {
        TaskStatus::Running => ("●", Style::new().fg(Color::Yellow)),
        TaskStatus::Orphan => ("?", Style::new().fg(Color::Magenta)),
        TaskStatus::Done(s) if s == "completed" => ("✓", Style::new().fg(Color::Green)),
        TaskStatus::Done(s) if s == "expirado" => ("◌", Style::new().fg(DIM)),
        TaskStatus::Done(_) => ("✗", Style::new().fg(Color::Red)),
    }
}

fn task_status_text(t: &TaskInfo) -> String {
    match &t.status {
        TaskStatus::Running => "rodando".into(),
        TaskStatus::Orphan => "órfã (sessão encerrada)".into(),
        TaskStatus::Done(s) => match s.as_str() {
            "completed" => "concluída".into(),
            "killed" | "stopped" => "interrompida".into(),
            "failed" => "falhou".into(),
            other => other.into(),
        },
    }
}

fn panel_block(title: String, focused: bool) -> Block<'static> {
    let style = if focused { Style::new().fg(FOCUS).bold() } else { Style::new().fg(DIM) };
    Block::bordered()
        .border_type(if focused { BorderType::Thick } else { BorderType::Rounded })
        .border_style(style)
        .title(Span::styled(title, if focused { Style::new().fg(FOCUS).bold() } else { Style::new().fg(Color::Gray) }))
}

fn kv(k: &str, v: impl Into<String>) -> Line<'static> {
    Line::from(vec![Span::styled(format!("{k:<14}"), Style::new().fg(DIM)), Span::raw(v.into())])
}

// ---------- layout ----------

pub fn draw(f: &mut Frame, app: &mut App) {
    let [header, body, footer] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0), Constraint::Length(1)]).areas(f.area());
    draw_header(f, header, app);
    if app.zoom {
        app.rects = [Rect::default(); 4];
        app.main_rect = body;
        draw_main(f, body, app);
    } else {
        let [left, right] = Layout::horizontal([Constraint::Percentage(27), Constraint::Percentage(73)]).areas(body);
        // Alturas fixas: dependem só da altura da tela, nunca do número de itens.
        // Lista maior que o painel rola (roda do mouse ou seleção).
        let h = left.height;
        let [a, b, c, d] = Layout::vertical([
            Constraint::Min(5),
            Constraint::Length((h * 18 / 100).max(4)),
            Constraint::Length((h * 20 / 100).max(4)),
            Constraint::Length((h * 24 / 100).max(8)),
        ])
        .areas(left);
        app.rects = [a, b, c, d];
        app.main_rect = right;
        draw_sessions(f, a, app);
        draw_agents(f, b, app);
        draw_tasks(f, c, app);
        draw_days(f, d, app);
        draw_main(f, right, app);
    }
    draw_footer(f, footer, app);
    if app.help {
        draw_help(f);
    }
    if app.launcher.is_some() {
        draw_launcher(f, app);
    }
    if app.confirm.is_some() {
        draw_confirm(f, app);
    }
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width.saturating_sub(2));
    let h = h.min(area.height.saturating_sub(2));
    Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h }
}

fn draw_confirm(f: &mut Frame, app: &App) {
    let Some(c) = &app.confirm else { return };
    let w = 76u16;
    let mut lines = vec![Line::raw("")];
    for b in &c.body {
        let st = if b.starts_with('⚠') { Style::new().fg(Color::Yellow) } else { Style::new() };
        lines.push(Line::from(Span::styled(format!(" {b}"), st)));
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        Span::raw(" "),
        Span::styled(" y ", Style::new().fg(Color::Black).bg(Color::Red).bold()),
        Span::raw(" confirmar    "),
        Span::styled(" n ", Style::new().fg(Color::Black).bg(DIM).bold()),
        Span::raw(" cancelar"),
    ]));
    let p = Paragraph::new(lines).wrap(Wrap { trim: false });
    let wrapped: usize = c.body.iter().map(|b| b.chars().count() / (w as usize - 4) + 1).sum();
    let h = (wrapped as u16 + 6).max(6);
    let r = centered(f.area(), w, h);
    f.render_widget(Clear, r);
    f.render_widget(
        p.block(
            Block::bordered()
                .border_type(BorderType::Thick)
                .border_style(Style::new().fg(Color::Red))
                .title(Span::styled(format!(" {}? ", c.title), Style::new().fg(Color::Red).bold())),
        ),
        r,
    );
}

fn draw_launcher(f: &mut Frame, app: &App) {
    let Some(form) = &app.launcher else { return };
    let r = centered(f.area(), 84, 24);
    f.render_widget(Clear, r);
    let block = Block::bordered()
        .border_type(BorderType::Thick)
        .border_style(Style::new().fg(FOCUS))
        .title(Span::styled(" Nova instância do Claude Code ", Style::new().fg(FOCUS).bold()))
        .title_alignment(Alignment::Center);
    let inner = block.inner(r);
    f.render_widget(block, r);
    let [fields, list, foot] = Layout::vertical([Constraint::Length(6), Constraint::Min(3), Constraint::Length(2)]).areas(inner);

    let label = |i: usize, name: &str| {
        let st = if form.field == i { Style::new().fg(Color::Black).bg(ACCENT).bold() } else { Style::new().fg(DIM) };
        Span::styled(format!(" {name:<12}"), st)
    };
    let cursor = |i: usize| if form.field == i { Span::styled("▏", Style::new().fg(ACCENT)) } else { Span::raw("") };
    let choice = |opts: &[&str], sel: usize| -> Vec<Span<'static>> {
        let mut v = vec![Span::raw(" ")];
        for (i, o) in opts.iter().enumerate() {
            let st = if i == sel { Style::new().fg(Color::Black).bg(Color::Magenta).bold() } else { Style::new().fg(DIM) };
            v.push(Span::styled(format!(" {o} "), st));
            v.push(Span::raw(" "));
        }
        v
    };
    let mut model = vec![label(1, "Modelo")];
    model.extend(choice(&crate::actions::MODELS, form.model));
    let mut mode = vec![label(2, "Modo")];
    mode.extend(choice(&["aqui no painel", "aba nova do terminal", "background"], form.mode));
    let lines = vec![
        Line::from(vec![label(0, "Repositório"), Span::raw(" "), Span::raw(form.repo.clone()), cursor(0)]),
        Line::from(model),
        Line::from(mode),
        Line::from(vec![
            label(3, "Nome"),
            Span::raw(" "),
            Span::raw(form.name.clone()),
            cursor(3),
            Span::styled(if form.name.is_empty() && form.field != 3 { "(opcional)" } else { "" }, Style::new().fg(DIM).italic()),
        ]),
        Line::from(vec![
            label(4, "Prompt"),
            Span::raw(" "),
            Span::raw(form.prompt.clone()),
            cursor(4),
            Span::styled(
                if form.prompt.is_empty() && form.field != 4 { "(opcional: primeira mensagem)" } else { "" },
                Style::new().fg(DIM).italic(),
            ),
        ]),
    ];
    f.render_widget(Paragraph::new(lines), fields);

    let sug = form.suggestions(&app.snap.repos);
    let sel = form.sug.min(sug.len().saturating_sub(1));
    let h = list.height.saturating_sub(2) as usize;
    let start = sel.saturating_sub(h.saturating_sub(1));
    let items: Vec<ListItem> = sug
        .iter()
        .enumerate()
        .skip(start)
        .take(h)
        .map(|(i, r)| {
            let st = if i == sel { Style::new().bg(SEL_BG).bold() } else { Style::new() };
            let live = app.snap.sessions.iter().filter(|s| s.alive && s.cwd.eq_ignore_ascii_case(r)).count();
            let mut spans = vec![Span::styled(format!(" {} ", if i == sel { "›" } else { " " }), Style::new().fg(ACCENT)), Span::styled(r.to_string(), st)];
            if live > 0 {
                spans.push(Span::styled(format!("  ● {live} aberta{}", if live > 1 { "s" } else { "" }), Style::new().fg(Color::Green)));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();
    let title = if sug.is_empty() {
        " Nenhum repositório conhecido combina; Enter usa o caminho digitado se existir ".to_string()
    } else {
        format!(" Repositórios ({}) · ↑↓ escolhe ", sug.len())
    };
    f.render_widget(
        List::new(items).block(Block::bordered().border_type(BorderType::Rounded).border_style(Style::new().fg(DIM)).title(title)),
        list,
    );

    let spec = crate::actions::LaunchSpec {
        cwd: String::new(),
        model: if form.model == 0 { None } else { Some(crate::actions::MODELS[form.model].to_string()) },
        name: Some(form.name.clone()),
        prompt: Some(form.prompt.clone()),
        background: form.mode == 2,
    };
    let cmd = format!("claude {}", crate::actions::launch_args(&spec).join(" "));
    let foot_lines = vec![
        match &form.error {
            Some(e) => Line::from(Span::styled(format!(" {e}"), Style::new().fg(Color::Red))),
            None => Line::from(vec![
                Span::styled(" em ", Style::new().fg(DIM)),
                Span::styled(form.chosen(&app.snap.repos).unwrap_or_else(|| "?".into()), Style::new().fg(Color::White)),
            ]),
        },
        Line::from(vec![Span::styled(" $ ", Style::new().fg(DIM)), Span::styled(cmd, Style::new().fg(ACCENT))]),
    ];
    f.render_widget(Paragraph::new(foot_lines), foot);
}

fn draw_header(f: &mut Frame, area: Rect, app: &App) {
    let s = &app.snap;
    let alive = s.sessions.iter().filter(|x| x.alive).count();
    let busy = s.sessions.iter().filter(|x| x.alive && !matches!(x.status.as_str(), "idle" | "blocked")).count();
    let agents = s.agents.iter().filter(|a| a.running).count();
    let shells = s.tasks.iter().filter(|t| t.status == TaskStatus::Running && t.kind == TaskKind::Shell).count();
    let monitors = s.tasks.iter().filter(|t| t.status == TaskStatus::Running && t.kind == TaskKind::Monitor).count();
    let today = Local::now().date_naive();
    let today_tok = s.daily.get(&today).map(|t| t.total()).unwrap_or(0);
    let sep = Span::styled(" │ ", Style::new().fg(DIM));
    let mut spans = vec![
        Span::styled(" agent-dash ", Style::new().fg(Color::Black).bg(FOCUS).bold()),
        Span::raw(" "),
        Span::styled(format!("{alive}"), Style::new().fg(Color::Green).bold()),
        Span::raw(" sessões"),
        Span::styled(format!(" ({busy} trabalhando)"), Style::new().fg(Color::Yellow)),
        sep.clone(),
        Span::styled(format!("{agents}"), Style::new().fg(Color::Yellow).bold()),
        Span::raw(" agentes"),
        sep.clone(),
        Span::styled(format!("{shells}"), Style::new().fg(ACCENT).bold()),
        Span::raw(" shells "),
        Span::styled(format!("{monitors}"), Style::new().fg(ACCENT).bold()),
        Span::raw(" monitors"),
        sep.clone(),
        Span::raw("hoje "),
        Span::styled(fmt_tok(today_tok), Style::new().fg(Color::Magenta).bold()),
        Span::raw(" tok "),
        Span::styled(spark_text(&s.per_minute[s.per_minute.len().saturating_sub(30)..]), Style::new().fg(Color::Magenta)),
    ];
    if !s.loaded {
        spans.push(sep.clone());
        spans.push(Span::styled("carregando…", Style::new().fg(Color::Yellow)));
    } else if s.history_total > 0 && s.history_done < s.history_total {
        spans.push(sep.clone());
        spans.push(Span::styled(
            format!("lendo histórico {:.0}%", s.history_done as f64 * 100.0 / s.history_total as f64),
            Style::new().fg(Color::Yellow),
        ));
    }
    f.render_widget(Line::from(spans), area);
}

fn draw_footer(f: &mut Frame, area: Rect, app: &App) {
    if let Some(msg) = app.status_text() {
        f.render_widget(Line::from(Span::styled(format!(" {msg}"), Style::new().fg(Color::Yellow))), area);
        return;
    }
    if app.chat_focus {
        let mut spans = vec![Span::styled(" CHAT ", Style::new().fg(Color::Black).bg(FOCUS).bold()), Span::raw(" ")];
        spans.extend(key_line(&[("F12", "volta ao painel"), ("Shift+PgUp/PgDn", "rolar"), ("Ctrl+Enter", "nova linha")]).spans);
        spans.push(Span::styled("o teclado está indo para o Claude", Style::new().fg(DIM).italic()));
        f.render_widget(Line::from(spans), area);
        return;
    }
    if app.launcher.is_some() {
        let keys: &[(&str, &str)] = &[
            ("Tab ↑↓", "campo"),
            ("↑↓", "repositório"),
            ("← →", "modelo/modo"),
            ("Ctrl+U", "limpar campo"),
            ("Enter", "iniciar"),
            ("Esc", "cancelar"),
        ];
        f.render_widget(key_line(keys), area);
        return;
    }
    if app.confirm.is_some() {
        f.render_widget(key_line(&[("y Enter", "confirmar"), ("n Esc", "cancelar")]), area);
        return;
    }
    let ctx: &[(&str, &str)] = match app.focus {
        Panel::Sessions => &[("Enter", "chat"), ("n", "nova"), ("o", "aba nova"), ("x", "encerrar"), ("d", "limpar"), ("e", "pasta"), ("c", "copiar")],
        Panel::Agents => &[("n", "nova"), ("d/D", "limpar"), ("e", "pasta"), ("c", "copiar id")],
        Panel::Tasks => &[("x", "parar"), ("d/D", "limpar"), ("o", "saída"), ("c", "caminho"), ("J/K", "rolar")],
        Panel::Usage => &[("n", "nova"), ("c", "copiar")],
    };
    let common: &[(&str, &str)] = &[
        ("1-4", "painel"),
        ("↑↓", "mover"),
        ("[ ]", "aba"),
        ("z", "zoom"),
        ("a", if app.show_all { "só ativos" } else { "tudo" }),
        ("f", if app.only_selected { "todas" } else { "filtrar" }),
    ];
    let tail: &[(&str, &str)] = &[("?", "ajuda"), ("q", "sair")];
    let mut spans = vec![Span::raw(" ")];
    for (k, d) in common.iter().chain(ctx.iter()).chain(tail.iter()) {
        spans.push(Span::styled(*k, Style::new().fg(ACCENT).bold()));
        spans.push(Span::styled(format!(" {d}  "), Style::new().fg(DIM)));
    }
    f.render_widget(Line::from(spans), area);
}

fn key_line(keys: &[(&str, &str)]) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    for (k, d) in keys {
        spans.push(Span::styled(k.to_string(), Style::new().fg(ACCENT).bold()));
        spans.push(Span::styled(format!(" {d}  "), Style::new().fg(DIM)));
    }
    Line::from(spans)
}

/// Lista de um painel da esquerda. A janela visível é `app.offsets`, não a do
/// `ListState` (que sempre puxa a rolagem de volta para o item selecionado):
/// assim a roda do mouse rola sem mexer na seleção.
fn draw_list(f: &mut Frame, area: Rect, app: &mut App, p: Panel, items: Vec<ListItem>, title: String) {
    let focused = app.focus == p;
    let i = p.idx();
    let h = area.height.saturating_sub(2) as usize;
    let n = items.len();
    let max_off = n.saturating_sub(h);
    let mut off = app.offsets[i];
    let cur = app.states[i].selected();
    // Seleção mudou (ou a lista reordenou com ela à vista): rola o mínimo para mostrá-la.
    if std::mem::take(&mut app.reveal[i])
        && let Some(s) = cur
        && h > 0
    {
        if s < off {
            off = s;
        } else if s >= off + h {
            off = s + 1 - h;
        }
    }
    let off = off.min(max_off);
    app.offsets[i] = off;
    let sel = cur.filter(|s| *s >= off && *s < off + h).map(|s| s - off);
    app.sel_visible[i] = sel.is_some() || cur.is_none();
    let visible: Vec<ListItem> = items.into_iter().skip(off).take(h).collect();
    let hl = if focused { Style::new().bg(SEL_BG).add_modifier(Modifier::BOLD) } else { Style::new().bg(Color::Rgb(35, 38, 46)) };
    let list = List::new(visible).block(panel_block(title, focused)).highlight_style(hl);
    f.render_stateful_widget(list, area, &mut ListState::default().with_selected(sel));
    if (focused || app.wheel == Some(p)) && max_off > 0 {
        let bar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .track_symbol(Some("│"))
            .thumb_symbol("█")
            .style(Style::new().fg(if focused { FOCUS } else { Color::Gray }));
        let mut st = ScrollbarState::new(max_off + 1).viewport_content_length(h).position(off);
        f.render_stateful_widget(bar, area.inner(Margin { vertical: 1, horizontal: 0 }), &mut st);
    }
}

fn empty_hint(f: &mut Frame, area: Rect, text: &str) {
    let inner = Rect { x: area.x + 2, y: area.y + 1, width: area.width.saturating_sub(4), height: 1 };
    f.render_widget(Paragraph::new(Span::styled(text.to_string(), Style::new().fg(DIM).italic())), inner);
}

fn draw_sessions(f: &mut Frame, area: Rect, app: &mut App) {
    let chats: Vec<String> = app.terms.iter().map(|t| t.key.clone()).collect();
    let list = app.sessions();
    let alive = app.snap.sessions.iter().filter(|s| s.alive).count();
    let w = area.width.saturating_sub(4) as usize;
    let name_w = w.saturating_sub(26).clamp(8, 30);
    let items: Vec<ListItem> = list
        .iter()
        .map(|s| {
            let mut extra = String::new();
            if chats.contains(&s.session_id) {
                extra.push_str(" ▣");
            }
            if s.agents_running > 0 {
                extra.push_str(&format!(" ⚑{}", s.agents_running));
            }
            if s.tasks_running > 0 {
                extra.push_str(&format!(" ⚙{}", s.tasks_running));
            }
            ListItem::new(Line::from(vec![
                session_dot(s),
                Span::raw(" "),
                Span::styled(fit(&s.name, name_w), if s.alive { Style::new() } else { Style::new().fg(DIM) }),
                Span::styled(format!(" {:<7}", fit(status_label(s), 7)), Style::new().fg(DIM)),
                Span::styled(format!("{:>7}", fmt_tok(s.usage.tokens.total() + s.agents_usage.total())), Style::new().fg(Color::Magenta)),
                Span::styled(format!(" {:>5}", fmt_ago(s.usage.last_ts.or(s.started_at))), Style::new().fg(DIM)),
                Span::styled(extra, Style::new().fg(Color::Yellow)),
            ]))
        })
        .collect();
    let n = items.len();
    let title = if app.show_all { format!("[1] Sessões ({alive} vivas, {n} em 24h)") } else { format!("[1] Sessões ({alive})") };
    draw_list(f, area, app, Panel::Sessions, items, title);
    if n == 0 {
        empty_hint(f, area, "nenhuma sessão do Claude Code rodando");
    }
}

fn draw_agents(f: &mut Frame, area: Rect, app: &mut App) {
    let list = app.agents();
    let running = list.iter().filter(|a| a.running).count();
    let w = area.width.saturating_sub(4) as usize;
    let items: Vec<ListItem> = list
        .iter()
        .map(|a| {
            let dot = if a.running { Span::styled("●", Style::new().fg(Color::Yellow)) } else { Span::styled("✓", Style::new().fg(DIM)) };
            let typ = fit(&a.agent_type, 14);
            let tok = format!("{:>7}", fmt_tok(a.usage.tokens.total()));
            let rest = w.saturating_sub(2 + 15 + 8 + 1);
            let desc = format!("{} · {}", a.description, a.session_name);
            ListItem::new(Line::from(vec![
                dot,
                Span::raw(" "),
                Span::styled(typ, Style::new().fg(ACCENT)),
                Span::raw(" "),
                Span::styled(fit(&desc, rest), if a.running { Style::new() } else { Style::new().fg(DIM) }),
                Span::styled(tok, Style::new().fg(Color::Magenta)),
            ]))
        })
        .collect();
    let n = items.len();
    let scope = if app.only_selected { " · sessão" } else { "" };
    let title = format!("[2] Agentes ({running} rodando{scope})");
    draw_list(f, area, app, Panel::Agents, items, title);
    if n == 0 {
        empty_hint(f, area, "nenhum subagente ativo");
    }
}

fn draw_tasks(f: &mut Frame, area: Rect, app: &mut App) {
    let list = app.tasks();
    let running = list.iter().filter(|t| t.status == TaskStatus::Running).count();
    let w = area.width.saturating_sub(4) as usize;
    let items: Vec<ListItem> = list
        .iter()
        .map(|t| {
            let (icon, st) = task_style(t);
            let rest = w.saturating_sub(2 + 9 + 6);
            let desc = format!("{} · {}", t.description, t.session_name);
            let age = match &t.status {
                TaskStatus::Running => fmt_ago(t.started),
                _ => fmt_ago(t.ended.or(t.started)),
            };
            ListItem::new(Line::from(vec![
                Span::styled(icon, st),
                Span::raw(" "),
                Span::styled(format!("{:<8}", t.kind.label()), Style::new().fg(ACCENT)),
                Span::raw(" "),
                Span::styled(fit(&desc, rest), if t.status == TaskStatus::Running { Style::new() } else { Style::new().fg(DIM) }),
                Span::styled(format!("{age:>5}"), Style::new().fg(DIM)),
            ]))
        })
        .collect();
    let n = items.len();
    let scope = if app.only_selected { " · sessão" } else { "" };
    let title = format!("[3] Shells/Monitors/Tasks ({running} rodando{scope})");
    draw_list(f, area, app, Panel::Tasks, items, title);
    if n == 0 {
        empty_hint(f, area, "nada em background");
    }
}

fn draw_days(f: &mut Frame, area: Rect, app: &mut App) {
    let focused = app.focus == Panel::Usage;
    let days = app.days(); // mais recente primeiro
    let block = panel_block("[4] Uso por dia".to_string(), focused);
    let inner = block.inner(area);
    let bar_w: u16 = 6;
    let n = ((inner.width + 1) / (bar_w + 1)).max(1) as usize;
    // Janela de dias que mantém o selecionado visível; mais antigo à esquerda.
    let sel = app.states[3].selected().unwrap_or(0);
    let start = (sel + 1).saturating_sub(n);
    let bars: Vec<Bar> = days
        .iter()
        .enumerate()
        .skip(start)
        .take(n)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(|(i, (d, t))| {
            let on = i == sel;
            let color = if on && focused { Color::Yellow } else if on { Color::LightMagenta } else { Color::Magenta };
            Bar::default()
                .value(t.total())
                .text_value(fmt_tok(t.total()))
                .label(Line::from(Span::styled(
                    d.format("%d/%m").to_string(),
                    if on { Style::new().fg(Color::White).bold() } else { Style::new().fg(DIM) },
                )))
                .style(Style::new().fg(color))
                .value_style(Style::new().fg(Color::Black).bg(color))
        })
        .collect();
    let bc = BarChart::default().block(block).data(BarGroup::default().bars(&bars)).bar_width(bar_w).bar_gap(1);
    f.render_widget(bc, area);
}

// ---------- painel principal ----------

fn main_block(app: &App, title: String) -> Block<'static> {
    let mut spans = vec![Span::styled(format!(" {title} "), Style::new().fg(Color::White).bold())];
    let tabs = app.focus.tabs();
    if tabs.len() > 1 {
        spans.push(Span::styled("─", Style::new().fg(DIM)));
        for (i, t) in tabs.iter().enumerate() {
            let st = if i == app.tab { Style::new().fg(Color::Black).bg(ACCENT).bold() } else { Style::new().fg(DIM) };
            spans.push(Span::styled(format!(" {t} "), st));
        }
    }
    Block::bordered().border_type(BorderType::Rounded).border_style(Style::new().fg(DIM)).title(Line::from(spans))
}

/// Desenha o chat embutido da sessão `key` (ou uma dica, se não houver).
fn draw_chat(f: &mut Frame, area: Rect, app: &mut App, key: Option<String>) {
    let idx = key.as_ref().and_then(|k| app.term_idx(k));
    let focused = app.chat_focus;
    let title = match idx {
        Some(i) => format!("Chat {}", app.terms[i].title),
        None => "Chat".into(),
    };
    let mut block = main_block(app, title);
    if focused {
        block = block.border_type(BorderType::Thick).border_style(Style::new().fg(FOCUS));
    }
    let inner = block.inner(area);
    f.render_widget(block, area);
    let [inner, meter] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);
    app.chat_size = (inner.height, inner.width);
    let sess = key.as_ref().and_then(|k| app.snap.sessions.iter().find(|s| &s.session_id == k));
    let sep = Span::styled("  │  ", Style::new().fg(DIM));
    let lab = |t: &str| Span::styled(t.to_string(), Style::new().fg(DIM));
    let line = match sess.filter(|s| s.usage.turns > 0) {
        Some(s) => {
            let u = &s.usage;
            let mins = minutes_series(u);
            let last5: u64 = mins[mins.len() - 5..].iter().sum::<u64>() / 5;
            let mut v = vec![
                lab(" contexto "),
                Span::styled(fmt_tok(u.last_turn.context()), Style::new().fg(Color::Yellow).bold()),
                sep.clone(),
                lab("sessão "),
                Span::styled(fmt_tok(u.tokens.total()), Style::new().fg(Color::Magenta).bold()),
                lab(" tok"),
                lab(&format!("  (saída {} · cache lido {})", fmt_tok(u.tokens.output), fmt_tok(u.tokens.cache_read))),
            ];
            if s.agents_total > 0 {
                v.push(lab(" + subagentes "));
                v.push(Span::styled(fmt_tok(s.agents_usage.total()), Style::new().fg(Color::Magenta)));
            }
            v.extend([
                sep.clone(),
                Span::styled(format!("{}/min", fmt_tok(last5)), Style::new().fg(ACCENT)),
                lab(" "),
                Span::styled(spark_text(&mins[mins.len() - 20..]), Style::new().fg(ACCENT)),
                sep.clone(),
                lab(&format!("{} respostas", u.turns)),
                sep.clone(),
                lab(u.model.as_deref().unwrap_or("-")),
            ]);
            Line::from(v)
        }
        None => Line::from(lab(" contexto e tokens aparecem depois da primeira resposta")),
    };
    f.render_widget(line.style(Style::new().bg(Color::Rgb(22, 26, 36))), meter);
    let Some(i) = idx else {
        let lines = vec![
            Line::raw(""),
            Line::from(Span::styled("  Esta sessão não tem chat aberto aqui dentro.", Style::new().fg(DIM))),
            Line::raw(""),
            Line::from(vec![Span::styled("  Enter ", Style::new().fg(ACCENT).bold()), Span::raw("abre o chat aqui (retoma uma sessão encerrada ou em background)")]),
            Line::from(vec![Span::styled("  n     ", Style::new().fg(ACCENT).bold()), Span::raw("nova instância dentro do painel")]),
            Line::from(vec![Span::styled("  F12   ", Style::new().fg(ACCENT).bold()), Span::raw("devolve o teclado ao painel quando estiver no chat")]),
        ];
        f.render_widget(Paragraph::new(lines), inner);
        return;
    };
    let t = &mut app.terms[i];
    t.resize(inner.height, inner.width);
    let scroll = t.scroll;
    let mut p = t.parser.lock().unwrap();
    p.set_scrollback(scroll);
    let shown = p.screen().scrollback();
    f.render_widget(tui_term::widget::PseudoTerminal::new(p.screen()), inner);
    p.set_scrollback(0);
    drop(p);
    if shown > 0 {
        let tag = format!(" ↑ {shown} linhas acima do fim ");
        let w = tag.chars().count() as u16;
        let r = Rect { x: area.right().saturating_sub(w + 2), y: area.bottom() - 1, width: w, height: 1 };
        f.render_widget(Span::styled(tag, Style::new().fg(Color::Black).bg(Color::Yellow)), r);
    }
}

fn draw_main(f: &mut Frame, area: Rect, app: &mut App) {
    if app.chat_focus {
        let key = app.chat_key.clone();
        draw_chat(f, area, app, key);
        return;
    }
    if app.focus == Panel::Sessions && app.tab == 3 {
        let key = app.selected_session().map(|s| s.session_id.clone());
        draw_chat(f, area, app, key);
        return;
    }
    match app.focus {
        Panel::Sessions => draw_session_main(f, area, app),
        Panel::Agents => draw_agent_main(f, area, app),
        Panel::Tasks => draw_task_main(f, area, app),
        Panel::Usage => draw_usage_main(f, area, app),
    }
}

fn token_rows(u: &Usage, extra: Option<(&str, Tokens)>) -> Vec<Row<'static>> {
    let t = &u.tokens;
    let mut rows = vec![
        Row::new(vec![Cell::from("Entrada"), Cell::from(fmt_tok(t.input))]),
        Row::new(vec![Cell::from("Cache escrita"), Cell::from(fmt_tok(t.cache_write))]),
        Row::new(vec![Cell::from("Cache leitura"), Cell::from(fmt_tok(t.cache_read))]),
        Row::new(vec![Cell::from("Saída"), Cell::from(fmt_tok(t.output))]),
        Row::new(vec![Cell::from("Total").bold(), Cell::from(fmt_tok(t.total())).fg(Color::Magenta).bold()]),
    ];
    if let Some((label, x)) = extra {
        rows.push(Row::new(vec![Cell::from(label.to_string()), Cell::from(fmt_tok(x.total())).fg(Color::Magenta)]));
    }
    rows.push(Row::new(vec![Cell::from("Contexto atual"), Cell::from(fmt_tok(u.last_turn.context())).fg(Color::Yellow)]));
    rows.push(Row::new(vec![Cell::from("Respostas"), Cell::from(u.turns.to_string())]));
    rows
}

fn tools_lines(u: &Usage, width: usize, max_rows: usize) -> Vec<Line<'static>> {
    let mut v: Vec<(&String, &u32)> = u.tool_counts.iter().collect();
    v.sort_by(|a, b| b.1.cmp(a.1));
    let max = v.first().map(|x| *x.1).unwrap_or(1).max(1);
    let bar_w = width.saturating_sub(20);
    v.into_iter()
        .take(max_rows)
        .map(|(name, n)| {
            let filled = ((*n as f64 / max as f64) * bar_w as f64).ceil() as usize;
            Line::from(vec![
                Span::raw(fit(name, 12)),
                Span::styled(format!("{n:>5} "), Style::new().fg(DIM)),
                Span::styled("▇".repeat(filled), Style::new().fg(ACCENT)),
            ])
        })
        .collect()
}

fn event_lines(events: &[Event], scroll: u16) -> Vec<Line<'static>> {
    events
        .iter()
        .rev()
        .skip(scroll as usize)
        .map(|e| {
            let (tag, st) = match e.kind {
                EventKind::Prompt => ("› você ", Style::new().fg(Color::Magenta).bold()),
                EventKind::Tool => ("⚙ tool ", Style::new().fg(ACCENT)),
                EventKind::Text => ("✎ texto", Style::new().fg(Color::White)),
                EventKind::Notice => ("■ task ", Style::new().fg(Color::Yellow)),
            };
            Line::from(vec![
                Span::styled(format!("{} ", fmt_time(e.ts)), Style::new().fg(DIM)),
                Span::styled(format!("{tag} "), st),
                Span::raw(e.text.clone()),
            ])
        })
        .collect()
}

fn sparkline_block(f: &mut Frame, area: Rect, u: &Usage, color: Color) {
    let data = minutes_series(u);
    let max = data.iter().copied().max().unwrap_or(0);
    let sum: u64 = data.iter().sum();
    let title = format!(" Tokens/min · última hora (soma {} · pico {}/min) ", fmt_tok(sum), fmt_tok(max));
    let w = area.width.saturating_sub(2) as usize;
    // Estica os 60 minutos para a largura disponível.
    let stretched: Vec<u64> = if w > 60 { (0..w).map(|i| data[i * 60 / w]).collect() } else { data[60 - w.min(60)..].to_vec() };
    let sp = Sparkline::default()
        .block(Block::bordered().border_type(BorderType::Rounded).border_style(Style::new().fg(DIM)).title(title))
        .data(&stretched)
        .style(Style::new().fg(color));
    f.render_widget(sp, area);
}

fn draw_session_main(f: &mut Frame, area: Rect, app: &mut App) {
    let Some(s) = app.selected_session().cloned() else {
        f.render_widget(Paragraph::new("Nenhuma sessão selecionada.").block(main_block(app, "Sessão".into())), area);
        return;
    };
    let block = main_block(app, format!("Sessão {}", s.name));
    let inner = block.inner(area);
    f.render_widget(block, area);

    match app.tab {
        1 => {
            let lines = event_lines(&s.events, app.scroll);
            let p = if lines.is_empty() { Paragraph::new("Sem atividade registrada.") } else { Paragraph::new(lines) };
            f.render_widget(p, inner);
        }
        2 => {
            let header = Row::new(vec!["PID", "Processo", "CPU", "Mem", "Comando"]).style(Style::new().fg(DIM));
            let mut rows = vec![Row::new(vec![
                Cell::from(s.pid.map(|p| p.to_string()).unwrap_or_default()),
                Cell::from("claude (sessão)").bold(),
                Cell::from(format!("{:.1}%", s.cpu)),
                Cell::from(fmt_mb(s.mem)),
                Cell::from(""),
            ])];
            for p in &s.procs {
                rows.push(Row::new(vec![
                    Cell::from(p.pid.to_string()),
                    Cell::from(format!("{}{}", "  ".repeat(p.depth), p.name)),
                    Cell::from(format!("{:.1}%", p.cpu)),
                    Cell::from(fmt_mb(p.mem)),
                    Cell::from(p.cmd.clone()).fg(DIM),
                ]));
            }
            let t = Table::new(
                rows,
                [Constraint::Length(7), Constraint::Length(22), Constraint::Length(7), Constraint::Length(9), Constraint::Min(10)],
            )
            .header(header);
            f.render_widget(t, inner);
        }
        _ => {
            let [info, mid, spark, act] = Layout::vertical([
                Constraint::Length(11),
                Constraint::Length(10),
                Constraint::Length(6),
                Constraint::Min(3),
            ])
            .areas(inner);
            let (dot_txt, dot_style) = {
                let d = session_dot(&s);
                (d.content.to_string(), d.style)
            };
            let lines = vec![
                Line::from(Span::styled(s.title.clone().unwrap_or_else(|| "(sem título)".into()), Style::new().bold())),
                Line::from(vec![
                    Span::styled(format!("{:<14}", "Status"), Style::new().fg(DIM)),
                    Span::styled(format!("{dot_txt} {}", status_label(&s)), dot_style),
                    Span::styled(
                        format!("   pid {} · v{} · {}", s.pid.map(|p| p.to_string()).unwrap_or("-".into()), s.version, s.kind),
                        Style::new().fg(DIM),
                    ),
                ]),
                kv("Diretório", s.cwd.clone()),
                kv("Modelo", s.usage.model.clone().unwrap_or_else(|| "-".into())),
                kv(
                    "Tempo",
                    format!("iniciada há {} · última resposta há {}", fmt_ago(s.started_at), fmt_ago(s.usage.last_ts)),
                ),
                kv(
                    "Paralelo",
                    format!(
                        "{} agentes rodando ({} no total) · {} tasks em background",
                        s.agents_running, s.agents_total, s.tasks_running
                    ),
                ),
                match &s.pending {
                    Some(q) => Line::from(vec![
                        Span::styled(format!("{:<14}", "Aguarda você"), Style::new().fg(DIM)),
                        Span::styled(q.clone(), Style::new().fg(Color::Magenta)),
                    ]),
                    None => kv("Processo", format!("CPU {:.1}% · {} · {} processos filhos", s.cpu, fmt_mb(s.mem), s.procs.len())),
                },
                kv("Último prompt", s.last_prompt.clone().unwrap_or_default()),
                kv(
                    "Sessão",
                    match &s.bg_id {
                        Some(id) => format!("{} · background {id}", s.session_id),
                        None => s.session_id.clone(),
                    },
                ),
                kv("Transcript", s.transcript.as_ref().map(|p| p.display().to_string()).unwrap_or_default()),
                Line::from({
                    let mut v = vec![Span::styled(format!("{:<14}", "Comandos"), Style::new().fg(DIM))];
                    let acts: Vec<(&str, &str)> = match (s.alive || s.status == "pendente", s.bg_id.is_some()) {
                        (true, false) => vec![("x", "encerrar"), ("n", "nova instância"), ("e", "pasta"), ("c", "copiar resume")],
                        (true, true) if !s.alive => vec![("o", "reabrir e responder"), ("d", "remover"), ("n", "nova instância"), ("e", "pasta")],
                        (true, true) => vec![("o", "abrir (attach)"), ("x", "parar"), ("n", "nova instância"), ("e", "pasta")],
                        (false, true) => vec![("o", "retomar"), ("d", "remover"), ("n", "nova instância"), ("e", "pasta")],
                        (false, false) => vec![("o", "retomar numa aba"), ("d", "limpar"), ("n", "nova instância"), ("e", "pasta")],
                    };
                    for (k, d) in acts {
                        v.push(Span::styled(format!(" {k} "), Style::new().fg(Color::Black).bg(ACCENT).bold()));
                        v.push(Span::styled(format!(" {d}  "), Style::new().fg(Color::Gray)));
                    }
                    v
                }),
            ];
            f.render_widget(Paragraph::new(lines), info);

            let [tok, tools] = Layout::horizontal([Constraint::Length(30), Constraint::Min(10)]).areas(mid);
            let extra = if s.agents_total > 0 { Some(("+ subagentes", s.agents_usage)) } else { None };
            let t = Table::new(token_rows(&s.usage, extra), [Constraint::Length(15), Constraint::Length(10)])
                .block(Block::bordered().border_type(BorderType::Rounded).border_style(Style::new().fg(DIM)).title(" Tokens "));
            f.render_widget(t, tok);
            let tl = tools_lines(&s.usage, tools.width.saturating_sub(2) as usize, tools.height.saturating_sub(2) as usize);
            f.render_widget(
                Paragraph::new(tl).block(Block::bordered().border_type(BorderType::Rounded).border_style(Style::new().fg(DIM)).title(" Ferramentas ")),
                tools,
            );
            sparkline_block(f, spark, &s.usage, Color::Magenta);
            let ev = event_lines(&s.events, 0);
            f.render_widget(
                Paragraph::new(ev).block(
                    Block::bordered()
                        .border_type(BorderType::Rounded)
                        .border_style(Style::new().fg(DIM))
                        .title(" Atividade recente  ( ] para ver tudo ) "),
                ),
                act,
            );
        }
    }
}

fn draw_agent_main(f: &mut Frame, area: Rect, app: &mut App) {
    let Some(a) = app.selected_agent().cloned() else {
        f.render_widget(
            Paragraph::new("Nenhum agente nesta lista. Use `a` para incluir os já terminados.").block(main_block(app, "Agente".into())),
            area,
        );
        return;
    };
    let block = main_block(app, format!("Agente {}", a.agent_type));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if app.tab == 1 {
        f.render_widget(Paragraph::new(event_lines(&a.events, app.scroll)), inner);
        return;
    }
    let [info, mid, spark, act] =
        Layout::vertical([Constraint::Length(8), Constraint::Length(10), Constraint::Length(6), Constraint::Min(3)]).areas(inner);
    let status = if a.running { Span::styled("● rodando", Style::new().fg(Color::Yellow)) } else { Span::styled("✓ terminado", Style::new().fg(Color::Green)) };
    let lines = vec![
        Line::from(Span::styled(a.description.clone(), Style::new().bold())),
        Line::from(vec![Span::styled(format!("{:<14}", "Status"), Style::new().fg(DIM)), status]),
        kv("Tipo", a.agent_type.clone()),
        kv("Sessão", a.session_name.clone()),
        kv("Workflow", a.workflow.clone().unwrap_or_else(|| "-".into())),
        kv("Modelo", a.usage.model.clone().unwrap_or_else(|| "-".into())),
        kv("Atividade", format!("última resposta há {}", fmt_ago(a.last_activity))),
        kv("Id", a.agent_id.clone()),
    ];
    f.render_widget(Paragraph::new(lines), info);
    let [tok, tools] = Layout::horizontal([Constraint::Length(30), Constraint::Min(10)]).areas(mid);
    f.render_widget(
        Table::new(token_rows(&a.usage, None), [Constraint::Length(15), Constraint::Length(10)])
            .block(Block::bordered().border_type(BorderType::Rounded).border_style(Style::new().fg(DIM)).title(" Tokens ")),
        tok,
    );
    let tl = tools_lines(&a.usage, tools.width.saturating_sub(2) as usize, tools.height.saturating_sub(2) as usize);
    f.render_widget(
        Paragraph::new(tl).block(Block::bordered().border_type(BorderType::Rounded).border_style(Style::new().fg(DIM)).title(" Ferramentas ")),
        tools,
    );
    sparkline_block(f, spark, &a.usage, Color::Yellow);
    f.render_widget(
        Paragraph::new(event_lines(&a.events, 0))
            .block(Block::bordered().border_type(BorderType::Rounded).border_style(Style::new().fg(DIM)).title(" Atividade recente ")),
        act,
    );
}

fn draw_task_main(f: &mut Frame, area: Rect, app: &mut App) {
    let Some(t) = app.selected_task().cloned() else {
        f.render_widget(
            Paragraph::new("Nenhuma task nesta lista. Use `a` para incluir as já terminadas.").block(main_block(app, "Task".into())),
            area,
        );
        return;
    };
    let block = main_block(app, format!("{} {}", t.kind.label(), t.id));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if app.tab == 1 {
        let dur = match (t.started, t.ended) {
            (Some(s), Some(e)) => fmt_dur(e - s),
            (Some(s), None) => format!("{} (em andamento)", fmt_dur(Local::now() - s)),
            _ => "-".into(),
        };
        let (icon, st) = task_style(&t);
        let mut lines = vec![
            Line::from(Span::styled(t.description.clone(), Style::new().bold())),
            Line::from(vec![
                Span::styled(format!("{:<14}", "Status"), Style::new().fg(DIM)),
                Span::styled(format!("{icon} {}", task_status_text(&t)), st),
            ]),
            kv("Tipo", t.kind.label()),
            kv("Sessão", t.session_name.clone()),
            kv("Início", fmt_time(t.started)),
            kv("Fim", fmt_time(t.ended)),
            kv("Duração", dur),
            kv("Saída", t.output_file.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "-".into())),
            Line::raw(""),
        ];
        if let Some(cmd) = &t.command {
            lines.push(Line::from(Span::styled("Comando", Style::new().fg(DIM))));
            lines.push(Line::from(Span::styled(cmd.clone(), Style::new().fg(ACCENT))));
        }
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
        return;
    }

    let Some(path) = t.output_file.clone() else {
        f.render_widget(Paragraph::new(Span::styled("Sem arquivo de saída para esta task.", Style::new().fg(DIM))), inner);
        return;
    };
    let height = inner.height as usize;
    let lines = app.tail_lines(&path).to_vec();
    let total = lines.len();
    let max_scroll = total.saturating_sub(height);
    let from_bottom = (app.scroll as usize).min(max_scroll);
    app.scroll = from_bottom as u16;
    let end = total - from_bottom;
    let start = end.saturating_sub(height);
    let text: Vec<Line> = lines[start..end].iter().map(|l| Line::raw(l.clone())).collect();
    let p = if total == 0 { Paragraph::new(Span::styled("(saída vazia até agora)", Style::new().fg(DIM))) } else { Paragraph::new(text) };
    f.render_widget(p, inner);
    if from_bottom > 0 {
        let tag = format!(" ↑ {from_bottom} linhas acima do fim ");
        let w = tag.chars().count() as u16;
        let r = Rect { x: area.right().saturating_sub(w + 2), y: area.bottom() - 1, width: w, height: 1 };
        f.render_widget(Span::styled(tag, Style::new().fg(Color::Black).bg(Color::Yellow)), r);
    }
}

fn draw_usage_main(f: &mut Frame, area: Rect, app: &mut App) {
    let block = main_block(app, "Consumo de tokens".into());
    let inner = block.inner(area);
    f.render_widget(block, area);
    let [chart, totals, models] =
        Layout::vertical([Constraint::Percentage(50), Constraint::Length(3), Constraint::Min(5)]).areas(inner);

    let bar_w: u16 = 6;
    let n = ((chart.width.saturating_sub(2)) / (bar_w + 1)).max(1) as usize;
    let days: Vec<(chrono::NaiveDate, Tokens)> = app.snap.daily.iter().rev().take(n).map(|(d, t)| (*d, *t)).collect();
    let sel = app.selected_day().map(|(d, _)| d);
    let bars: Vec<Bar> = days
        .iter()
        .rev()
        .map(|(d, t)| {
            let st = if Some(*d) == sel { Style::new().fg(Color::Yellow) } else { Style::new().fg(Color::Magenta) };
            Bar::default()
                .value(t.total())
                .text_value(fmt_tok(t.total()))
                .label(Line::from(d.format("%d/%m").to_string()))
                .style(st)
                .value_style(Style::new().fg(Color::Black).bg(if Some(*d) == sel { Color::Yellow } else { Color::Magenta }))
        })
        .collect();
    let bc = BarChart::default()
        .block(Block::bordered().border_type(BorderType::Rounded).border_style(Style::new().fg(DIM)).title(" Tokens por dia "))
        .data(BarGroup::default().bars(&bars))
        .bar_width(bar_w)
        .bar_gap(1);
    f.render_widget(bc, chart);

    let today = Local::now().date_naive();
    let sum_days = |k: i64| -> u64 {
        app.snap.daily.iter().filter(|(d, _)| (today - **d).num_days() < k).map(|(_, t)| t.total()).sum()
    };
    let line = Line::from(vec![
        Span::styled(" Hoje ", Style::new().fg(DIM)),
        Span::styled(fmt_tok(sum_days(1)), Style::new().fg(Color::Magenta).bold()),
        Span::styled("   7 dias ", Style::new().fg(DIM)),
        Span::styled(fmt_tok(sum_days(7)), Style::new().fg(Color::Magenta).bold()),
        Span::styled("   30 dias ", Style::new().fg(DIM)),
        Span::styled(fmt_tok(sum_days(30)), Style::new().fg(Color::Magenta).bold()),
        Span::styled("   desde o início ", Style::new().fg(DIM)),
        Span::styled(fmt_tok(sum_days(i64::MAX)), Style::new().fg(Color::Magenta).bold()),
    ]);
    let note = Line::from(Span::styled(
        " total = entrada + cache escrita + cache leitura + saída (respostas deduplicadas por id)",
        Style::new().fg(DIM).italic(),
    ));
    f.render_widget(Paragraph::new(vec![line, note]), totals);

    let Some((day, dt)) = app.selected_day() else { return };
    let header = Row::new(vec!["Modelo", "Entrada", "Cache esc.", "Cache leit.", "Saída", "Total"]).style(Style::new().fg(DIM));
    let mut rows: Vec<Row> = Vec::new();
    if let Some(mm) = app.snap.daily_by_model.get(&day) {
        let mut v: Vec<(&String, &Tokens)> = mm.iter().filter(|(_, t)| t.total() > 0).collect();
        v.sort_by(|a, b| b.1.total().cmp(&a.1.total()));
        for (m, t) in v {
            rows.push(Row::new(vec![
                Cell::from(m.clone()),
                Cell::from(fmt_tok(t.input)),
                Cell::from(fmt_tok(t.cache_write)),
                Cell::from(fmt_tok(t.cache_read)),
                Cell::from(fmt_tok(t.output)),
                Cell::from(fmt_tok(t.total())).fg(Color::Magenta),
            ]));
        }
    }
    rows.push(
        Row::new(vec![
            Cell::from("Total"),
            Cell::from(fmt_tok(dt.input)),
            Cell::from(fmt_tok(dt.cache_write)),
            Cell::from(fmt_tok(dt.cache_read)),
            Cell::from(fmt_tok(dt.output)),
            Cell::from(fmt_tok(dt.total())),
        ])
        .bold(),
    );
    let sessions = app.snap.daily_sessions.get(&day).copied().unwrap_or(0);
    let t = Table::new(
        rows,
        [
            Constraint::Min(18),
            Constraint::Length(9),
            Constraint::Length(11),
            Constraint::Length(12),
            Constraint::Length(9),
            Constraint::Length(9),
        ],
    )
    .header(header)
    .block(
        Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(DIM))
            .title(format!(" {} {} · {} sessões ativas ", weekday_pt(day, true), day.format("%d/%m/%Y"), sessions)),
    );
    f.render_widget(t, models);
}

fn draw_help(f: &mut Frame) {
    let area = f.area();
    let w = 70.min(area.width.saturating_sub(4));
    let h = 40.min(area.height.saturating_sub(2));
    let r = Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h };
    let k = |key: &str, desc: &str| {
        Line::from(vec![Span::styled(format!("  {key:<14}"), Style::new().fg(ACCENT).bold()), Span::raw(desc.to_string())])
    };
    let sec = |t: &str| Line::from(Span::styled(t.to_string(), Style::new().fg(FOCUS).bold()));
    let lines = vec![
        sec("Navegação"),
        k("1 2 3 4", "ir para Sessões / Agentes / Tasks / Uso"),
        k("Tab  h l ← →", "painel anterior / próximo"),
        k("j k ↑ ↓", "mover seleção"),
        k("g G Home End", "início / fim da lista"),
        k("[ ]", "trocar aba do painel principal"),
        k("J K PgUp PgDn", "rolar o painel principal"),
        k("mouse", "roda rola o que está sob o cursor · clique seleciona"),
        k("z  +", "zoom no painel principal"),
        k("Esc", "sair do zoom / fechar ajuda"),
        Line::raw(""),
        sec("Chat dentro do painel"),
        k("Enter", "sessão: abrir/entrar no chat aqui (▣ = chat aberto)"),
        k("F12  Ctrl+]", "devolver o teclado ao painel (o chat segue rodando)"),
        k("Shift+PgUp/Dn", "rolar o chat"),
        k("Ctrl+Enter", "quebrar linha no chat"),
        Line::raw(""),
        sec("Filtros"),
        k("a", "mostrar tudo das últimas 24h (inclui encerrados)"),
        k("f", "agentes/tasks só da sessão selecionada"),
        Line::raw(""),
        sec("Ações"),
        k("n", "nova instância (aqui no painel, aba nova ou background)"),
        k("o", "sessão: retomar/attach numa aba nova · task: abrir saída"),
        k("x", "parar shell/monitor · encerrar sessão (confirma)"),
        k("d  D", "limpar item terminado / todos do painel"),
        k("u", "desfazer a última limpeza"),
        k("e", "abrir a pasta da sessão no Explorer"),
        k("c", "copiar (claude --resume <id>, id, caminho…)"),
        k("r", "forçar atualização"),
        k("q  Ctrl+C", "sair"),
        Line::raw(""),
        sec("Legenda"),
        Line::from(vec![
            Span::styled("  ● ", Style::new().fg(Color::Yellow)),
            Span::raw("ocupada/rodando  "),
            Span::styled("● ", Style::new().fg(Color::Green)),
            Span::raw("ociosa  "),
            Span::styled("● ", Style::new().fg(ACCENT)),
            Span::raw("shell  "),
            Span::styled("◐ ", Style::new().fg(Color::Magenta)),
            Span::raw("espera  "),
            Span::styled("○ ", Style::new().fg(DIM)),
            Span::raw("encerrada"),
        ]),
        Line::from(vec![
            Span::styled("  ✓ ", Style::new().fg(Color::Green)),
            Span::raw("concluída  "),
            Span::styled("✗ ", Style::new().fg(Color::Red)),
            Span::raw("interrompida  "),
            Span::styled("◌ ", Style::new().fg(DIM)),
            Span::raw("monitor expirado  "),
            Span::styled("? ", Style::new().fg(Color::Magenta)),
            Span::raw("órfã"),
        ]),
        Line::from(vec![Span::raw("  "), Span::styled("⚑n", Style::new().fg(Color::Yellow)), Span::raw(" agentes rodando  "), Span::styled("⚙n", Style::new().fg(Color::Yellow)), Span::raw(" tasks rodando")]),
    ];
    f.render_widget(Clear, r);
    f.render_widget(
        Paragraph::new(Text::from(lines)).block(
            Block::bordered()
                .border_type(BorderType::Thick)
                .border_style(Style::new().fg(FOCUS))
                .title(" Ajuda ")
                .title_alignment(Alignment::Center),
        ),
        r,
    );
}
