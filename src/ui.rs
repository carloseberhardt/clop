use std::time::Duration;

use chrono::Local;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Axis, Block, BorderType, Cell, Chart, Dataset, GraphType, Paragraph, Row, Table, TableState,
};
use ratatui::Frame;

use crate::app::{App, Panel};
use crate::counts::Day;
use crate::sessions::Session;
use crate::sys::human;

// btop-ish palette
const BORDER: Color = Color::Rgb(88, 91, 112);
const TITLE: Color = Color::Rgb(205, 214, 244);
const DIM: Color = Color::Rgb(127, 132, 156);
const TEXT: Color = Color::Rgb(205, 214, 244);
const GREEN: Color = Color::Rgb(166, 227, 161);
const YELLOW: Color = Color::Rgb(249, 226, 175);
const RED: Color = Color::Rgb(243, 139, 168);
const CLAUDE: Color = Color::Rgb(217, 119, 87);
const BLOCKS: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
const SEL: Color = Color::Rgb(49, 50, 68);
const SESSION_COLORS: [Color; 6] = [
    Color::Rgb(137, 220, 235),
    Color::Rgb(203, 166, 247),
    Color::Rgb(250, 179, 135),
    Color::Rgb(166, 227, 161),
    Color::Rgb(245, 194, 231),
    Color::Rgb(137, 180, 250),
];

fn scolor(i: usize) -> Color {
    SESSION_COLORS[i % SESSION_COLORS.len()]
}

fn block(title: &str) -> Block<'_> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(BORDER))
        .title(Line::from(vec![" ".into(), Span::styled(title, Style::new().fg(TITLE).bold()), " ".into()]))
}

/// Green → yellow → red by position along the bar, like btop's meters.
fn heat(frac: f64) -> Color {
    let f = frac.clamp(0.0, 1.0);
    let (a, b, t) = if f < 0.5 { (GREEN, YELLOW, f * 2.0) } else { (YELLOW, RED, (f - 0.5) * 2.0) };
    match (a, b) {
        (Color::Rgb(r1, g1, b1), Color::Rgb(r2, g2, b2)) => {
            let m = |x: u8, y: u8| (x as f64 + (y as f64 - x as f64) * t) as u8;
            Color::Rgb(m(r1, r2), m(g1, g2), m(b1, b2))
        }
        _ => a,
    }
}

fn bar(ratio: f64, width: usize) -> Vec<Span<'static>> {
    let filled = ((ratio.clamp(0.0, 1.0)) * width as f64).round() as usize;
    (0..width)
        .map(|i| {
            if i < filled {
                Span::styled("■", Style::new().fg(heat(i as f64 / width.max(1) as f64)))
            } else {
                Span::styled("■", Style::new().fg(Color::Rgb(49, 50, 68)))
            }
        })
        .collect()
}

fn ago(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", s / 60),
        _ => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
    }
}

fn tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1e6)
    } else {
        format!("{}k", n / 1000)
    }
}

