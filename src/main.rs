mod app;
mod config;
mod counts;
mod execmon;
mod janitor;
mod procs;
mod sessions;
mod sys;
mod transcript;
mod ui;

use std::time::{Duration, Instant};

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::widgets::TableState;

const TICK: Duration = Duration::from_secs(1);

fn main() -> anyhow::Result<()> {
    if std::env::args().any(|a| a == "--default-config") {
        print!("{}", config::TEMPLATE);
        return Ok(());
    }
    if std::env::args().any(|a| a == "--help" || a == "-h") {
        println!("clop — a btop-style view of Claude Code activity\n");
        println!("  clop                     run the TUI");
        println!("  clop --once              print what clop sees and exit");
        println!("  clop --cleanup-script    print a cleanup script for what ended sessions left behind");
        println!("  clop --default-config    print a commented config file");
        println!("\nconfig: {}", config::path().display());
        return Ok(());
    }
    config::load();
    if let Some(l) = config::loaded() {
        if l.status.starts_with("error") {
            eprintln!("clop: {}: {}", l.path.display(), l.status);
        }
    }
    if std::env::args().any(|a| a == "--once") {
        return once();
    }
    if std::env::args().any(|a| a == "--cleanup-script") {
        return cleanup_script();
    }
    let mut app = app::App::new();
    let mut terminal = ratatui::init();
    let mut tstate = TableState::default();
    let mut last = Instant::now();
    let res = (|| -> anyhow::Result<()> {
        loop {
            terminal.draw(|f| ui::draw(f, &app, &mut tstate))?;
            let timeout = TICK.saturating_sub(last.elapsed());
            if event::poll(timeout)? {
                if let Event::Key(k) = event::read()? {
                    if k.kind != KeyEventKind::Press {
                        continue;
                    }
                    match k.code {
                        KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                        KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => return Ok(()),
                        KeyCode::Char('j') | KeyCode::Down if app.panel == app::Panel::Janitor => {
                            let n = app.janitor_items().len();
                            if app.jan_sel + 1 < n {
                                app.jan_sel += 1;
                            }
                        }
                        KeyCode::Char('k') | KeyCode::Up if app.panel == app::Panel::Janitor => {
                            app.jan_sel = app.jan_sel.saturating_sub(1)
                        }
                        KeyCode::Char('j') | KeyCode::Down => {
                            if app.selected + 1 < app.sessions.len() {
                                app.selected += 1;
                            }
                        }
                        KeyCode::Char('k') | KeyCode::Up => app.selected = app.selected.saturating_sub(1),
                        KeyCode::Char('c') => app.panel = app::Panel::Janitor,
                        KeyCode::Char('l') if app.panel == app::Panel::Janitor => {
                            app.jan_show_live = !app.jan_show_live;
                            app.jan_sel = 0;
                        }
                        KeyCode::Char('r') if app.panel == app::Panel::Janitor => {
                            app.jan_rescan.store(true, std::sync::atomic::Ordering::SeqCst);
                            app.toast = Some(("rescanning…".into(), Instant::now()));
                        }
                        KeyCode::Char('y') if app.panel == app::Panel::Janitor => {
                            let items = app.janitor_items();
                            if let Some(it) = items.get(app.jan_sel) {
                                let cmd = it.cmd.clone();
                                app.yank(&cmd, "cleanup command");
                            }
                        }
                        KeyCode::Char('Y') if app.panel == app::Panel::Janitor => {
                            let items = app.janitor_items();
                            let ended: Vec<&janitor::Item> = items.iter().filter(|i| !i.in_use).collect();
                            let script = janitor::script(&ended, &app.host);
                            app.yank(&script, &format!("cleanup script ({} items)", ended.len()));
                        }
                        KeyCode::Char('a') => app.feed_all = !app.feed_all,
                        KeyCode::Tab => {
                            app.panel = match app.panel {
                                app::Panel::Activity => app::Panel::Programs,
                                app::Panel::Programs => app::Panel::Skills,
                                app::Panel::Skills => app::Panel::Janitor,
                                app::Panel::Janitor => app::Panel::Activity,
                            }
                        }
                        KeyCode::Char('p') => app.panel = app::Panel::Programs,
                        KeyCode::Char('s') => app.panel = app::Panel::Skills,
                        KeyCode::Char('f') => app.panel = app::Panel::Activity,
                        KeyCode::Char('w') => app.window = app.window.next(),
                        _ => {}
                    }
                }
            }
            if last.elapsed() >= TICK {
                app.tick();
                last = Instant::now();
            }
        }
    })();
    ratatui::restore();
    app.save();
    res
}

