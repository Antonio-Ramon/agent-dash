//! Renderização (ratatui). Layout no estilo lazygit: painéis numerados à
//! esquerda, painel principal com abas à direita, atalhos no rodapé.

use crate::app::{App, Panel, EXTERNAL_KIND};
use crate::i18n::{t, tr};
use crate::model::*;
use chrono::{DateTime, Local};
use ratatui::layout::{Alignment, Constraint, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, BorderType, Cell, Clear, List, ListItem, ListState, Paragraph, Row, Scrollbar,
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

fn weekday(d: chrono::NaiveDate) -> &'static str {
    use chrono::Datelike;
    let i = d.weekday().num_days_from_monday() as usize;
    if crate::i18n::en() {
        ["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday"][i]
    } else {
        ["Segunda", "Terça", "Quarta", "Quinta", "Sexta", "Sábado", "Domingo"][i]
    }
}

fn spark_text(data: &[u64]) -> String {
    const B: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let max = data.iter().copied().max().unwrap_or(0);
    data.iter()
        .map(|&v| if max == 0 || v == 0 { ' ' } else { B[((v as f64 / max as f64) * 7.0).round() as usize] })
        .collect()
}

/// Cor de cada agente nos totais de uso (barras, cabeçalho e tabela).
fn agent_color(agent: &str) -> Color {
    match agent {
        CLAUDE => Color::Magenta,
        "Antigravity" => Color::LightBlue,
        "Codex" => Color::Green,
        _ => Color::Yellow,
    }
}

/// Cor do uso na visão atual: a do agente filtrado ou a do total.
fn usage_color(app: &App) -> Color {
    app.usage_filter.as_deref().map_or(Color::Magenta, agent_color)
}

/// Tokens por minuto da última hora na visão atual (`v`).
fn per_minute_view(app: &App) -> Vec<u64> {
    match &app.usage_filter {
        None => app.snap.per_minute.clone(),
        Some(a) => app.snap.per_minute_by_agent.get(a).cloned().unwrap_or_else(|| vec![0; 60]),
    }
}

/// Partes da barra de um dia: uma por agente (Claude embaixo), ou só a do
/// agente filtrado.
fn day_segments(app: &App, day: chrono::NaiveDate) -> Vec<(u64, Color)> {
    let Some(m) = app.snap.daily_by_agent.get(&day) else { return Vec::new() };
    let mut v: Vec<(&String, &Tokens)> = m.iter().filter(|(a, t)| app.usage_shows(a) && t.total() > 0).collect();
    v.sort_by(|a, b| agent_order(a.0, b.0));
    v.into_iter().map(|(a, t)| (t.total(), agent_color(a))).collect()
}

/// Legenda dos agentes (`■ Claude ■ Antigravity`), só quando há mais de um.
fn agent_legend(app: &App) -> Vec<Span<'static>> {
    let agents = app.usage_agents();
    if agents.len() < 2 {
        return Vec::new();
    }
    agents
        .into_iter()
        .filter(|a| app.usage_shows(a))
        .flat_map(|a| [Span::styled(" ■ ", Style::new().fg(agent_color(&a))), Span::styled(a, Style::new().fg(DIM))])
        .collect()
}

struct StackBar {
    label: String,
    segs: Vec<(u64, Color)>,
    selected: bool,
}