fn trunc(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(n.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

fn label(s: &Session) -> String {
    let base = s.reg.cwd.rsplit('/').next().unwrap_or(&s.reg.cwd).to_string();
    base
}

pub fn draw(f: &mut Frame, app: &App, tstate: &mut TableState) {
    let area = f.area();
    let rows_needed = app
        .sessions
        .iter()
        .map(|s| 1 + s.subagents.iter().filter(|a| a.active()).count())
        .sum::<usize>() as u16;
    let [top, mid, bottom, foot] = Layout::vertical([
        Constraint::Length(5),
        Constraint::Length((rows_needed + 3).clamp(6, 14)),
        Constraint::Min(8),
        Constraint::Length(1),
    ])
    .areas(area);
    draw_header(f, app, top);
    let [sess, chart] = Layout::horizontal([Constraint::Percentage(62), Constraint::Percentage(38)]).areas(mid);
    draw_sessions(f, app, sess, tstate);
    draw_chart(f, app, chart);
    // The feed shares the row with the process tree; the counters get the full width.
    match app.panel {
        Panel::Activity => {
            let [feed, tree] = Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)]).areas(bottom);
            draw_feed(f, app, feed);
            draw_tree(f, app, tree);
        }
        Panel::Programs => draw_programs(f, app, bottom),
        Panel::Skills => draw_skills(f, app, bottom),
        Panel::Janitor => draw_janitor(f, app, bottom),
    }
    let k = |key: &'static str, what: String| vec![key.fg(YELLOW).bold(), format!(" {what}  ").fg(DIM)];
    let mut spans = vec![" ".into()];
    spans.extend(k("q", "quit".into()));
    spans.extend(k("tab", "panel (f feed · p programs · s skills · c cleanup)".into()));
    match app.panel {
        Panel::Janitor => {
            spans.extend(k("j/k", "select".into()));
            spans.extend(k("y", "copy command".into()));
            spans.extend(k("Y", "copy script".into()));
            spans.extend(k("l", if app.jan_show_live { "hide in-use" } else { "show in-use" }.into()));
            spans.extend(k("r", "rescan".into()));
        }
        Panel::Activity => {
            spans.extend(k("j/k", "session".into()));
            spans.extend(k("a", if app.feed_all { "feed: all" } else { "feed: selected" }.into()));
        }
        _ => {
            spans.extend(k("w", format!("window: {}", app.window.label())));
        }
    }
    if let Some((msg, at)) = &app.toast {
        if at.elapsed().as_secs() < 4 {
            spans.push(Span::styled(format!("  {msg}"), Style::new().fg(GREEN).bold()));
        }
    }
    let keys = Line::from(spans);
    f.render_widget(Paragraph::new(keys), foot);
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let title = format!("clop · {} · {} sessions · {}", app.host, app.sessions.len(), Local::now().format("%H:%M:%S"));
    let b = block(&title);
    let inner = b.inner(area);
    f.render_widget(b, area);
    let [cpu, mem, disk] =
        Layout::horizontal([Constraint::Percentage(40), Constraint::Percentage(30), Constraint::Percentage(30)])
            .spacing(2)
            .areas(inner);

    // CPU: per-thread bars on top, then a two-row history with Claude's share
    // stacked under the machine total; numbers on the right.
    let [cl, cg, cr] = Layout::horizontal([Constraint::Length(7), Constraint::Min(12), Constraint::Length(18)])
        .spacing(1)
        .areas(cpu);
    f.render_widget(
        Paragraph::new(vec![
            Line::from("cpu".fg(DIM)),
            Line::from(format!("{} thr", app.sys.ncpu).fg(DIM)),
            Line::from(""),
        ]),
        cl,
    );
    let [threads, hist] = Layout::vertical([Constraint::Length(1), Constraint::Length(2)]).areas(cg);
    let gap = app.sys.ncpu * 2 <= threads.width as usize;
    let per: Vec<Span> = app
        .sys
        .per_cpu
        .iter()
        .map(|&u| {
            let lvl = ((u / 100.0) * 8.0).round().clamp(0.0, 8.0) as usize;
            let ch = if lvl == 0 { '▁' } else { BLOCKS[lvl] };
            let style = if lvl == 0 { Style::new().fg(Color::Rgb(69, 71, 90)) } else { Style::new().fg(heat(u / 100.0)) };
            Span::styled(if gap { format!("{ch} ") } else { ch.to_string() }, style)
        })
        .collect();
    f.render_widget(Paragraph::new(Line::from(per)), threads);
    f.render_widget(StackedSpark { total: &app.cpu_hist, part: &app.claude_hist }, hist);
    let claude_now = app.claude_hist.back().copied().unwrap_or(0.0);
    f.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                format!("{:>3.0}%", app.sys.cpu).fg(heat(app.sys.cpu / 100.0)).bold(),
                " total".fg(DIM),
            ]),
            Line::from(vec![format!("{:>3.0}%", claude_now.ceil()).fg(CLAUDE).bold(), " claude".fg(DIM)]),
            Line::from(format!("ld {:.1} {:.1} {:.1}", app.sys.load[0], app.sys.load[1], app.sys.load[2]).fg(DIM)),
        ]),
        cr,
    );

    // Memory + swap meters.
    let meter = |name: &str, used: u64, total: u64, width: usize| -> Line<'static> {
        let r = if total > 0 { used as f64 / total as f64 } else { 0.0 };
        let mut v = vec![Span::styled(format!("{name:<5}"), Style::new().fg(DIM))];
        v.extend(bar(r, width));
        v.push(Span::styled(format!(" {}/{}", human(used), human(total)), Style::new().fg(TEXT)));
        Line::from(v)
    };
    let bw = (mem.width as usize).saturating_sub(18).max(4);
    f.render_widget(
        Paragraph::new(vec![
            meter("mem", app.sys.mem_used, app.sys.mem_total, bw),
            meter("swap", app.sys.swap_used, app.sys.swap_total, bw),
            claude_line("claude", app.claude_rss, "in RAM"),
        ]),
        mem,
    );
    let dw = (disk.width as usize).saturating_sub(21).max(4);
    let lines: Vec<Line> = app
        .sys
        .disks
        .iter()
        .take(2)
        .map(|d| {
            let r = d.used as f64 / (d.used + d.avail).max(1) as f64;
            let mut v = vec![Span::styled(format!("{:<5}", d.label), Style::new().fg(DIM))];
            v.extend(bar(r, dw));
            v.push(Span::styled(format!(" {:.0}% ", r * 100.0), Style::new().fg(heat(r)).bold()));
            v.push(Span::styled(format!("{} free", human(d.avail)), Style::new().fg(TEXT)));
            Line::from(v)
        })
        .collect();
    let mut lines = lines;
    while lines.len() < 2 {
        lines.push(Line::from(""));
    }
    lines.push(claude_line("claude", app.claude_dirs.lock().map(|d| *d).unwrap_or(0), "~/.claude + /tmp"));
    f.render_widget(Paragraph::new(lines), disk);
}

