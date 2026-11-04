//! /proc scanner: every process we can see, with CPU% computed between scans
//! and the `CLAUDE_PID` env var that ties a process back to its session even
//! after it has been reparented to init.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::time::Instant;

/// `getconf CLK_TCK` and `getconf PAGESIZE` on both target machines.
const CLK_TCK: f64 = 100.0;
const PAGE: u64 = 4096;

#[derive(Clone, Debug)]
pub struct Proc {
    pub pid: i32,
    pub ppid: i32,
    pub comm: String,
    pub state: char,
    pub start: u64,
    pub rss: u64,
    pub cmdline: Vec<String>,
    pub claude_pid: Option<i32>,
    /// Percent of one core since the previous scan.
    pub cpu: f64,
    /// Last scan in which this process consumed any CPU.
    pub last_active: Instant,
    pub first_seen: Instant,
}

struct Prev {
    start: u64,
    ticks: u64,
    comm: String,
    cmdline: Vec<String>,
    claude_pid: Option<i32>,
    last_active: Instant,
    first_seen: Instant,
}

pub struct Scanner {
    uid: u32,
    prev: HashMap<i32, Prev>,
    last_scan: Option<Instant>,
}

impl Scanner {
    pub fn new() -> Self {
        Self { uid: unsafe { libc::getuid() }, prev: HashMap::new(), last_scan: None }
    }

    pub fn scan(&mut self) -> HashMap<i32, Proc> {
        let now = Instant::now();
        let dt = self.last_scan.map(|t| now.duration_since(t).as_secs_f64()).unwrap_or(0.0);
        self.last_scan = Some(now);

        let mut out = HashMap::new();
        let mut next_prev = HashMap::new();
        let Ok(dir) = fs::read_dir("/proc") else { return out };
        for ent in dir.flatten() {
            let Some(pid) = ent.file_name().to_str().and_then(|s| s.parse::<i32>().ok()) else {
                continue;
            };
            // Only our own processes: others' environ/cmdline aren't readable anyway,
            // and Claude never runs as another user.
            match ent.metadata() {
                Ok(m) if m.uid() == self.uid => {}
                _ => continue,
            }
            let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else { continue };
            let Some((comm, rest)) = parse_stat(&stat) else { continue };
            let f: Vec<&str> = rest.split_whitespace().collect();
            if f.len() < 22 {
                continue;
            }
            let state = f[0].chars().next().unwrap_or('?');
            let ppid = f[1].parse().unwrap_or(0);
            let ticks = f[11].parse::<u64>().unwrap_or(0) + f[12].parse::<u64>().unwrap_or(0);
            let start = f[19].parse().unwrap_or(0);
            let rss = f[21].parse::<u64>().unwrap_or(0) * PAGE;

            let old = self.prev.remove(&pid).filter(|p| p.start == start);
            let (cmdline, claude_pid, last_active, first_seen, cpu) = match old {
                Some(p) => {
                    let d = ticks.saturating_sub(p.ticks);
                    let cpu = if dt > 0.0 { d as f64 / CLK_TCK / dt * 100.0 } else { 0.0 };
                    let active = if d > 0 { now } else { p.last_active };
                    // comm changes on exec; re-read argv then.
                    let cmdline = if p.comm != comm { read_cmdline(pid) } else { p.cmdline };
                    (cmdline, p.claude_pid, active, p.first_seen, cpu)
                }
                None => (read_cmdline(pid), read_claude_pid(pid), now, now, 0.0),
            };
            next_prev.insert(
                pid,
                Prev {
                    start,
                    ticks,
                    comm: comm.clone(),
                    cmdline: cmdline.clone(),
                    claude_pid,
                    last_active,
                    first_seen,
                },
            );
            out.insert(
                pid,
                Proc { pid, ppid, comm, state, start, rss, cmdline, claude_pid, cpu, last_active, first_seen },
            );
        }
        self.prev = next_prev;
        out
    }
}

fn parse_stat(s: &str) -> Option<(String, &str)> {
    let open = s.find('(')?;
    let close = s.rfind(')')?;
    Some((s[open + 1..close].to_string(), s.get(close + 2..)?))
}

fn read_cmdline(pid: i32) -> Vec<String> {
    fs::read(format!("/proc/{pid}/cmdline"))
        .map(|b| {
            b.split(|c| *c == 0)
                .filter(|a| !a.is_empty())
                .map(|a| String::from_utf8_lossy(a).into_owned())
                .collect()
        })
        .unwrap_or_default()
}

fn read_claude_pid(pid: i32) -> Option<i32> {
    let env = fs::read(format!("/proc/{pid}/environ")).ok()?;
    env.split(|c| *c == 0)
        .find_map(|kv| kv.strip_prefix(b"CLAUDE_PID="))
        .and_then(|v| std::str::from_utf8(v).ok()?.parse().ok())
}

/// Which session (claude pid) each process belongs to: by ancestry first,
/// falling back to the inherited `CLAUDE_PID` env var.
pub fn attribute(procs: &HashMap<i32, Proc>, sessions: &HashSet<i32>) -> HashMap<i32, i32> {
    let mut owner = HashMap::new();
    for (&pid, p) in procs {
        if sessions.contains(&pid) {
            continue;
        }
        let mut cur = p.ppid;
        let mut hops = 0;
        let mut found = None;
        while cur > 1 && hops < 64 {
            if sessions.contains(&cur) {
                found = Some(cur);
                break;
            }
            cur = procs.get(&cur).map(|q| q.ppid).unwrap_or(0);
            hops += 1;
        }
        if let Some(s) = found.or(p.claude_pid.filter(|c| sessions.contains(c))) {
            owner.insert(pid, s);
        }
    }
    owner
}

/// Children map for drawing trees.
pub fn children(procs: &HashMap<i32, Proc>) -> HashMap<i32, Vec<i32>> {
    let mut kids: HashMap<i32, Vec<i32>> = HashMap::new();
    for p in procs.values() {
        kids.entry(p.ppid).or_default().push(p.pid);
    }
    for v in kids.values_mut() {
        v.sort();
    }
    kids
}
