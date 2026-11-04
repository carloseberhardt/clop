//! Kernel exec events via the netlink proc connector. Unlike polling /proc,
//! this sees every program start, including a 5 ms `grep`. Works unprivileged
//! on the kernels we target (verified on 7.2.11, Arch and Fedora).

use std::collections::{HashSet, VecDeque};
use std::fs;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use chrono::Local;

use crate::counts::{day_key, Shared};

const NETLINK_CONNECTOR: i32 = 11;
const CN_IDX_PROC: u32 = 1;
const CN_VAL_PROC: u32 = 1;
const PROC_CN_MCAST_LISTEN: u32 = 1;
const PROC_EVENT_EXEC: u32 = 2;
const NLMSG_HDR: usize = 16;
const CN_MSG: usize = 20;

pub type SessionPids = Arc<Mutex<HashSet<i32>>>;

/// A Claude-owned program start, kept briefly so the process tree can show
/// programs that finished between two scans.
#[derive(Clone)]
pub struct Exec {
    pub pid: i32,
    pub ppid: i32,
    pub name: String,
    pub args: Vec<String>,
    pub at: Instant,
}

pub type Recent = Arc<Mutex<VecDeque<Exec>>>;

/// Start listening; returns false if the kernel refused (the "ran" column
/// then stays empty and the UI says so).
/// `counting` is true only in the clop instance that owns the counters; every
/// instance still feeds `recent` for its own process tree.
pub fn spawn(store: Shared, sessions: SessionPids, counting: Arc<AtomicBool>, recent: Recent) -> bool {
    let fd = unsafe { libc::socket(libc::AF_NETLINK, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, NETLINK_CONNECTOR) };
    if fd < 0 {
        return false;
    }
    unsafe {
        let mut sa: libc::sockaddr_nl = std::mem::zeroed();
        sa.nl_family = libc::AF_NETLINK as u16;
        sa.nl_groups = CN_IDX_PROC;
        if libc::bind(fd, &sa as *const _ as *const libc::sockaddr, std::mem::size_of::<libc::sockaddr_nl>() as u32) < 0 {
            libc::close(fd);
            return false;
        }
        // Bigger receive buffer so bursts (a cargo build) don't overflow.
        let sz: i32 = 4 << 20;
        libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_RCVBUF, &sz as *const _ as *const libc::c_void, 4);

        let mut msg = Vec::with_capacity(NLMSG_HDR + CN_MSG + 4);
        msg.extend_from_slice(&((NLMSG_HDR + CN_MSG + 4) as u32).to_ne_bytes());
        msg.extend_from_slice(&(libc::NLMSG_DONE as u16).to_ne_bytes());
        msg.extend_from_slice(&0u16.to_ne_bytes());
        msg.extend_from_slice(&0u32.to_ne_bytes());
        msg.extend_from_slice(&0u32.to_ne_bytes());
        msg.extend_from_slice(&CN_IDX_PROC.to_ne_bytes());
        msg.extend_from_slice(&CN_VAL_PROC.to_ne_bytes());
        msg.extend_from_slice(&0u32.to_ne_bytes()); // seq
        msg.extend_from_slice(&0u32.to_ne_bytes()); // ack
        msg.extend_from_slice(&4u16.to_ne_bytes()); // len
        msg.extend_from_slice(&0u16.to_ne_bytes()); // flags
        msg.extend_from_slice(&PROC_CN_MCAST_LISTEN.to_ne_bytes());
        if libc::send(fd, msg.as_ptr() as *const libc::c_void, msg.len(), 0) < 0 {
            libc::close(fd);
            return false;
        }
    }
    std::thread::spawn(move || {
        let mut buf = vec![0u8; 65536];
        loop {
            let n = unsafe { libc::recv(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len(), 0) };
            if n < 0 {
                if std::io::Error::last_os_error().raw_os_error() == Some(libc::ENOBUFS) {
                    // Overflowed: events lost; count one miss and carry on.
                    if counting.load(Ordering::Relaxed) {
                        if let Ok(mut s) = store.lock() {
                            s.day(&day_key(Local::now())).missed += 1;
                        }
                    }
                    continue;
                }
                return;
            }
            let n = n as usize;
            let mut off = 0;
            while off + NLMSG_HDR <= n {
                let len = u32::from_ne_bytes(buf[off..off + 4].try_into().unwrap()) as usize;
                if len < NLMSG_HDR || off + len > n {
                    break;
                }
                let ev = off + NLMSG_HDR + CN_MSG;
                if ev + 24 <= off + len {
                    let what = u32::from_ne_bytes(buf[ev..ev + 4].try_into().unwrap());
                    if what == PROC_EVENT_EXEC {
                        let tgid = i32::from_ne_bytes(buf[ev + 20..ev + 24].try_into().unwrap());
                        handle_exec(tgid, &store, &sessions, &counting, &recent);
                    }
                }
                off += (len + 3) & !3;
            }
        }
    });
    true
}