fn draw_sessions(f: &mut Frame, app: &App, area: Rect, tstate: &mut TableState) {
    let fps = app.footprints.lock().map(|m| m.clone()).unwrap_or_default();
    let mut rows = Vec::new();
    let mut sel_row = 0;
    for (i, s) in app.sessions.iter().enumerate() {
        if i == app.selected {
            sel_row = rows.len();
        }
        let t = app.totals.get(&s.reg.pid).cloned().unwrap_or_default();
        let status = s.reg.status.as_deref().unwrap_or("?");
        let since = s
            .reg
            .status_updated_at
            .map(|ms| {
                let now = Local::now().timestamp_millis() as u64;
                Duration::from_millis(now.saturating_sub(ms))
            })
            .unwrap_or_default();
        let (icon, state) = match status {
            "busy" => (Span::styled("●", Style::new().fg(GREEN)), Span::styled("working", Style::new().fg(GREEN))),
            "idle" => (Span::styled("○", Style::new().fg(DIM)), Span::styled(format!("idle {}", ago(since)), Style::new().fg(DIM))),
            other => (Span::styled("◐", Style::new().fg(YELLOW)), Span::styled(other.to_string(), Style::new().fg(YELLOW))),
        };
        let model = s.log.model.as_deref().map(|m| m.trim_start_matches("claude-").to_string()).unwrap_or_default();
        let disk = fps.get(&s.reg.session_id).map(|f| human(f.total())).unwrap_or_else(|| "…".into());
        let mut flags = Vec::new();
        if s.log.spinning() {
            flags.push(Span::styled("spinning? ", Style::new().fg(YELLOW).bold()));
        }
        if !t.idle.is_empty() {
            flags.push(Span::styled(format!("{} idle child ", t.idle.len()), Style::new().fg(YELLOW)));
        }
        let name = s.reg.name.clone().unwrap_or_else(|| label(s));
        rows.push(Row::new(vec![
            Cell::from(Line::from(vec![icon, " ".into(), Span::styled(trunc(&name, 24), Style::new().fg(scolor(i)).bold())])),
            Cell::from(trunc(&label(s), 20)).fg(TEXT),
            Cell::from(model).fg(DIM),
            Cell::from(Line::from(state)),
            Cell::from(tokens(s.log.ctx_tokens)).fg(TEXT),
            Cell::from(format!("{:>5.1}%", t.cpu)).fg(heat(t.cpu / 100.0)),
            Cell::from(human(t.rss)).fg(TEXT),
            Cell::from(format!("{}", t.nprocs)).fg(DIM),
            Cell::from(disk).fg(TEXT),
            Cell::from(Line::from(flags)),
        ]));
        for a in s.subagents.iter().filter(|a| a.active()) {
            let kind = if a.agent_type.is_empty() { "agent" } else { &a.agent_type };
            rows.push(Row::new(vec![
                Cell::from(Line::from(vec!["  └ ".fg(DIM), Span::styled(trunc(kind, 12), Style::new().fg(scolor(i)))])),
                Cell::from(trunc(&a.description, 30)).fg(DIM),
                Cell::from(""),
                Cell::from("working").fg(GREEN),
                Cell::from(""),
                Cell::from(""),
                Cell::from(""),
                Cell::from(""),
                Cell::from(""),
                Cell::from(format!("{} tools", a.tools)).fg(DIM),
            ]));
        }
    }
    if rows.is_empty() {
        f.render_widget(Paragraph::new("  no running Claude Code sessions".fg(DIM)).block(block("sessions")), area);
        return;
    }
    let widths = [
        Constraint::Length(27),
        Constraint::Length(21),
        Constraint::Length(10),
        Constraint::Length(10),
        Constraint::Length(6),
        Constraint::Length(7),
        Constraint::Length(6),
        Constraint::Length(5),
        Constraint::Length(6),
        Constraint::Min(8),
    ];
    let header = Row::new(["session", "project", "model", "state", "ctx", "cpu", "mem", "proc", "disk", ""])
        .style(Style::new().fg(DIM).add_modifier(Modifier::UNDERLINED));
    let table = Table::new(rows, widths)
        .header(header)
        .block(block("sessions"))
        .row_highlight_style(Style::new().bg(SEL));
    tstate.select(Some(sel_row));
    f.render_stateful_widget(table, area, tstate);
}