/// Non-interactive dump for checking what clop sees.
fn once() -> anyhow::Result<()> {
    let mut app = app::App::new();
    std::thread::sleep(Duration::from_millis(1100));
    app.tick();
    std::thread::sleep(Duration::from_millis(300));
    for s in &app.sessions {
        let t = app.totals.get(&s.reg.pid).cloned().unwrap_or_default();
        println!(
            "{} pid={} cwd={} status={:?} model={:?} ctx={} cpu={:.1}% rss={} procs={} events={} subagents={} spinning={} idle={:?}",
            s.reg.name.as_deref().unwrap_or("?"),
            s.reg.pid,
            s.reg.cwd,
            s.reg.status,
            s.log.model,
            s.log.ctx_tokens,
            t.cpu,
            sys::human(t.rss),
            t.nprocs,
            s.log.events.len(),
            s.subagents.len(),
            s.log.spinning(),
            t.idle,
        );
        for e in s.log.events.iter().rev().take(5) {
            println!("   {} {:<6} {:<16} {}{}", e.at.format("%H:%M"), e.tool, e.verb, if e.risky { "⚠ " } else { "" }, e.detail);
        }
    }
    println!("cpu={:.0}% mem={}/{}", app.sys.cpu, sys::human(app.sys.mem_used), sys::human(app.sys.mem_total));
    println!("exec monitor: {}", if app.exec_ok { "listening" } else { "unavailable" });
    if let Some(l) = config::loaded() {
        println!("config: {} ({})", l.path.display(), l.status);
    }
    for _ in 0..600 {
        if app.store.lock().map(|s| s.indexed).unwrap_or(true) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let s = app.store.lock().unwrap();
    let top = |v: Vec<(String, u64)>| v.into_iter().take(12).map(|(k, n)| format!("{k}={n}")).collect::<Vec<_>>().join(" ");
    println!("asked (all): {}", top(s.totals(None, |d| &d.asked)));
    println!("ran (all):   {}", top(s.totals(None, |d| &d.ran)));
    println!("skills:      {}", top(s.totals(None, |d| &d.skills)));
    println!("slash:       {}", top(s.totals(None, |d| &d.slash)));
    println!("tools:       {}", top(s.totals(None, |d| &d.tools)));
    println!("first day: {:?}  files indexed: {}", s.first_day(), s.offsets.len());
    drop(s);
    for _ in 0..600 {
        if app.jan.lock().map(|j| j.scanned.is_some()).unwrap_or(true) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let items = app.janitor_items();
    println!("janitor: {} items from ended sessions", items.len());
    for it in &items {
        println!("  {:<9} {:<60} {:>8} {} | {}", it.kind.label(), it.what, it.size.map(sys::human).unwrap_or_default(), it.note, it.cmd.lines().last().unwrap_or(""));
    }
    app.save();
    Ok(())
}

/// Print the cleanup script for everything ended sessions left behind.
fn cleanup_script() -> anyhow::Result<()> {
    let app = app::App::new();
    for _ in 0..600 {
        if app.jan.lock().map(|j| j.scanned.is_some()).unwrap_or(true) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let items = app.janitor_items();
    let ended: Vec<&janitor::Item> = items.iter().filter(|i| !i.in_use).collect();
    print!("{}", janitor::script(&ended, &app.host));
    Ok(())
}
