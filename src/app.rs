use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::counts::{self, Shared, Store};
use crate::execmon::{self, SessionPids};
use crate::janitor;
use crate::procs::{self, Proc, Scanner};
use crate::sessions::{self, Footprint, Footprints, Session};
use crate::sys::{Sys, SysReader};

const HIST: usize = 120;
/// A child subtree with no CPU for this long counts as idle.
pub const IDLE_AFTER: Duration = Duration::from_secs(5 * 60);
/// How long a finished process lingers, fading, in the tree (from settings).
pub fn ghost_ttl() -> Duration {
    Duration::from_secs(crate::config::get().processes.fade_seconds)
}

#[derive(Clone, Copy, PartialEq)]
pub enum Panel {
    Activity,
    Programs,
    Skills,
    Janitor,
}

#[derive(Clone, Copy, PartialEq)]
pub enum Window {
    Today,
    Week,
    All,
}

impl Window {
    pub fn days(self) -> Option<i64> {
        match self {
            Window::Today => Some(1),
            Window::Week => Some(7),
            Window::All => None,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Window::Today => "today",
            Window::Week => "7 days",
            Window::All => "all time",
        }
    }
    pub fn next(self) -> Self {
        match self {
            Window::Today => Window::Week,
            Window::Week => Window::All,
            Window::All => Window::Today,
        }
    }
}

#[derive(Default, Clone)]
pub struct Totals {
    pub cpu: f64,
    pub rss: u64,
    pub nprocs: usize,
    /// Topmost pids of idle child subtrees, with how long they've been idle.
    pub idle: Vec<(i32, Duration)>,
}

pub struct App {
    scanner: Scanner,
    sysr: SysReader,
    pub host: String,
    pub sessions: Vec<Session>,
    pub procs: HashMap<i32, Proc>,
    /// Live processes plus recently finished ones, for drawing the tree.
    pub tree_procs: HashMap<i32, Proc>,
    pub tree_kids: HashMap<i32, Vec<i32>>,
    /// Finished processes still shown in the tree, fading out.
    pub ghosts: HashMap<i32, (Proc, Instant)>,
    counting: Arc<AtomicBool>,
    recent: execmon::Recent,
    pub kids: HashMap<i32, Vec<i32>>,
    pub owner: HashMap<i32, i32>,
    pub totals: HashMap<i32, Totals>,
    pub sys: Sys,
    pub cpu_hist: VecDeque<f64>,
    /// Claude's process trees as % of the whole machine, per tick.
    pub claude_hist: VecDeque<f64>,
    pub claude_rss: u64,
    /// ~/.claude plus /tmp/claude-<uid>, measured with the footprints.
    pub claude_dirs: Arc<Mutex<u64>>,
    pub sess_hist: HashMap<i32, VecDeque<f64>>,
    pub selected: usize,
    pub feed_all: bool,
    pub footprints: Footprints,
    fp_busy: Arc<AtomicBool>,
    fp_last: Option<Instant>,
    pub panel: Panel,
    pub window: Window,
    pub store: Shared,
    session_pids: SessionPids,
    pub exec_ok: bool,
    /// Holding the counter lock: this instance counts and saves.
    lead: Option<std::fs::File>,
    pub installed_skills: Vec<String>,
    pub jan: janitor::Shared,
    jan_live: janitor::LiveIds,
    pub jan_rescan: Arc<AtomicBool>,
    pub jan_sel: usize,
    pub jan_show_live: bool,
    pub toast: Option<(String, Instant)>,
    saved_at: Instant,
}