fn draw_chart(f: &mut Frame, app: &App, area: Rect) {
    let series: Vec<(usize, Vec<(f64, f64)>)> = app
        .sessions
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let h = app.sess_hist.get(&s.reg.pid);
            let pts = h
                .map(|h| {
                    let n = h.len();
                    h.iter().enumerate().map(|(j, v)| ((120 - n + j) as f64, *v)).collect()
                })
                .unwrap_or_default();
            (i, pts)
        })
        .collect();
    let ymax = series
        .iter()
        .flat_map(|(_, p)| p.iter().map(|(_, y)| *y))
        .fold(10.0_f64, f64::max)
        .ceil();
    let datasets: Vec<Dataset> = series
        .iter()
        .map(|(i, pts)| {
            Dataset::default()
                .marker(Marker::Braille)
                .graph_type(GraphType::Line)
                .style(Style::new().fg(scolor(*i)))
                .data(pts)
        })
        .collect();
    let chart = Chart::new(datasets)
        .block(block("claude cpu · 2 min"))
        .x_axis(Axis::default().bounds([0.0, 120.0]))
        .y_axis(
            Axis::default()
                .bounds([0.0, ymax])
                .labels(vec![Span::styled("0", Style::new().fg(DIM)), Span::styled(format!("{ymax:.0}%"), Style::new().fg(DIM))]),
        );
    f.render_widget(chart, area);
}

fn tool_color(tool: &str) -> Color {
    match tool {
        "Bash" => Color::Rgb(250, 179, 135),
        "Edit" | "Write" | "MultiEdit" | "NotebookEdit" => GREEN,
        "Read" | "Grep" | "Glob" => Color::Rgb(137, 180, 250),
        "Agent" => Color::Rgb(203, 166, 247),
        "Skill" => Color::Rgb(245, 194, 231),
        "mcp" => Color::Rgb(148, 226, 213),
        "you" => YELLOW,
        _ => TEXT,
    }
}

fn draw_feed(f: &mut Frame, app: &App, area: Rect) {
    let b = block(if app.feed_all { "activity · all sessions" } else { "activity · selected session" });
    let inner = b.inner(area);
    let mut evs: Vec<(usize, &crate::transcript::Event)> = Vec::new();
    for (i, s) in app.sessions.iter().enumerate() {
        if !app.feed_all && i != app.selected {
            continue;
        }
        evs.extend(s.log.events.iter().map(|e| (i, e)));
    }
    evs.sort_by_key(|(_, e)| std::cmp::Reverse(e.at));
    let w = inner.width as usize;
    let lines: Vec<Line> = evs
        .iter()
        .take(inner.height as usize)
        .map(|(i, e)| {
            let s = &app.sessions[*i];
            let who = match &e.agent {
                Some(a) => format!("↳{}", a),
                None => s.reg.name.clone().unwrap_or_else(|| label(s)),
            };
            let verb = if e.verb.is_empty() { String::new() } else { trunc(&e.verb, 16) };
            let used = 6 + 11 + 7 + 17;
            let mut spans = vec![
                Span::styled(e.at.format("%H:%M ").to_string(), Style::new().fg(DIM)),
                Span::styled(format!("{:<10} ", trunc(&who, 10)), Style::new().fg(scolor(*i))),
                Span::styled(format!("{:<6} ", trunc(&e.tool, 6)), Style::new().fg(tool_color(&e.tool))),
                Span::styled(format!("{verb:<16} "), Style::new().fg(TEXT).bold()),
            ];
            let detail = trunc(&e.detail, w.saturating_sub(used + 2));
            if e.risky {
                spans.push(Span::styled(format!("⚠ {detail}"), Style::new().fg(RED).bold()));
            } else {
                spans.push(Span::styled(detail, Style::new().fg(DIM)));
            }
            Line::from(spans)
        })
        .collect();
    f.render_widget(Paragraph::new(lines).block(b), area);
}