fn handle_exec(pid: i32, store: &Shared, sessions: &SessionPids, counting: &AtomicBool, recent: &Recent) {
    let live = match sessions.lock() {
        Ok(s) if !s.is_empty() => s.clone(),
        _ => return,
    };
    let count = counting.load(Ordering::Relaxed);
    // One read of stat gives name and parent together; short-lived programs
    // (grep -q) are often gone a moment later.
    let Some((comm, ppid)) = read_stat(pid) else {
        if count {
            if let Ok(mut s) = store.lock() {
                s.day(&day_key(Local::now())).missed += 1;
            }
        }
        return;
    };
    if !belongs(pid, ppid, &live) {
        return;
    }
    let args: Vec<String> = fs::read(format!("/proc/{pid}/cmdline"))
        .map(|b| b.split(|c| *c == 0).filter(|a| !a.is_empty()).map(|a| String::from_utf8_lossy(a).into_owned()).collect())
        .unwrap_or_default();
    let name = display_name(pid, comm);
    if let Ok(mut r) = recent.lock() {
        r.push_back(Exec { pid, ppid, name: name.clone(), args, at: Instant::now() });
        while r.len() > 500 {
            r.pop_front();
        }
    }
    if count {
        if let Ok(mut s) = store.lock() {
            *s.day(&day_key(Local::now())).ran.entry(name).or_default() += 1;
        }
    }
}

fn read_stat(pid: i32) -> Option<(String, i32)> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let open = stat.find('(')?;
    let close = stat.rfind(')')?;
    let ppid = stat.get(close + 2..)?.split_whitespace().nth(1)?.parse().ok()?;
    Some((stat[open + 1..close].to_string(), ppid))
}

/// Claude Code's binary is named after its version (…/claude/versions/2.1.318),
/// and its shell snapshot runs `grep` and `find` through that same binary with
/// argv[0] set to ugrep / bfs. Report those by the command Claude typed.
fn display_name(pid: i32, comm: String) -> String {
    if !comm.chars().all(|c| c.is_ascii_digit() || c == '.') {
        return comm;
    }
    let argv0 = fs::read(format!("/proc/{pid}/cmdline"))
        .ok()
        .and_then(|b| b.split(|c| *c == 0).next().map(|a| String::from_utf8_lossy(a).into_owned()))
        .unwrap_or_default();
    match argv0.rsplit('/').next().unwrap_or("") {
        "ugrep" => "grep (ugrep)".into(),
        "bfs" => "find (bfs)".into(),
        "rg" => "rg (claude)".into(),
        _ => match fs::read_link(format!("/proc/{pid}/exe")) {
            Ok(exe) if exe.to_string_lossy().contains("/claude/versions/") => "claude".into(),
            _ => comm,
        },
    }
}

/// Does this process descend from a live session, or carry its CLAUDE_PID?
fn belongs(pid: i32, ppid: i32, live: &HashSet<i32>) -> bool {
    if live.contains(&ppid) {
        return true;
    }
    if let Ok(env) = fs::read(format!("/proc/{pid}/environ")) {
        let cp = env
            .split(|c| *c == 0)
            .find_map(|kv| kv.strip_prefix(b"CLAUDE_PID="))
            .and_then(|v| std::str::from_utf8(v).ok()?.parse::<i32>().ok());
        if cp.is_some_and(|c| live.contains(&c)) {
            return true;
        }
    }
    let mut cur = ppid;
    for _ in 0..64 {
        if cur <= 1 {
            return false;
        }
        if live.contains(&cur) {
            return true;
        }
        match read_stat(cur) {
            Some((_, p)) => cur = p,
            None => return false,
        }
    }
    false
}