impl App {
    pub fn new() -> Self {
        let host = fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default().trim().split('.').next().unwrap_or("").to_string();
        let mut a = Self {
            scanner: Scanner::new(),
            sysr: SysReader::new(),
            host,
            sessions: Vec::new(),
            procs: HashMap::new(),
            tree_procs: HashMap::new(),
            tree_kids: HashMap::new(),
            ghosts: HashMap::new(),
            counting: Arc::new(AtomicBool::new(false)),
            recent: Arc::new(Mutex::new(VecDeque::new())),
            kids: HashMap::new(),
            owner: HashMap::new(),
            totals: HashMap::new(),
            sys: Sys::default(),
            cpu_hist: VecDeque::new(),
            claude_hist: VecDeque::new(),
            claude_rss: 0,
            claude_dirs: Arc::new(Mutex::new(0)),
            sess_hist: HashMap::new(),
            selected: 0,
            feed_all: true,
            footprints: Arc::new(Mutex::new(HashMap::new())),
            fp_busy: Arc::new(AtomicBool::new(false)),
            fp_last: None,
            panel: Panel::Activity,
            window: Window::Today,
            store: Arc::new(Mutex::new(Store::load())),
            session_pids: Arc::new(Mutex::new(HashSet::new())),
            exec_ok: false,
            lead: None,
            installed_skills: counts::installed_skills(),
            jan: Arc::new(Mutex::new(janitor::View::default())),
            jan_live: Arc::new(Mutex::new(HashSet::new())),
            jan_rescan: Arc::new(AtomicBool::new(false)),
            jan_sel: 0,
            jan_show_live: false,
            toast: None,
            saved_at: Instant::now(),
        };
        a.exec_ok = execmon::spawn(a.store.clone(), a.session_pids.clone(), a.counting.clone(), a.recent.clone());
        a.try_lead();
        janitor::spawn(a.jan.clone(), a.jan_live.clone(), a.jan_rescan.clone());
        a.tick();
        a
    }

    pub fn tick(&mut self) {
        let old = std::mem::take(&mut self.procs);
        self.procs = self.scanner.scan();
        // Session processes that ended since the last scan become ghosts.
        let now = Instant::now();
        for (pid, p) in old {
            if !self.procs.contains_key(&pid) && self.owner.contains_key(&pid) {
                let mut g = p;
                g.cpu = 0.0;
                self.ghosts.insert(pid, (g, now));
            }
        }

        // Registry entries whose process is alive and really is that process
        // (procStart guards against pid reuse after a crash).
        let live: Vec<sessions::Reg> = sessions::read_registry()
            .into_iter()
            .filter(|r| {
                self.procs.get(&r.pid).is_some_and(|p| {
                    r.proc_start.as_deref().map(|s| s == p.start.to_string()).unwrap_or(true)
                })
            })
            .collect();
        let live_ids: HashSet<&str> = live.iter().map(|r| r.session_id.as_str()).collect();
        self.sessions.retain(|s| live_ids.contains(s.reg.session_id.as_str()));
        for r in live {
            match self.sessions.iter_mut().find(|s| s.reg.session_id == r.session_id) {
                Some(s) => s.reg = r,
                None => self.sessions.push(Session::new(r)),
            }
        }
        self.sessions.sort_by_key(|s| s.reg.started_at.unwrap_or(0));
        if self.selected >= self.sessions.len() {
            self.selected = self.sessions.len().saturating_sub(1);
        }
        for s in &mut self.sessions {
            s.refresh();
        }

        let spids: HashSet<i32> = self.sessions.iter().map(|s| s.reg.pid).collect();
        if let Ok(mut sp) = self.session_pids.lock() {
            *sp = spids.clone();
        }
        if let Ok(mut l) = self.jan_live.lock() {
            *l = self.sessions.iter().map(|s| s.reg.session_id.clone()).collect();
        }
        self.owner = procs::attribute(&self.procs, &spids);
        self.kids = procs::children(&self.procs);
        self.update_ghosts(&spids);
        self.compute_totals();

        self.sys = self.sysr.read();
        push(&mut self.cpu_hist, self.sys.cpu);
        let claude_cores: f64 = self.totals.values().map(|t| t.cpu).sum();
        let share = if self.sys.ncpu > 0 { claude_cores / self.sys.ncpu as f64 } else { 0.0 };
        push(&mut self.claude_hist, share.min(self.sys.cpu));
        self.claude_rss = self.totals.values().map(|t| t.rss).sum();
        self.sess_hist.retain(|pid, _| spids.contains(pid));
        for s in &self.sessions {
            let cpu = self.totals.get(&s.reg.pid).map(|t| t.cpu).unwrap_or(0.0);
            push(self.sess_hist.entry(s.reg.pid).or_default(), cpu);
        }

        self.maybe_measure_disk();
        if self.lead.is_some() {
            if self.saved_at.elapsed() >= Duration::from_secs(30) {
                self.save();
            }
        } else if self.saved_at.elapsed() >= Duration::from_secs(10) {
            // Another clop is counting; show its numbers, and take over if it quit.
            self.saved_at = Instant::now();
            if !self.try_lead() {
                if let Ok(mut s) = self.store.lock() {
                    let mut fresh = Store::load();
                    fresh.indexed = true;
                    *s = fresh;
                }
            }
        }
    }