fn draw_tree(f: &mut Frame, app: &App, area: Rect) {
    let Some(s) = app.selected_session() else {
        f.render_widget(block("processes"), area);
        return;
    };
    let i = app.session_index(s.reg.pid);
    let title = format!("processes · {}", s.reg.name.clone().unwrap_or_else(|| label(s)));
    let b = block(&title);
    let inner = b.inner(area);
    let idle: std::collections::HashMap<i32, Duration> =
        app.totals.get(&s.reg.pid).map(|t| t.idle.iter().cloned().collect()).unwrap_or_default();
    let mut lines = Vec::new();
    let w = inner.width as usize;
    walk(app, s, s.reg.pid, String::new(), true, true, &idle, i, w, &mut lines);

    // Processes tied to this session by env var but no longer under it.
    let strays: Vec<i32> = app
        .owner
        .iter()
        .filter(|(pid, spid)| **spid == s.reg.pid && !descends(app, **pid, s.reg.pid))
        .map(|(pid, _)| *pid)
        .collect();
    // Only the roots of detached subtrees; their children draw beneath them.
    let mut strays: Vec<i32> = strays
        .iter()
        .copied()
        .filter(|p| app.procs.get(p).map(|q| !strays.contains(&q.ppid)).unwrap_or(false))
        .collect();
    strays.sort();
    if !strays.is_empty() {
        lines.push(Line::from("detached (reparented, still carry CLAUDE_PID):".fg(YELLOW)));
        for p in strays {
            walk(app, s, p, String::new(), true, true, &idle, i, w, &mut lines);
        }
    }
    f.render_widget(Paragraph::new(lines).block(b), area);
}

fn descends(app: &App, mut pid: i32, root: i32) -> bool {
    for _ in 0..64 {
        let Some(p) = app.procs.get(&pid) else { return false };
        if p.ppid == root {
            return true;
        }
        if p.ppid <= 1 {
            return false;
        }
        pid = p.ppid;
    }
    false
}

#[allow(clippy::too_many_arguments)]
fn walk(
    app: &App,
    s: &Session,
    pid: i32,
    prefix: String,
    last: bool,
    root: bool,
    idle: &std::collections::HashMap<i32, Duration>,
    ci: usize,
    w: usize,
    out: &mut Vec<Line<'static>>,
) {
    let Some(p) = app.tree_procs.get(&pid) else { return };
    let ghost = app.ghost_age(pid);
    let branch = if root { "" } else if last { "└─ " } else { "├─ " };
    let mcp = if root { None } else { s.mcp_name(&p.cmdline) };
    let name = match &mcp {
        Some(m) => format!("mcp:{m}"),
        None => p.comm.clone(),
    };
    let args = if mcp.is_some() {
        String::new()
    } else {
        p.cmdline.iter().skip(1).map(String::as_str).collect::<Vec<_>>().join(" ")
    };
    let stats = match ghost {
        Some(age) => format!("   ✕ {:>3}s ago", age.as_secs()),
        None => format!(" {:>5.1}% {:>5}", p.cpu, human(p.rss)),
    };
    let head = format!("{prefix}{branch}");
    let tag_len = if idle.contains_key(&pid) { 10 } else { 0 } + if p.state == 'Z' { 7 } else { 0 };
    let fixed = head.chars().count() + name.chars().count() + stats.chars().count() + 1 + tag_len;
    let room = w.saturating_sub(fixed);
    let args = trunc(&args, room);
    let pad = room.saturating_sub(args.chars().count());
    let name_style = if root {
        Style::new().fg(scolor(ci)).bold()
    } else if mcp.is_some() {
        Style::new().fg(tool_color("mcp")).bold()
    } else {
        Style::new().fg(TEXT).bold()
    };
    let mut spans = match ghost {
        // Finished: draw it fading from dim text toward the background.
        Some(age) => {
            let f = fade(age.as_secs_f64() / crate::app::ghost_ttl().as_secs_f64().max(1.0));
            vec![
                Span::styled(head, Style::new().fg(BORDER)),
                Span::styled(name, Style::new().fg(f)),
                Span::styled(format!(" {args}{}", " ".repeat(pad)), Style::new().fg(f)),
                Span::styled(stats, Style::new().fg(f)),
            ]
        }
        None => vec![
            Span::styled(head, Style::new().fg(BORDER)),
            Span::styled(name, name_style),
            Span::styled(format!(" {args}{}", " ".repeat(pad)), Style::new().fg(DIM)),
            Span::styled(stats, Style::new().fg(heat(p.cpu / 100.0))),
        ],
    };
    if let Some(d) = idle.get(&pid) {
        spans.push(Span::styled(format!(" idle {}", ago(*d)), Style::new().fg(YELLOW)));
    }
    if p.state == 'Z' {
        spans.push(Span::styled(" zombie", Style::new().fg(RED)));
    }
    out.push(Line::from(spans));
    let all = app.tree_kids.get(&pid).cloned().unwrap_or_default();
    // Live children always; finished ones only the most recent few, so a
    // build spawning hundreds of compilers doesn't push live processes away.
    const MAX_GHOSTS: usize = 6;
    let (mut ghosts, live): (Vec<i32>, Vec<i32>) = all.into_iter().partition(|k| app.ghost_age(*k).is_some());
    ghosts.sort_by_key(|k| app.ghost_age(*k));
    let hidden = ghosts.len().saturating_sub(MAX_GHOSTS);
    ghosts.truncate(MAX_GHOSTS);
    ghosts.sort();
    let mut kids = ghosts;
    kids.extend(live);
    let next_prefix = if root { String::new() } else { format!("{prefix}{}", if last { "   " } else { "│  " }) };
    let n = kids.len() + (hidden > 0) as usize;
    for (j, k) in kids.iter().enumerate() {
        walk(app, s, *k, next_prefix.clone(), j + 1 == n, false, idle, ci, w, out);
    }
    if hidden > 0 {
        out.push(Line::from(vec![
            Span::styled(format!("{next_prefix}└─ "), Style::new().fg(BORDER)),
            Span::styled(format!("… {hidden} more finished"), Style::new().fg(fade(0.4))),
        ]));
    }
}