/// Barras verticais empilhadas (uma cor por parte), com o total escrito no
/// pé da barra e o rótulo embaixo. A altura é proporcional ao total; parte
/// que não é zero ganha pelo menos 1/8 de linha para não sumir.
fn draw_stacked(f: &mut Frame, area: Rect, bars: &[StackBar], bar_w: u16, gap: u16, sel_color: Color) {
    const EIGHTHS: [&str; 9] = [" ", "▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];
    if area.height < 2 || area.width < bar_w {
        return;
    }
    let chart_h = area.height - 1;
    let h8 = chart_h as u64 * 8;
    let max = bars.iter().map(|b| b.segs.iter().map(|s| s.0).sum::<u64>()).max().unwrap_or(0).max(1);
    let buf = f.buffer_mut();
    for (i, bar) in bars.iter().enumerate() {
        let x0 = area.x + i as u16 * (bar_w + gap);
        if x0 + bar_w > area.right() {
            break;
        }
        // Fim (em oitavos de linha, a partir da base) de cada parte.
        let mut ends: Vec<(u64, Color)> = Vec::new();
        let mut acc = 0;
        for &(v, c) in bar.segs.iter().filter(|s| s.0 > 0) {
            acc = (acc + ((v * h8 + max / 2) / max).max(1)).min(h8);
            ends.push((acc, c));
        }
        let top = acc;
        let at = |p: u64| ends.iter().find(|(e, _)| *e > p).map(|x| x.1);
        for r in 0..chart_h as u64 {
            let (lo, hi) = (r * 8, r * 8 + 8);
            if top <= lo {
                break;
            }
            let Some(a) = at(lo) else { break };
            let y = area.y + chart_h - 1 - r as u16;
            let (sym, fg, bg) = if top >= hi {
                match at(hi - 1) {
                    Some(b) if b != a => {
                        let end_a = ends.iter().find(|(e, _)| *e > lo).map_or(hi, |x| x.0);
                        (EIGHTHS[(end_a - lo) as usize], a, b)
                    }
                    _ => (EIGHTHS[8], a, Color::Reset),
                }
            } else {
                (EIGHTHS[(top - lo) as usize], a, Color::Reset)
            };
            for x in x0..x0 + bar_w {
                buf[(x, y)].set_symbol(sym).set_fg(fg).set_bg(bg);
            }
        }
        let total: u64 = bar.segs.iter().map(|s| s.0).sum();
        let center = |s: &str| {
            let s: String = s.chars().take(bar_w as usize).collect();
            let pad = (bar_w as usize - s.chars().count()) / 2;
            (x0 + pad as u16, s)
        };
        if total > 0 {
            let base = if bar.selected { sel_color } else { bar.segs.iter().find(|s| s.0 > 0).map_or(Color::Magenta, |s| s.1) };
            for x in x0..x0 + bar_w {
                buf[(x, area.y + chart_h - 1)].set_symbol(" ").set_bg(base);
            }
            let (x, s) = center(&fmt_tok(total));
            buf.set_string(x, area.y + chart_h - 1, s, Style::new().fg(Color::Black).bg(base));
        }
        let (x, s) = center(&bar.label);
        let st = if bar.selected { Style::new().fg(Color::White).bold() } else { Style::new().fg(DIM) };
        buf.set_string(x, area.bottom() - 1, s, st);
    }
}

fn minutes_series(u: &Usage) -> Vec<u64> {
    let now_min = Local::now().timestamp().div_euclid(60);
    (0..60).map(|i| *u.per_minute.get(&(now_min - 59 + i)).unwrap_or(&0)).collect()
}

fn session_dot(s: &SessionInfo) -> Span<'static> {
    if s.status == "pendente" {
        return Span::styled("◐", Style::new().fg(Color::Magenta));
    }
    // Outros agentes: losango no lugar da bolinha, mesmas cores de status.
    if s.agent.is_some() {
        return match (s.alive, s.status.as_str()) {
            (false, _) => Span::styled("◇", Style::new().fg(DIM)),
            (true, "busy") => Span::styled("◆", Style::new().fg(Color::Yellow)),
            (true, "idle") => Span::styled("◆", Style::new().fg(Color::Green)),
            _ => Span::styled("◆", Style::new().fg(Color::Blue)),
        };
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
        "busy" => t("ocupada", "busy"),
        "idle" => t("ociosa", "idle"),
        "shell" => "shell",
        "blocked" => t("aguarda", "blocked"),
        "pendente" => t("espera", "waiting"),
        "encerrada" => t("encerrada", "ended"),
        "running" | "working" => t("rodando", "running"),
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

fn task_status_text(task: &TaskInfo) -> String {
    match &task.status {
        TaskStatus::Running => t("rodando", "running").into(),
        TaskStatus::Orphan => t("órfã (sessão encerrada)", "orphan (session ended)").into(),
        TaskStatus::Done(s) => match s.as_str() {
            "completed" => t("concluída", "completed").into(),
            "killed" | "stopped" => t("interrompida", "interrupted").into(),
            "failed" => t("falhou", "failed").into(),
            "expirado" => t("expirado", "expired").into(),
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
    app.chat_rect = Rect::default();
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
        Span::raw(t(" confirmar    ", " confirm    ")),
        Span::styled(" n ", Style::new().fg(Color::Black).bg(DIM).bold()),
        Span::raw(t(" cancelar", " cancel")),
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
    let r = centered(f.area(), 84, 25);
    f.render_widget(Clear, r);
    let block = Block::bordered()
        .border_type(BorderType::Thick)
        .border_style(Style::new().fg(FOCUS))
        .title(Span::styled(t(" Nova instância ", " New instance "), Style::new().fg(FOCUS).bold()))
        .title_alignment(Alignment::Center);
    let inner = block.inner(r);
    f.render_widget(block, r);
    let [fields, list, foot] = Layout::vertical([Constraint::Length(7), Constraint::Min(3), Constraint::Length(2)]).areas(inner);
    use crate::app::{F_AGENT, F_MODE, F_MODEL, F_NAME, F_PROMPT, F_REPO};
    let def = app.agents.get(form.agent);

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
    // Agentes: os que não estão no PATH aparecem riscados.
    let mut agent = vec![label(F_AGENT, t("Agente", "Agent")), Span::raw(" ")];
    for (i, a) in app.agents.iter().enumerate() {
        let missing = form.installed.get(i).is_none_or(|p| p.is_none());
        let mut st = if i == form.agent { Style::new().fg(Color::Black).bg(Color::Magenta).bold() } else { Style::new().fg(DIM) };
        if missing {
            st = st.crossed_out();
        }
        agent.push(Span::styled(format!(" {} ", a.name), st));
        agent.push(Span::raw(" "));
    }
    // O primeiro item é sempre "sem flag de modelo".
    let mut models = def.map(|d| d.model_choices()).unwrap_or_else(|| vec![String::new()]);
    models[0] = t("padrão", "default").into();
    let model_refs: Vec<&str> = models.iter().map(String::as_str).collect();
    let mut model = vec![label(F_MODEL, t("Modelo", "Model"))];
    // Lista longa (ex.: Antigravity) não cabe na linha: mostra só o escolhido.
    let avail = fields.width.saturating_sub(15) as usize;
    let full: usize = model_refs.iter().map(|m| m.chars().count() + 3).sum::<usize>() + 1;
    if full <= avail {
        model.extend(choice(&model_refs, form.model));
    } else {
        let sel = model_refs.get(form.model).copied().unwrap_or(model_refs[0]);
        model.extend([
            Span::raw(" "),
            Span::styled("◂", Style::new().fg(ACCENT)),
            Span::styled(format!(" {sel} "), Style::new().fg(Color::Black).bg(Color::Magenta).bold()),
            Span::styled("▸", Style::new().fg(ACCENT)),
            Span::styled(tr!("  {}/{} · ←→ troca", "  {}/{} · ←→ switch", form.model + 1, model_refs.len()), Style::new().fg(DIM)),
        ]);
    }
    let modes = [t("aqui no painel", "here in the panel"), t("aba nova do terminal", "new terminal tab"), "background"];
    let mut mode = vec![label(F_MODE, t("Modo", "Mode"))];
    mode.extend(choice(&modes[..form.modes(&app.agents)], form.mode));
    let lines = vec![
        Line::from(vec![label(F_REPO, t("Repositório", "Repository")), Span::raw(" "), Span::raw(form.repo.clone()), cursor(F_REPO)]),
        Line::from(agent),
        Line::from(model),
        Line::from(mode),
        Line::from(vec![
            label(F_NAME, t("Nome", "Name")),
            Span::raw(" "),
            Span::raw(form.name.clone()),
            cursor(F_NAME),
            Span::styled(if form.name.is_empty() && form.field != F_NAME { t("(opcional)", "(optional)") } else { "" }, Style::new().fg(DIM).italic()),
        ]),
        Line::from(vec![
            label(F_PROMPT, "Prompt"),
            Span::raw(" "),
            Span::raw(form.prompt.clone()),
            cursor(F_PROMPT),
            Span::styled(
                if form.prompt.is_empty() && form.field != F_PROMPT { t("(opcional: primeira mensagem)", "(optional: first message)") } else { "" },
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
                let s = if live > 1 { "s" } else { "" };
                spans.push(Span::styled(tr!("  ● {live} aberta{s}", "  ● {live} open"), Style::new().fg(Color::Green)));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();
    let title = if sug.is_empty() {
        t(
            " Nenhum repositório conhecido combina; Enter usa o caminho digitado se existir ",
            " No known repository matches; Enter uses the typed path if it exists ",
        )
        .to_string()
    } else {
        let n = sug.len();
        tr!(" Repositórios ({n}) · ↑↓ escolhe ", " Repositories ({n}) · ↑↓ to pick ")
    };
    f.render_widget(
        List::new(items).block(Block::bordered().border_type(BorderType::Rounded).border_style(Style::new().fg(DIM)).title(title)),
        list,
    );

    let model = models.get(form.model).cloned().filter(|_| form.model > 0);
    let cmd = match def {
        Some(d) if !d.is_claude() => {
            format!("{} {}", d.command, d.launch_args(model.as_deref(), Some(&form.prompt)).join(" "))
        }
        _ => {
            let spec = crate::actions::LaunchSpec {
                cwd: String::new(),
                model,
                name: Some(form.name.clone()),
                prompt: Some(form.prompt.clone()),
                background: form.mode == 2,
            };
            format!("claude {}", crate::actions::launch_args(&spec).join(" "))
        }
    };
    let missing = def.filter(|_| form.installed.get(form.agent).is_none_or(|p| p.is_none()));
    let foot_lines = vec![
        match (&form.error, missing) {
            (Some(e), _) => Line::from(Span::styled(format!(" {e}"), Style::new().fg(Color::Red))),
            (None, Some(d)) => Line::from(Span::styled(
                match &d.install {
                    Some(i) => tr!(" {} não está instalado · instale com: {i}", " {} is not installed · install with: {i}", d.command),
                    None => tr!(" {} não está no PATH", " {} is not on the PATH", d.command),
                },
                Style::new().fg(Color::Yellow),
            )),
            (None, None) => Line::from(vec![
                Span::styled(t(" em ", " in "), Style::new().fg(DIM)),
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
    let busy = s.sessions.iter().filter(|x| x.alive && !matches!(x.status.as_str(), "idle" | "blocked" | "aberta")).count();
    let agents = s.agents.iter().filter(|a| a.running).count();
    let shells = s.tasks.iter().filter(|t| t.status == TaskStatus::Running && t.kind == TaskKind::Shell).count();
    let monitors = s.tasks.iter().filter(|t| t.status == TaskStatus::Running && t.kind == TaskKind::Monitor).count();
    let today = Local::now().date_naive();
    // Uso de hoje separado por agente (só o total quando só o Claude foi usado).
    let mut today_agents: Vec<(String, u64)> = s
        .daily_by_agent
        .get(&today)
        .map(|m| m.iter().filter(|(a, t)| t.total() > 0 && app.usage_shows(a)).map(|(a, t)| (a.clone(), t.total())).collect())
        .unwrap_or_default();
    today_agents.sort_by(|a, b| agent_order(&a.0, &b.0));
    let per_agent = app.usage_agents().len() > 1 || app.usage_filter.is_some();
    let mut today_spans = Vec::new();
    if let (Some(a), true) = (&app.usage_filter, today_agents.is_empty()) {
        today_spans.push(Span::styled(format!("{a} "), Style::new().fg(agent_color(a))));
        today_spans.push(Span::styled("0", Style::new().fg(agent_color(a)).bold()));
    } else if !per_agent || today_agents.is_empty() {
        let tot: u64 = today_agents.iter().map(|x| x.1).sum();
        today_spans.push(Span::styled(fmt_tok(tot), Style::new().fg(usage_color(app)).bold()));
    } else {
        for (i, (a, n)) in today_agents.iter().enumerate() {
            if i > 0 {
                today_spans.push(Span::styled(" · ", Style::new().fg(DIM)));
            }
            today_spans.push(Span::styled(format!("{a} "), Style::new().fg(agent_color(a))));
            today_spans.push(Span::styled(fmt_tok(*n), Style::new().fg(agent_color(a)).bold()));
        }
    }
    let pm = per_minute_view(app);
    let sep = Span::styled(" │ ", Style::new().fg(DIM));
    let mut spans = vec![
        Span::styled(" agent-dash ", Style::new().fg(Color::Black).bg(FOCUS).bold()),
        Span::raw(" "),
        Span::styled(format!("{alive}"), Style::new().fg(Color::Green).bold()),
        Span::raw(t(" sessões", " sessions")),
        Span::styled(tr!(" ({busy} trabalhando)", " ({busy} working)"), Style::new().fg(Color::Yellow)),
        sep.clone(),
        Span::styled(format!("{agents}"), Style::new().fg(Color::Yellow).bold()),
        Span::raw(t(" agentes", " agents")),
        sep.clone(),
        Span::styled(format!("{shells}"), Style::new().fg(ACCENT).bold()),
        Span::raw(" shells "),
        Span::styled(format!("{monitors}"), Style::new().fg(ACCENT).bold()),
        Span::raw(" monitors"),
        sep.clone(),
        Span::raw(t("hoje ", "today ")),
    ];
    spans.extend(today_spans);
    spans.push(Span::raw(" tok "));
    spans.push(Span::styled(spark_text(&pm[pm.len().saturating_sub(30)..]), Style::new().fg(usage_color(app))));
    if !s.loaded {
        spans.push(sep.clone());
        spans.push(Span::styled(t("carregando…", "loading…"), Style::new().fg(Color::Yellow)));
    } else if s.history_total > 0 && s.history_done < s.history_total {
        spans.push(sep.clone());
        let pct = s.history_done as f64 * 100.0 / s.history_total as f64;
        spans.push(Span::styled(
            tr!("lendo histórico {pct:.0}%", "reading history {pct:.0}%"),
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
        spans.extend(
            key_line(&[
                ("F12", t("volta ao painel", "back to panel")),
                ("Shift+PgUp/PgDn", t("rolar", "scroll")),
                (t("arrastar", "drag"), t("copiar", "copy")),
                ("Ctrl+Enter", t("nova linha", "new line")),
            ])
            .spans,
        );
        spans.push(Span::styled(t("o teclado está indo para o Claude", "keys go to Claude"), Style::new().fg(DIM).italic()));
        f.render_widget(Line::from(spans), area);
        return;
    }
    if app.launcher.is_some() {
        let keys: &[(&str, &str)] = &[
            ("Tab ↑↓", t("campo", "field")),
            ("↑↓", t("repositório", "repository")),
            ("← →", t("modelo/modo", "model/mode")),
            ("Ctrl+U", t("limpar campo", "clear field")),
            ("Enter", t("iniciar", "start")),
            ("Esc", t("cancelar", "cancel")),
        ];
        f.render_widget(key_line(keys), area);
        return;
    }
    if app.confirm.is_some() {
        f.render_widget(key_line(&[("y Enter", t("confirmar", "confirm")), ("n Esc", t("cancelar", "cancel"))]), area);
        return;
    }
    let (new, clean, folder, copy) = (t("nova", "new"), t("limpar", "clear"), t("pasta", "folder"), t("copiar", "copy"));
    let view = match &app.usage_filter {
        None => t("agente: todos", "agent: all").to_string(),
        Some(a) => tr!("agente: {a}", "agent: {a}"),
    };
    let ctx: &[(&str, &str)] = match app.focus {
        Panel::Sessions => &[("Enter", "chat"), ("n", new), ("o", t("aba nova", "new tab")), ("x", t("encerrar", "end")), ("d", clean), ("e", folder), ("c", copy)],
        Panel::Agents => &[("n", new), ("d/D", clean), ("e", folder), ("c", t("copiar id", "copy id"))],
        Panel::Tasks => &[("x", t("parar", "stop")), ("d/D", clean), ("o", t("saída", "output")), ("c", t("caminho", "path")), ("J/K", t("rolar", "scroll"))],
        Panel::Usage => &[("n", new), ("c", copy), ("v", view.as_str())],
    };
    let common: &[(&str, &str)] = &[
        ("1-4", t("painel", "panel")),
        ("↑↓", t("mover", "move")),
        ("[ ]", t("aba", "tab")),
        ("z", "zoom"),
        ("a", if app.show_all { t("só ativos", "active only") } else { t("tudo", "all") }),
        ("f", if app.only_selected { t("todas", "all sessions") } else { t("filtrar", "filter") }),
    ];
    let tail: &[(&str, &str)] = &[("?", t("ajuda", "help")), ("q", t("sair", "quit"))];
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
                Span::styled(
                    // Agente cujos tokens o dashboard não lê (ou ainda sem resposta): "—".
                    format!(
                        "{:>7}",
                        if s.agent.is_some() && s.usage.turns == 0 { "—".into() } else { fmt_tok(s.usage.tokens.total() + s.agents_usage.total()) }
                    ),
                    Style::new().fg(Color::Magenta),
                ),
                Span::styled(format!(" {:>5}", fmt_ago(s.usage.last_ts.or(s.started_at))), Style::new().fg(DIM)),
                Span::styled(extra, Style::new().fg(Color::Yellow)),
            ]))
        })
        .collect();
    let n = items.len();
    let title = match app.show_all {
        true => tr!("[1] Sessões ({alive} vivas, {n} em 24h)", "[1] Sessions ({alive} alive, {n} in 24h)"),
        false => tr!("[1] Sessões ({alive})", "[1] Sessions ({alive})"),
    };
    draw_list(f, area, app, Panel::Sessions, items, title);
    if n == 0 {
        empty_hint(f, area, t("nenhuma sessão do Claude Code rodando", "no Claude Code session running"));
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
    let scope = if app.only_selected { t(" · sessão", " · session") } else { "" };
    let title = tr!("[2] Agentes ({running} rodando{scope})", "[2] Agents ({running} running{scope})");
    draw_list(f, area, app, Panel::Agents, items, title);
    if n == 0 {
        empty_hint(f, area, t("nenhum subagente ativo", "no active subagent"));
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
    let scope = if app.only_selected { t(" · sessão", " · session") } else { "" };
    let title = tr!("[3] Shells/Monitors/Tasks ({running} rodando{scope})", "[3] Shells/Monitors/Tasks ({running} running{scope})");
    draw_list(f, area, app, Panel::Tasks, items, title);
    if n == 0 {
        empty_hint(f, area, t("nada em background", "nothing in background"));
    }
}

fn draw_days(f: &mut Frame, area: Rect, app: &mut App) {
    let focused = app.focus == Panel::Usage;
    let days = app.days(); // mais recente primeiro
    let title = match &app.usage_filter {
        None => t("[4] Uso por dia", "[4] Daily usage").to_string(),
        Some(a) => tr!("[4] Uso por dia · {a}", "[4] Daily usage · {a}"),
    };
    let block = panel_block(title, focused);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let bar_w: u16 = 6;
    let n = ((inner.width + 1) / (bar_w + 1)).max(1) as usize;
    // Janela de dias que mantém o selecionado visível; mais antigo à esquerda.
    let sel = app.states[3].selected().unwrap_or(0);
    let start = (sel + 1).saturating_sub(n);
    let bars: Vec<StackBar> = days
        .iter()
        .enumerate()
        .skip(start)
        .take(n)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(|(i, (d, _))| StackBar { label: d.format("%d/%m").to_string(), segs: day_segments(app, *d), selected: i == sel })
        .collect();
    draw_stacked(f, inner, &bars, bar_w, 1, if focused { Color::Yellow } else { Color::LightMagenta });
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
            let (out, cache) = (fmt_tok(u.tokens.output), fmt_tok(u.tokens.cache_read));
            let mut v = vec![
                lab(t(" contexto ", " context ")),
                Span::styled(fmt_tok(u.last_turn.context()), Style::new().fg(Color::Yellow).bold()),
                sep.clone(),
                lab(t("sessão ", "session ")),
                Span::styled(fmt_tok(u.tokens.total()), Style::new().fg(Color::Magenta).bold()),
                lab(" tok"),
                lab(&tr!("  (saída {out} · cache lido {cache})", "  (output {out} · cache read {cache})")),
            ];
            if s.agents_total > 0 {
                v.push(lab(t(" + subagentes ", " + subagents ")));
                v.push(Span::styled(fmt_tok(s.agents_usage.total()), Style::new().fg(Color::Magenta)));
            }
            v.extend([
                sep.clone(),
                Span::styled(format!("{}/min", fmt_tok(last5)), Style::new().fg(ACCENT)),
                lab(" "),
                Span::styled(spark_text(&mins[mins.len() - 20..]), Style::new().fg(ACCENT)),
                sep.clone(),
                lab(&tr!("{} respostas", "{} replies", u.turns)),
                sep.clone(),
                lab(u.model.as_deref().unwrap_or("-")),
            ]);
            Line::from(v)
        }
        None => match sess.filter(|s| s.agent.is_some()) {
            Some(s) if s.agent.as_deref() == Some(crate::antigravity::AGENT) => {
                Line::from(lab(t(" Antigravity · contexto e tokens aparecem depois da primeira resposta", " Antigravity · context and tokens show up after the first reply")))
            }
            Some(s) => Line::from(lab(&tr!(
                " {} · o dashboard ainda não lê os tokens deste agente",
                " {} · the dashboard does not read this agent's tokens yet",
                s.agent.as_deref().unwrap_or("agente")
            ))),
            None => Line::from(lab(t(" contexto e tokens aparecem depois da primeira resposta", " context and tokens show up after the first reply"))),
        },
    };
    f.render_widget(line.style(Style::new().bg(Color::Rgb(22, 26, 36))), meter);
    let Some(i) = idx else {
        let key = |k: &'static str, d: &'static str| Line::from(vec![Span::styled(k, Style::new().fg(ACCENT).bold()), Span::raw(d)]);
        let lines = vec![
            Line::raw(""),
            Line::from(Span::styled(t("  Esta sessão não tem chat aberto aqui dentro.", "  This session has no chat open in here."), Style::new().fg(DIM))),
            Line::raw(""),
            key("  Enter ", t("abre o chat aqui (retoma uma sessão encerrada ou em background)", "opens the chat here (resumes an ended or background session)")),
            key("  n     ", t("nova instância dentro do painel", "new instance inside the panel")),
            key("  F12   ", t("devolve o teclado ao painel quando estiver no chat", "gives the keyboard back to the panel while in the chat")),
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
    // Rolagem além do histórico não existe: guarda a real, que é a usada na seleção.
    t.scroll = shown;
    app.chat_rect = inner;
    if let Some(sel) = app.chat_sel.as_ref().filter(|s| s.key == app.terms[i].key) {
        let (a, b) = if sel.anchor <= sel.head { (sel.anchor, sel.head) } else { (sel.head, sel.anchor) };
        let style = Style::new().fg(Color::White).bg(Color::Rgb(60, 95, 160));
        let buf = f.buffer_mut();
        for r in 0..inner.height {
            let l = r as i64 - shown as i64;
            if l < a.0 || l > b.0 {
                continue;
            }
            let c0 = if l == a.0 { a.1 } else { 0 };
            let c1 = if l == b.0 { b.1 } else { inner.width.saturating_sub(1) };
            for c in c0..=c1.min(inner.width.saturating_sub(1)) {
                buf[(inner.x + c, inner.y + r)].set_style(style);
            }
        }
    }
    if shown > 0 {
        let tag = tr!(" ↑ {shown} linhas acima do fim ", " ↑ {shown} lines above the end ");
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
    let tk = &u.tokens;
    let mut rows = vec![
        Row::new(vec![Cell::from(t("Entrada", "Input")), Cell::from(fmt_tok(tk.input))]),
        Row::new(vec![Cell::from(t("Cache escrita", "Cache write")), Cell::from(fmt_tok(tk.cache_write))]),
        Row::new(vec![Cell::from(t("Cache leitura", "Cache read")), Cell::from(fmt_tok(tk.cache_read))]),
        Row::new(vec![Cell::from(t("Saída", "Output")), Cell::from(fmt_tok(tk.output))]),
        Row::new(vec![Cell::from("Total").bold(), Cell::from(fmt_tok(tk.total())).fg(Color::Magenta).bold()]),
    ];
    if let Some((label, x)) = extra {
        rows.push(Row::new(vec![Cell::from(label.to_string()), Cell::from(fmt_tok(x.total())).fg(Color::Magenta)]));
    }
    rows.push(Row::new(vec![Cell::from(t("Contexto atual", "Current context")), Cell::from(fmt_tok(u.last_turn.context())).fg(Color::Yellow)]));
    rows.push(Row::new(vec![Cell::from(t("Respostas", "Replies")), Cell::from(u.turns.to_string())]));
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
                EventKind::Prompt => (t("› você ", "› you  "), Style::new().fg(Color::Magenta).bold()),
                EventKind::Tool => ("⚙ tool ", Style::new().fg(ACCENT)),
                EventKind::Text => (t("✎ texto", "✎ text "), Style::new().fg(Color::White)),
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
    let (sum, max) = (fmt_tok(sum), fmt_tok(max));
    let title = tr!(" Tokens/min · última hora (soma {sum} · pico {max}/min) ", " Tokens/min · last hour (sum {sum} · peak {max}/min) ");
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
        f.render_widget(Paragraph::new(t("Nenhuma sessão selecionada.", "No session selected.")).block(main_block(app, t("Sessão", "Session").into())), area);
        return;
    };
    let block = main_block(app, tr!("Sessão {}", "Session {}", s.name));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if let (Some(agent), 0) = (&s.agent, app.tab) {
        // Outro agente: resumo próprio, sem subagentes nem tasks (Atividade e
        // Processos usam as abas normais).
        let in_panel = app.term_idx(&s.session_id).is_some();
        let status = if s.alive { format!("◆ {}", status_label(&s)) } else { format!("○ {}", t("encerrada", "ended")) };
        let mut lines = vec![
            kv(t("Agente", "Agent"), agent.clone()),
            kv("Status", if s.alive { format!("{status}   pid {}", s.pid.unwrap_or(0)) } else { status }),
            kv(t("Diretório", "Directory"), s.cwd.clone()),
        ];
        if s.kind == EXTERNAL_KIND {
            lines.push(kv(t("Comando", "Command"), s.title.clone().unwrap_or_default()));
        } else {
            let conv = if s.session_id.starts_with(crate::antigravity::NEW_PREFIX) {
                t("ainda não existe (nasce na primeira mensagem)", "does not exist yet (created by the first message)").to_string()
            } else {
                s.session_id.clone()
            };
            lines.push(kv(t("Conversa", "Conversation"), conv));
            lines.push(kv(t("Último prompt", "Last prompt"), s.last_prompt.clone().unwrap_or_else(|| "-".into())));
            lines.push(kv(t("Atividade", "Activity"), format!("{} ({})", fmt_time(s.usage.last_ts), fmt_ago(s.usage.last_ts))));
        }
        lines.push(kv(t("Aberta", "Opened"), fmt_time(s.started_at)));
        let u = &s.usage;
        if u.turns > 0 {
            lines.push(kv(
                "Tokens",
                tr!(
                    "{}  (entrada {} · cache lido {} · saída {})",
                    "{}  (input {} · cache read {} · output {})",
                    fmt_tok(u.tokens.total()),
                    fmt_tok(u.tokens.input),
                    fmt_tok(u.tokens.cache_read),
                    fmt_tok(u.tokens.output)
                ),
            ));
            lines.push(kv(t("Contexto atual", "Current context"), fmt_tok(u.last_turn.context())));
            lines.push(kv(t("Modelo", "Model"), u.model.clone().unwrap_or_else(|| "-".into())));
            lines.push(kv(t("Respostas", "Replies"), u.turns.to_string()));
        }
        lines.push(Line::raw(""));
        let hint = match (in_panel, s.alive) {
            (true, _) => t("Roda aqui no painel: Enter entra no chat, F12 volta, x fecha.", "Runs here in the panel: Enter goes into the chat, F12 returns, x closes."),
            (false, true) => t(
                "Aberta em outro terminal: x encerra, depois Enter retoma aqui no painel.",
                "Open in another terminal: x ends it, then Enter resumes it here in the panel.",
            ),
            (false, false) => t("Enter retoma aqui no painel, o numa aba nova, c copia o comando.", "Enter resumes here in the panel, o in a new tab, c copies the command."),
        };
        let tokens = if u.turns > 0 {
            t("Subagentes e tasks só existem no Claude Code.", "Subagents and tasks only exist in Claude Code.")
        } else if agent == crate::antigravity::AGENT {
            t("Os tokens aparecem depois da primeira resposta do modelo.", "Tokens show up after the model's first reply.")
        } else {
            t("O dashboard ainda não lê os tokens deste agente.", "The dashboard does not read this agent's tokens yet.")
        };
        for line in [hint, tokens] {
            lines.push(Line::from(Span::styled(line, Style::new().fg(DIM))));
        }
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
        return;
    }

    match app.tab {
        1 => {
            let lines = event_lines(&s.events, app.scroll);
            let p = if lines.is_empty() { Paragraph::new(t("Sem atividade registrada.", "No recorded activity.")) } else { Paragraph::new(lines) };
            f.render_widget(p, inner);
        }
        2 => {
            let header = Row::new(vec!["PID", t("Processo", "Process"), "CPU", "Mem", t("Comando", "Command")]).style(Style::new().fg(DIM));
            let mut rows = vec![Row::new(vec![
                Cell::from(s.pid.map(|p| p.to_string()).unwrap_or_default()),
                Cell::from(tr!("{} (sessão)", "{} (session)", s.agent.as_deref().unwrap_or("claude").to_lowercase())).bold(),
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
            let (started, last) = (fmt_ago(s.started_at), fmt_ago(s.usage.last_ts));
            let (ar, at, tr_) = (s.agents_running, s.agents_total, s.tasks_running);
            let (cpu, mem, nproc) = (s.cpu, fmt_mb(s.mem), s.procs.len());
            let lines = vec![
                Line::from(Span::styled(s.title.clone().unwrap_or_else(|| t("(sem título)", "(untitled)").into()), Style::new().bold())),
                Line::from(vec![
                    Span::styled(format!("{:<14}", "Status"), Style::new().fg(DIM)),
                    Span::styled(format!("{dot_txt} {}", status_label(&s)), dot_style),
                    Span::styled(
                        format!("   pid {} · v{} · {}", s.pid.map(|p| p.to_string()).unwrap_or("-".into()), s.version, s.kind),
                        Style::new().fg(DIM),
                    ),
                ]),
                kv(t("Diretório", "Directory"), s.cwd.clone()),
                kv(t("Modelo", "Model"), s.usage.model.clone().unwrap_or_else(|| "-".into())),
                kv(
                    t("Tempo", "Time"),
                    tr!("iniciada há {started} · última resposta há {last}", "started {started} ago · last reply {last} ago"),
                ),
                kv(
                    t("Paralelo", "Parallel"),
                    tr!(
                        "{ar} agentes rodando ({at} no total) · {tr_} tasks em background",
                        "{ar} agents running ({at} total) · {tr_} background tasks"
                    ),
                ),
                match &s.pending {
                    Some(q) => Line::from(vec![
                        Span::styled(format!("{:<14}", t("Aguarda você", "Waiting on you")), Style::new().fg(DIM)),
                        Span::styled(q.clone(), Style::new().fg(Color::Magenta)),
                    ]),
                    None => kv(
                        t("Processo", "Process"),
                        tr!("CPU {cpu:.1}% · {mem} · {nproc} processos filhos", "CPU {cpu:.1}% · {mem} · {nproc} child processes"),
                    ),
                },
                kv(t("Último prompt", "Last prompt"), s.last_prompt.clone().unwrap_or_default()),
                kv(
                    t("Sessão", "Session"),
                    match &s.bg_id {
                        Some(id) => format!("{} · background {id}", s.session_id),
                        None => s.session_id.clone(),
                    },
                ),
                kv("Transcript", s.transcript.as_ref().map(|p| p.display().to_string()).unwrap_or_default()),
                Line::from({
                    let mut v = vec![Span::styled(format!("{:<14}", t("Comandos", "Commands")), Style::new().fg(DIM))];
                    let (new, folder, remove) = (t("nova instância", "new instance"), t("pasta", "folder"), t("remover", "remove"));
                    let acts: Vec<(&str, &str)> = match (s.alive || s.status == "pendente", s.bg_id.is_some()) {
                        (true, false) => vec![("x", t("encerrar", "end")), ("n", new), ("e", folder), ("c", t("copiar resume", "copy resume"))],
                        (true, true) if !s.alive => vec![("o", t("reabrir e responder", "reopen and reply")), ("d", remove), ("n", new), ("e", folder)],
                        (true, true) => vec![("o", t("abrir (attach)", "open (attach)")), ("x", t("parar", "stop")), ("n", new), ("e", folder)],
                        (false, true) => vec![("o", t("retomar", "resume")), ("d", remove), ("n", new), ("e", folder)],
                        (false, false) => vec![("o", t("retomar numa aba", "resume in a tab")), ("d", t("limpar", "clear")), ("n", new), ("e", folder)],
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
            let extra = if s.agents_total > 0 { Some((t("+ subagentes", "+ subagents"), s.agents_usage)) } else { None };
            let tbl = Table::new(token_rows(&s.usage, extra), [Constraint::Length(15), Constraint::Length(10)])
                .block(Block::bordered().border_type(BorderType::Rounded).border_style(Style::new().fg(DIM)).title(" Tokens "));
            f.render_widget(tbl, tok);
            let tl = tools_lines(&s.usage, tools.width.saturating_sub(2) as usize, tools.height.saturating_sub(2) as usize);
            f.render_widget(
                Paragraph::new(tl).block(Block::bordered().border_type(BorderType::Rounded).border_style(Style::new().fg(DIM)).title(t(" Ferramentas ", " Tools "))),
                tools,
            );
            sparkline_block(f, spark, &s.usage, Color::Magenta);
            let ev = event_lines(&s.events, 0);
            f.render_widget(
                Paragraph::new(ev).block(
                    Block::bordered()
                        .border_type(BorderType::Rounded)
                        .border_style(Style::new().fg(DIM))
                        .title(t(" Atividade recente  ( ] para ver tudo ) ", " Recent activity  ( ] to see all ) ")),
                ),
                act,
            );
        }
    }
}

fn draw_agent_main(f: &mut Frame, area: Rect, app: &mut App) {
    let Some(a) = app.selected_agent().cloned() else {
        f.render_widget(
            Paragraph::new(t("Nenhum agente nesta lista. Use `a` para incluir os já terminados.", "No agent in this list. Press `a` to include finished ones."))
                .block(main_block(app, t("Agente", "Agent").into())),
            area,
        );
        return;
    };
    let block = main_block(app, tr!("Agente {}", "Agent {}", a.agent_type));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if app.tab == 1 {
        f.render_widget(Paragraph::new(event_lines(&a.events, app.scroll)), inner);
        return;
    }
    let [info, mid, spark, act] =
        Layout::vertical([Constraint::Length(8), Constraint::Length(10), Constraint::Length(6), Constraint::Min(3)]).areas(inner);
    let status = match a.running {
        true => Span::styled(t("● rodando", "● running"), Style::new().fg(Color::Yellow)),
        false => Span::styled(t("✓ terminado", "✓ finished"), Style::new().fg(Color::Green)),
    };
    let last = fmt_ago(a.last_activity);
    let lines = vec![
        Line::from(Span::styled(a.description.clone(), Style::new().bold())),
        Line::from(vec![Span::styled(format!("{:<14}", "Status"), Style::new().fg(DIM)), status]),
        kv(t("Tipo", "Type"), a.agent_type.clone()),
        kv(t("Sessão", "Session"), a.session_name.clone()),
        kv("Workflow", a.workflow.clone().unwrap_or_else(|| "-".into())),
        kv(t("Modelo", "Model"), a.usage.model.clone().unwrap_or_else(|| "-".into())),
        kv(t("Atividade", "Activity"), tr!("última resposta há {last}", "last reply {last} ago")),
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
        Paragraph::new(tl).block(Block::bordered().border_type(BorderType::Rounded).border_style(Style::new().fg(DIM)).title(t(" Ferramentas ", " Tools "))),
        tools,
    );
    sparkline_block(f, spark, &a.usage, Color::Yellow);
    f.render_widget(
        Paragraph::new(event_lines(&a.events, 0))
            .block(Block::bordered().border_type(BorderType::Rounded).border_style(Style::new().fg(DIM)).title(t(" Atividade recente ", " Recent activity "))),
        act,
    );
}

fn draw_task_main(f: &mut Frame, area: Rect, app: &mut App) {
    let Some(task) = app.selected_task().cloned() else {
        f.render_widget(
            Paragraph::new(t("Nenhuma task nesta lista. Use `a` para incluir as já terminadas.", "No task in this list. Press `a` to include finished ones."))
                .block(main_block(app, "Task".into())),
            area,
        );
        return;
    };
    let block = main_block(app, format!("{} {}", task.kind.label(), task.id));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if app.tab == 1 {
        let dur = match (task.started, task.ended) {
            (Some(s), Some(e)) => fmt_dur(e - s),
            (Some(s), None) => tr!("{} (em andamento)", "{} (in progress)", fmt_dur(Local::now() - s)),
            _ => "-".into(),
        };
        let (icon, st) = task_style(&task);
        let mut lines = vec![
            Line::from(Span::styled(task.description.clone(), Style::new().bold())),
            Line::from(vec![
                Span::styled(format!("{:<14}", "Status"), Style::new().fg(DIM)),
                Span::styled(format!("{icon} {}", task_status_text(&task)), st),
            ]),
            kv(t("Tipo", "Type"), task.kind.label()),
            kv(t("Sessão", "Session"), task.session_name.clone()),
            kv(t("Início", "Start"), fmt_time(task.started)),
            kv(t("Fim", "End"), fmt_time(task.ended)),
            kv(t("Duração", "Duration"), dur),
            kv(t("Saída", "Output"), task.output_file.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "-".into())),
            Line::raw(""),
        ];
        if let Some(cmd) = &task.command {
            lines.push(Line::from(Span::styled(t("Comando", "Command"), Style::new().fg(DIM))));
            lines.push(Line::from(Span::styled(cmd.clone(), Style::new().fg(ACCENT))));
        }
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
        return;
    }

    let Some(path) = task.output_file.clone() else {
        f.render_widget(Paragraph::new(Span::styled(t("Sem arquivo de saída para esta task.", "No output file for this task."), Style::new().fg(DIM))), inner);
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
    let p = match total {
        0 => Paragraph::new(Span::styled(t("(saída vazia até agora)", "(output empty so far)"), Style::new().fg(DIM))),
        _ => Paragraph::new(text),
    };
    f.render_widget(p, inner);
    if from_bottom > 0 {
        let tag = tr!(" ↑ {from_bottom} linhas acima do fim ", " ↑ {from_bottom} lines above the end ");
        let w = tag.chars().count() as u16;
        let r = Rect { x: area.right().saturating_sub(w + 2), y: area.bottom() - 1, width: w, height: 1 };
        f.render_widget(Span::styled(tag, Style::new().fg(Color::Black).bg(Color::Yellow)), r);
    }
}

fn draw_usage_main(f: &mut Frame, area: Rect, app: &mut App) {
    let title = match &app.usage_filter {
        None => t("Consumo de tokens · todos os agentes", "Token usage · all agents").to_string(),
        Some(a) => tr!("Consumo de tokens · só {a}", "Token usage · {a} only"),
    };
    let block = main_block(app, title);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let [chart, totals, models] =
        Layout::vertical([Constraint::Percentage(50), Constraint::Length(3), Constraint::Min(5)]).areas(inner);

    let bar_w: u16 = 6;
    let n = ((chart.width.saturating_sub(2)) / (bar_w + 1)).max(1) as usize;
    let days: Vec<chrono::NaiveDate> = app.days().into_iter().take(n).map(|(d, _)| d).collect();
    let sel = app.selected_day().map(|(d, _)| d);
    let bars: Vec<StackBar> = days
        .iter()
        .rev()
        .map(|d| StackBar { label: d.format("%d/%m").to_string(), segs: day_segments(app, *d), selected: Some(*d) == sel })
        .collect();
    let mut title = vec![Span::raw(t(" Tokens por dia ", " Tokens per day "))];
    title.extend(agent_legend(app));
    if title.len() > 1 {
        title.push(Span::raw(" "));
    }
    let chart_block = Block::bordered().border_type(BorderType::Rounded).border_style(Style::new().fg(DIM)).title(Line::from(title));
    let chart_inner = chart_block.inner(chart);
    f.render_widget(chart_block, chart);
    draw_stacked(f, chart_inner, &bars, bar_w, 1, Color::Yellow);

    let today = Local::now().date_naive();
    let all_days = app.days();
    let sum_days = |k: i64| -> u64 { all_days.iter().filter(|(d, _)| (today - *d).num_days() < k).map(|(_, t)| t.total()).sum() };
    let c = usage_color(app);
    let line = Line::from(vec![
        Span::styled(t(" Hoje ", " Today "), Style::new().fg(DIM)),
        Span::styled(fmt_tok(sum_days(1)), Style::new().fg(c).bold()),
        Span::styled(t("   7 dias ", "   7 days "), Style::new().fg(DIM)),
        Span::styled(fmt_tok(sum_days(7)), Style::new().fg(c).bold()),
        Span::styled(t("   30 dias ", "   30 days "), Style::new().fg(DIM)),
        Span::styled(fmt_tok(sum_days(30)), Style::new().fg(c).bold()),
        Span::styled(t("   desde o início ", "   all time "), Style::new().fg(DIM)),
        Span::styled(fmt_tok(sum_days(i64::MAX)), Style::new().fg(c).bold()),
    ]);
    let note = Line::from(Span::styled(
        t(
            " total = entrada + cache escrita + cache leitura + saída (respostas deduplicadas por id) · v troca o agente",
            " total = input + cache write + cache read + output (replies deduplicated by id) · v switches the agent",
        ),
        Style::new().fg(DIM).italic(),
    ));
    f.render_widget(Paragraph::new(vec![line, note]), totals);

    let Some((day, dt)) = app.selected_day() else { return };
    let header = Row::new(vec![t("Modelo", "Model"), t("Entrada", "Input"), t("Cache esc.", "Cache wr."), t("Cache leit.", "Cache rd."), t("Saída", "Output"), "Total"])
        .style(Style::new().fg(DIM));
    let tok_row = |name: Cell<'static>, t: &Tokens, total_color: Color| {
        Row::new(vec![
            name,
            Cell::from(fmt_tok(t.input)),
            Cell::from(fmt_tok(t.cache_write)),
            Cell::from(fmt_tok(t.cache_read)),
            Cell::from(fmt_tok(t.output)),
            Cell::from(fmt_tok(t.total())).fg(total_color),
        ])
    };
    let mut rows: Vec<Row> = Vec::new();
    // Modelos agrupados por agente, com o subtotal de cada um quando há mais de um.
    let mut agents: Vec<(&String, &Tokens)> = app
        .snap
        .daily_by_agent
        .get(&day)
        .map(|m| m.iter().filter(|(a, t)| app.usage_shows(a) && t.total() > 0).collect())
        .unwrap_or_default();
    agents.sort_by(|a, b| agent_order(a.0, b.0));
    let grouped = agents.len() > 1;
    for (agent, at) in &agents {
        let color = agent_color(agent);
        if grouped {
            rows.push(tok_row(Cell::from(agent.to_string()).fg(color).bold(), at, color).bold());
        }
        let mut v: Vec<(&String, &Tokens)> = app
            .snap
            .daily_by_model
            .get(&day)
            .map(|mm| {
                mm.iter()
                    .filter(|(m, t)| t.total() > 0 && app.snap.model_agent.get(*m).map_or(CLAUDE, String::as_str) == agent.as_str())
                    .collect()
            })
            .unwrap_or_default();
        v.sort_by(|a, b| b.1.total().cmp(&a.1.total()));
        for (m, t) in v {
            let name = if grouped { format!("  {m}") } else { m.clone() };
            rows.push(tok_row(Cell::from(name), t, color));
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
    let sessions: usize =
        app.snap.daily_sessions.get(&day).map_or(0, |m| m.iter().filter(|(a, _)| app.usage_shows(a)).map(|(_, n)| *n).sum());
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
            .title(tr!(" {} {} · {sessions} sessões ativas ", " {} {} · {sessions} active sessions ", weekday(day), day.format("%d/%m/%Y"))),
    );
    f.render_widget(t, models);
}

fn draw_help(f: &mut Frame) {
    let area = f.area();
    let w = 78.min(area.width.saturating_sub(4));
    let h = 40.min(area.height.saturating_sub(2));
    let r = Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h };
    let k = |key: &'static str, desc: &'static str| Line::from(vec![Span::styled(format!("  {key:<14}"), Style::new().fg(ACCENT).bold()), Span::raw(desc)]);
    let sec = |s: &'static str| Line::from(Span::styled(s, Style::new().fg(FOCUS).bold()));
    let dot = |s: &'static str, c: Color| Span::styled(s, Style::new().fg(c));
    let lines = vec![
        sec(t("Navegação", "Navigation")),
        k("1 2 3 4", t("ir para Sessões / Agentes / Tasks / Uso", "go to Sessions / Agents / Tasks / Usage")),
        k("Tab  h l ← →", t("painel anterior / próximo", "previous / next panel")),
        k("j k ↑ ↓", t("mover seleção", "move selection")),
        k("g G Home End", t("início / fim da lista", "top / bottom of the list")),
        k("[ ]", t("trocar aba do painel principal", "switch main panel tab")),
        k("J K PgUp PgDn", t("rolar o painel principal", "scroll the main panel")),
        k("mouse", t("roda rola o que está sob o cursor · clique seleciona", "wheel scrolls what is under the cursor · click selects")),
        k("z  +", t("zoom no painel principal", "zoom the main panel")),
        k("Esc", t("sair do zoom / fechar ajuda", "leave zoom / close help")),
        Line::raw(""),
        sec(t("Chat dentro do painel", "Chat inside the panel")),
        k("Enter", t("sessão: abrir/entrar no chat aqui (▣ = chat aberto)", "session: open/enter the chat here (▣ = chat open)")),
        k("F12  Ctrl+]", t("devolver o teclado ao painel (o chat segue rodando)", "give the keyboard back to the panel (chat keeps running)")),
        k("Shift+PgUp/Dn", t("rolar o chat", "scroll the chat")),
        k(t("arrastar", "drag"), t("selecionar texto do chat e copiar (Ctrl+C copia de novo)", "select chat text and copy it (Ctrl+C copies again)")),
        k("Ctrl+Enter", t("quebrar linha no chat", "new line in the chat")),
        Line::raw(""),
        sec(t("Filtros", "Filters")),
        k("a", t("mostrar tudo das últimas 24h (inclui encerrados)", "show everything from the last 24h (including ended)")),
        k("f", t("agentes/tasks só da sessão selecionada", "agents/tasks of the selected session only")),
        k("v", t("uso de tokens: todos os agentes ou um só (cabeçalho, barras e tabela)", "token usage: all agents or just one (header, bars and table)")),
        Line::raw(""),
        sec(t("Ações", "Actions")),
        k(
            "n",
            t(
                "nova instância: Claude, Antigravity, Codex… (aqui no painel, aba nova ou background)",
                "new instance: Claude, Antigravity, Codex… (here in the panel, new tab or background)",
            ),
        ),
        k("o", t("sessão: retomar/attach numa aba nova · task: abrir saída", "session: resume/attach in a new tab · task: open output")),
        k("x", t("parar shell/monitor · encerrar sessão (confirma)", "stop shell/monitor · end session (asks first)")),
        k("d  D", t("limpar item terminado / todos do painel", "clear finished item / all in the panel")),
        k("u", t("desfazer a última limpeza", "undo the last clear")),
        k("e", t("abrir a pasta da sessão no Explorer", "open the session folder in Explorer")),
        k("c", t("copiar (claude --resume <id>, id, caminho…)", "copy (claude --resume <id>, id, path…)")),
        k("r", t("forçar atualização", "force refresh")),
        k("q  Ctrl+C", t("sair", "quit")),
        Line::raw(""),
        sec(t("Legenda", "Legend")),
        Line::from(vec![
            dot("  ● ", Color::Yellow),
            Span::raw(t("ocupada/rodando  ", "busy/running  ")),
            dot("● ", Color::Green),
            Span::raw(t("ociosa  ", "idle  ")),
            dot("● ", ACCENT),
            Span::raw("shell  "),
            dot("◐ ", Color::Magenta),
            Span::raw(t("espera  ", "waiting  ")),
            dot("○ ", DIM),
            Span::raw(t("encerrada", "ended")),
        ]),
        Line::from(vec![
            dot("  ◆ ", Color::Green),
            Span::raw(t("sessão de outro agente (Antigravity, Codex…), mesmas cores  ", "session of another agent (Antigravity, Codex…), same colors  ")),
            dot("◇ ", DIM),
            Span::raw(t("encerrada", "ended")),
        ]),
        Line::from(vec![
            dot("  ✓ ", Color::Green),
            Span::raw(t("concluída  ", "completed  ")),
            dot("✗ ", Color::Red),
            Span::raw(t("interrompida  ", "interrupted  ")),
            dot("◌ ", DIM),
            Span::raw(t("monitor expirado  ", "monitor expired  ")),
            dot("? ", Color::Magenta),
            Span::raw(t("órfã", "orphan")),
        ]),
        Line::from(vec![
            Span::raw("  "),
            dot("⚑n", Color::Yellow),
            Span::raw(t(" agentes rodando  ", " agents running  ")),
            dot("⚙n", Color::Yellow),
            Span::raw(t(" tasks rodando", " tasks running")),
        ]),
    ];
    // Sobre: versão no título; licença, autores e repositório na borda de baixo
    // (fica à vista mesmo quando a tela é baixa e a lista é cortada).
    let about = format!(" {} · {} · {} ", crate::update::LICENSE, crate::update::authors(), crate::update::REPO.trim_start_matches("https://"));
    f.render_widget(Clear, r);
    f.render_widget(
        Paragraph::new(Text::from(lines)).block(
            Block::bordered()
                .border_type(BorderType::Thick)
                .border_style(Style::new().fg(FOCUS))
                .title(tr!(" Ajuda · agent-dash v{} ", " Help · agent-dash v{} ", crate::update::VERSION))
                .title_bottom(Line::from(about).fg(Color::Gray))
                .title_alignment(Alignment::Center),
        ),
        r,
    );
}