    pub fn is_counting(&self) -> bool {
        self.lead.is_some()
    }

    fn try_lead(&mut self) -> bool {
        let Some(lock) = counts::try_lead() else { return false };
        self.lead = Some(lock);
        if let Ok(mut s) = self.store.lock() {
            *s = Store::load();
        }
        self.counting.store(true, Ordering::SeqCst);
        counts::spawn_indexer(self.store.clone());
        true
    }

    pub fn save(&mut self) {
        self.saved_at = Instant::now();
        if self.lead.is_none() {
            return;
        }
        if let Ok(mut s) = self.store.lock() {
            s.save();
        }
    }

    /// Fold in programs the exec monitor saw start (most finish before any
    /// scan sees them), drop expired ghosts, and build the tree's view.
    fn update_ghosts(&mut self, spids: &HashSet<i32>) {
        let now = Instant::now();
        let execs: Vec<execmon::Exec> = self.recent.lock().map(|mut r| r.drain(..).collect()).unwrap_or_default();
        for e in execs {
            if self.procs.contains_key(&e.pid) {
                continue;
            }
            let g = Proc {
                pid: e.pid,
                ppid: e.ppid,
                comm: e.name,
                state: 'X',
                start: 0,
                rss: 0,
                cmdline: e.args,
                claude_pid: None,
                cpu: 0.0,
                last_active: e.at,
                first_seen: e.at,
            };
            // A pid can exec several times (sh → grep); keep the latest program.
            self.ghosts.insert(e.pid, (g, e.at));
        }
        self.ghosts.retain(|pid, (_, gone)| now.duration_since(*gone) < ghost_ttl() && !self.procs.contains_key(pid));
        // Ghosts must still hang off a session to be drawn.
        let mut tree = self.procs.clone();
        for (pid, (g, _)) in &self.ghosts {
            tree.entry(*pid).or_insert_with(|| g.clone());
        }
        let reaches_session = |mut pid: i32, tree: &HashMap<i32, Proc>| {
            for _ in 0..64 {
                let Some(p) = tree.get(&pid) else { return false };
                if spids.contains(&p.ppid) {
                    return true;
                }
                pid = p.ppid;
            }
            false
        };
        let keep: HashSet<i32> = self.ghosts.keys().copied().filter(|p| reaches_session(*p, &tree)).collect();
        tree.retain(|pid, _| self.procs.contains_key(pid) || keep.contains(pid));
        self.tree_kids = procs::children(&tree);
        self.tree_procs = tree;
    }

    pub fn ghost_age(&self, pid: i32) -> Option<Duration> {
        self.ghosts.get(&pid).filter(|_| !self.procs.contains_key(&pid)).map(|(_, t)| t.elapsed())
    }

    fn compute_totals(&mut self) {
        let now = Instant::now();
        let mut totals: HashMap<i32, Totals> = HashMap::new();
        for s in &self.sessions {
            let t = totals.entry(s.reg.pid).or_default();
            if let Some(p) = self.procs.get(&s.reg.pid) {
                t.cpu += p.cpu;
                t.rss += p.rss;
                t.nprocs += 1;
            }
        }
        for (pid, spid) in &self.owner {
            if let (Some(p), Some(t)) = (self.procs.get(pid), totals.get_mut(spid)) {
                t.cpu += p.cpu;
                t.rss += p.rss;
                t.nprocs += 1;
            }
        }
        // Idle subtrees: direct children of claude (not MCP servers, which idle
        // by design) whose whole subtree has been CPU-quiet for IDLE_AFTER.
        for s in &self.sessions {
            let Some(t) = totals.get_mut(&s.reg.pid) else { continue };
            for &c in self.kids.get(&s.reg.pid).map(Vec::as_slice).unwrap_or(&[]) {
                let Some(p) = self.procs.get(&c) else { continue };
                if s.mcp_name(&p.cmdline).is_some() {
                    continue;
                }
                let last = self.subtree_last_active(c);
                let quiet = now.duration_since(last);
                let watched = now.duration_since(p.first_seen);
                if quiet >= IDLE_AFTER && watched >= IDLE_AFTER {
                    t.idle.push((c, quiet));
                }
            }
        }
        self.totals = totals;
    }