const SPARK: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

fn spark(v: &[u64]) -> String {
    let max = v.iter().copied().max().unwrap_or(0);
    v.iter()
        .map(|&x| if x == 0 { ' ' } else { SPARK[((x as f64 / max as f64) * 7.0).round() as usize] })
        .collect()
}

/// One ranked list: name, heat bar scaled to the leader, count, 14-day sparkline.
fn ranked(
    f: &mut Frame,
    app: &App,
    area: Rect,
    title: &str,
    pick: fn(&Day) -> &std::collections::HashMap<String, u64>,
    footer: Option<Line<'static>>,
    accent: Color,
) {
    let b = block(title);
    let inner = b.inner(area);
    f.render_widget(b, area);
    let Ok(store) = app.store.lock() else { return };
    let rows = store.totals(app.window.days(), pick);
    let reserve = footer.is_some() as usize;
    let h = (inner.height as usize).saturating_sub(reserve);
    let max = rows.first().map(|r| r.1).unwrap_or(1).max(1);
    let total: u64 = rows.iter().map(|r| r.1).sum();
    let name_w = 16usize;
    let spark_w = 14usize;
    let bar_w = (inner.width as usize).saturating_sub(4 + name_w + 8 + spark_w + 3).clamp(3, 30);
    let mut lines: Vec<Line> = Vec::new();
    if rows.is_empty() {
        lines.push(Line::from(if store.indexed { "  nothing yet" } else { "  indexing transcripts…" }.fg(DIM)));
    }
    for (i, (name, n)) in rows.iter().take(h).enumerate() {
        let r = *n as f64 / max as f64;
        let filled = ((r * bar_w as f64).ceil() as usize).max(1);
        let hist = store.history(spark_w as i64, name, pick);
        lines.push(Line::from(vec![
            Span::styled(format!("{:>2} ", i + 1), Style::new().fg(DIM)),
            Span::styled(format!("{:<w$} ", trunc(name, name_w), w = name_w), Style::new().fg(TEXT).bold()),
            Span::styled("━".repeat(filled), Style::new().fg(heat(r * 0.8))),
            Span::styled("─".repeat(bar_w - filled.min(bar_w)), Style::new().fg(Color::Rgb(49, 50, 68))),
            Span::styled(format!(" {:>6} ", n), Style::new().fg(accent).bold()),
            Span::styled(spark(&hist), Style::new().fg(accent)),
        ]));
    }
    let _ = total;
    if let Some(ft) = footer {
        while lines.len() < h {
            lines.push(Line::from(""));
        }
        lines.push(ft);
    }
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_programs(f: &mut Frame, app: &App, area: Rect) {
    let [l, r] = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(area);
    let w = app.window.label();
    let since = app.store.lock().ok().and_then(|s| s.first_day()).map(|d| d.format("%b %-d").to_string());
    let asked_title = format!("asked · what Claude typed · {w}");
    let foot_l = since.map(|d| Line::from(format!(" from transcripts back to {d} · 14-day trend →").fg(DIM)));
    ranked(f, app, l, &asked_title, |d| &d.asked, foot_l, Color::Rgb(250, 179, 135));
    let ran_title = format!("ran · what actually exec'd · {w}");
    let missed = app.store.lock().map(|s| s.missed(app.window.days())).unwrap_or(0);
    let foot_r = if !app.is_counting() {
        Line::from(" another clop on this machine is counting · showing its saved numbers".fg(YELLOW))
    } else if app.exec_ok {
        Line::from(format!(" counted while clop runs · {missed} execs (any process) gone before they could be read").fg(DIM))
    } else {
        Line::from(" exec events unavailable on this kernel".fg(RED))
    };
    ranked(f, app, r, &ran_title, |d| &d.ran, Some(foot_r), Color::Rgb(137, 220, 235));
}

fn draw_skills(f: &mut Frame, app: &App, area: Rect) {
    let [l, m, r] =
        Layout::horizontal([Constraint::Percentage(36), Constraint::Percentage(32), Constraint::Percentage(32)]).areas(area);
    let w = app.window.label();
    ranked(f, app, l, &format!("skills Claude invoked · {w}"), |d| &d.skills, None, Color::Rgb(245, 194, 231));
    ranked(f, app, m, &format!("slash commands you typed · {w}"), |d| &d.slash, None, YELLOW);
    let used: std::collections::HashSet<String> = app
        .store
        .lock()
        .map(|s| {
            let mut u: std::collections::HashSet<String> = s.totals(app.window.days(), |d| &d.skills).into_iter().map(|x| x.0).collect();
            u.extend(s.totals(app.window.days(), |d| &d.slash).into_iter().map(|x| x.0));
            u
        })
        .unwrap_or_default();
    let unused: Vec<&String> = app
        .installed_skills
        .iter()
        .filter(|s| !used.contains(*s) && !used.contains(s.rsplit(':').next().unwrap_or(s)))
        .collect();
    let t = format!("installed, unused · {w} · {}/{}", unused.len(), app.installed_skills.len());
    let b = block(&t);
    let inner = b.inner(r);
    let lines: Vec<Line> = unused.iter().take(inner.height as usize).map(|s| Line::from(format!(" {s}").fg(DIM))).collect();
    f.render_widget(Paragraph::new(lines).block(b), r);
}

fn claude_line(name: &str, bytes: u64, what: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{name:<7}"), Style::new().fg(CLAUDE)),
        Span::styled(human(bytes), Style::new().fg(CLAUDE).bold()),
        Span::styled(format!(" {what}"), Style::new().fg(DIM)),
    ])
}

/// Two-row history: machine total in a dim color, Claude's share in the
/// Claude color stacked underneath it. Scales to the busiest recent sample.
struct StackedSpark<'a> {
    total: &'a std::collections::VecDeque<f64>,
    part: &'a std::collections::VecDeque<f64>,
}

impl ratatui::widgets::Widget for StackedSpark<'_> {
    fn render(self, area: Rect, buf: &mut ratatui::buffer::Buffer) {
        let w = area.width as usize;
        let n = self.total.len().min(w);
        let skip = self.total.len() - n;
        let max = self.total.iter().skip(skip).copied().fold(0.0_f64, f64::max);
        // Round the scale up to a step so it doesn't jitter every tick.
        let scale = [5.0, 10.0, 25.0, 50.0, 100.0].into_iter().find(|s| max <= *s).unwrap_or(100.0);
        let rows = area.height as usize;
        let levels = (rows * 8) as f64;
        for (i, (t, c)) in self.total.iter().skip(skip).zip(self.part.iter().skip(skip)).enumerate() {
            let x = area.x + (w - n + i) as u16;
            let tl = ((t / scale) * levels).round() as usize;
            let cl = (((c / scale) * levels).round() as usize).min(tl);
            for r in 0..rows {
                let lo = r * 8;
                let fill = tl.saturating_sub(lo).min(8);
                if fill == 0 {
                    continue;
                }
                // The part of this cell that is Claude's decides its color.
                let claude_fill = cl.saturating_sub(lo).min(8);
                let color = if claude_fill * 2 >= fill { CLAUDE } else { Color::Rgb(108, 112, 134) };
                let y = area.y + (rows - 1 - r) as u16;
                buf[(x, y)].set_char(BLOCKS[fill]).set_fg(color);
            }
        }
        let label = format!("{scale:.0}%");
        if area.width as usize > label.len() + 2 {
            buf.set_string(area.x, area.y, &label, Style::new().fg(Color::Rgb(69, 71, 90)));
        }
    }
}