    fn subtree_last_active(&self, pid: i32) -> Instant {
        let mut best = self.procs.get(&pid).map(|p| p.last_active).unwrap_or_else(Instant::now);
        let mut stack = vec![pid];
        while let Some(p) = stack.pop() {
            for &k in self.kids.get(&p).map(Vec::as_slice).unwrap_or(&[]) {
                if let Some(q) = self.procs.get(&k) {
                    best = best.max(q.last_active);
                }
                stack.push(k);
            }
        }
        best
    }

    fn maybe_measure_disk(&mut self) {
        let due = self.fp_last.map(|t| t.elapsed() >= Duration::from_secs(30)).unwrap_or(true);
        if !due || self.fp_busy.swap(true, Ordering::SeqCst) {
            return;
        }
        self.fp_last = Some(Instant::now());
        let jobs: Vec<_> = self.sessions.iter().map(|s| (s.reg.session_id.clone(), sessions::footprint_paths(s))).collect();
        let out = self.footprints.clone();
        let busy = self.fp_busy.clone();
        let dirs_out = self.claude_dirs.clone();
        std::thread::spawn(move || {
            let uid = unsafe { libc::getuid() };
            let dirs = sessions::du(&sessions::claude_dir()) + sessions::du(std::path::Path::new(&format!("/tmp/claude-{uid}")));
            *dirs_out.lock().unwrap() = dirs;
            let mut m = HashMap::new();
            for (id, (tr, dir, scratch, hist)) in jobs {
                let transcript = tr.map(|p| sessions::du(&p)).unwrap_or(0) + dir.map(|p| sessions::du(&p)).unwrap_or(0);
                m.insert(id, Footprint { transcript, scratch: sessions::du(&scratch), history: sessions::du(&hist) });
            }
            *out.lock().unwrap() = m;
            busy.store(false, Ordering::SeqCst);
        });
    }

    /// Janitor rows as shown: orphans first, then by kind and size; items
    /// from live sessions only when toggled on.
    pub fn janitor_items(&self) -> Vec<janitor::Item> {
        let mut v = janitor::orphans(self);
        if let Ok(j) = self.jan.lock() {
            v.extend(j.items.iter().cloned());
        }
        v.retain(|i| self.jan_show_live || !i.in_use);
        v.sort_by(|a, b| {
            a.in_use
                .cmp(&b.in_use)
                .then(a.kind.cmp(&b.kind))
                .then(a.what.ends_with("(prune)").cmp(&b.what.ends_with("(prune)")))
                .then(b.size.unwrap_or(0).cmp(&a.size.unwrap_or(0)))
        });
        v
    }

    /// Put text on the clipboard of the terminal we're drawn in (works over ssh).
    pub fn yank(&mut self, text: &str, what: &str) {
        use std::io::Write;
        let seq = format!("\x1b]52;c;{}\x07", base64(text.as_bytes()));
        let mut out = std::io::stdout();
        let _ = out.write_all(seq.as_bytes());
        let _ = out.flush();
        self.toast = Some((format!("copied {what} to clipboard"), Instant::now()));
    }

    pub fn selected_session(&self) -> Option<&Session> {
        self.sessions.get(self.selected)
    }

    pub fn session_index(&self, pid: i32) -> usize {
        self.sessions.iter().position(|s| s.reg.pid == pid).unwrap_or(0)
    }
}

fn push(h: &mut VecDeque<f64>, v: f64) {
    h.push_back(v);
    while h.len() > HIST {
        h.pop_front();
    }
}

fn base64(b: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::with_capacity(b.len().div_ceil(3) * 4);
    for c in b.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= c.len() {
                s.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                s.push('=');
            }
        }
    }
    s
}

#[cfg(test)]
mod tests {
    #[test]
    fn b64() {
        assert_eq!(super::base64(b"hello"), "aGVsbG8=");
        assert_eq!(super::base64(b"hi"), "aGk=");
        assert_eq!(super::base64(b"abc"), "YWJj");
    }
}