fn kind_color(k: crate::janitor::Kind) -> Color {
    use crate::janitor::Kind::*;
    match k {
        Orphan => RED,
        Worktree => Color::Rgb(203, 166, 247),
        File => Color::Rgb(250, 179, 135),
        Scratch => Color::Rgb(137, 180, 250),
        Image | Container => Color::Rgb(148, 226, 213),
        Refs => YELLOW,
    }
}

fn draw_janitor(f: &mut Frame, app: &App, area: Rect) {
    let items = app.janitor_items();
    let (scanning, scanned) = app.jan.lock().map(|j| (j.scanning, j.scanned)).unwrap_or((false, None));
    let total: u64 = items.iter().filter(|i| !i.in_use).filter_map(|i| i.size).sum();
    let ended = items.iter().filter(|i| !i.in_use).count();
    let status = match (scanned, scanning) {
        (None, _) => "scanning transcripts…".to_string(),
        (Some(_), true) => "rescanning…".to_string(),
        (Some(t), false) => format!("scanned {} ago", ago(t.elapsed().unwrap_or_default())),
    };
    let title = format!("cleanup · {ended} left behind by ended sessions · {} · {status}", human(total));
    let b = block(&title);
    let inner = b.inner(area);
    f.render_widget(b, area);
    if items.is_empty() {
        let msg = if scanned.is_some() { "  nothing left behind" } else { "  scanning…" };
        f.render_widget(Paragraph::new(msg.fg(DIM)), inner);
        return;
    }
    // Detail line for the selected item: its cleanup command.
    let [list, detail] = Layout::vertical([Constraint::Min(3), Constraint::Length(2)]).areas(inner);
    let sel = app.jan_sel.min(items.len() - 1);
    let h = list.height as usize;
    let start = sel.saturating_sub(h.saturating_sub(1));
    let w = list.width as usize;
    let what_w = w.saturating_sub(10 + 16 + 8 + 9 + 30).max(20);
    let lines: Vec<Line> = items
        .iter()
        .enumerate()
        .skip(start)
        .take(h)
        .map(|(i, it)| {
            let dim = it.in_use;
            let fg = |c: Color| if dim { DIM } else { c };
            let age = it.since.and_then(|t| t.elapsed().ok()).map(ago).unwrap_or_default();
            let mut note = it.note.clone();
            if it.in_use {
                note = if note.is_empty() { "in use".into() } else { format!("in use · {note}") };
            }
            let mut l = Line::from(vec![
                Span::styled(format!(" {:<10}", it.kind.label()), Style::new().fg(fg(kind_color(it.kind))).bold()),
                Span::styled(format!("{:<ww$} ", trunc(&it.what, what_w), ww = what_w), Style::new().fg(fg(TEXT))),
                Span::styled(format!("{:<15} ", trunc(&it.project, 15)), Style::new().fg(DIM)),
                Span::styled(format!("{:>7} ", it.size.map(human).unwrap_or_default()), Style::new().fg(fg(TEXT))),
                Span::styled(format!("{:>8} ", age), Style::new().fg(DIM)),
                Span::styled(note, Style::new().fg(if it.review { YELLOW } else { DIM })),
            ]);
            if i == sel {
                l = l.style(Style::new().bg(SEL));
            }
            l
        })
        .collect();
    f.render_widget(Paragraph::new(lines), list);
    let cmd = items[sel].cmd.replace('\n', " ⏎ ");
    let cfg_line = crate::config::loaded()
        .map(|l| {
            let home = dirs::home_dir().unwrap_or_default().to_string_lossy().into_owned();
            let p = l.path.to_string_lossy().replacen(&home, "~", 1);
            let color = if l.status.starts_with("error") { RED } else { DIM };
            Line::from(vec![Span::styled(format!(" settings: {p} · {}", l.status), Style::new().fg(color))])
        })
        .unwrap_or_default();
    f.render_widget(
        Paragraph::new(vec![
            cfg_line,
            Line::from(vec![" $ ".fg(GREEN).bold(), Span::styled(trunc(&cmd, w.saturating_sub(4)), Style::new().fg(TEXT))]),
        ]),
        detail,
    );
}

/// Text color for a finished process, `t` = 0 just ended … 1 about to vanish.
fn fade(t: f64) -> Color {
    let t = t.clamp(0.0, 1.0);
    let (a, b) = ((166.0, 173.0, 200.0), (55.0, 57.0, 72.0));
    let m = |x: f64, y: f64| (x + (y - x) * t) as u8;
    Color::Rgb(m(a.0, b.0), m(a.1, b.1), m(a.2, b.2))
}
