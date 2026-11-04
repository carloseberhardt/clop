//! Machine-wide stats: CPU, memory, swap, disks.

use std::collections::HashSet;
use std::ffi::CString;
use std::fs;
use std::os::unix::fs::MetadataExt;

#[derive(Default, Clone)]
pub struct Sys {
    pub cpu: f64,
    pub ncpu: usize,
    /// Busy % per logical CPU, in /proc/stat order.
    pub per_cpu: Vec<f64>,
    pub load: [f64; 3],
    pub mem_total: u64,
    pub mem_used: u64,
    pub swap_total: u64,
    pub swap_used: u64,
    pub disks: Vec<Disk>,
}

#[derive(Clone)]
pub struct Disk {
    pub label: String,
    pub used: u64,
    /// Space available to us (excludes ext4's root-reserved blocks).
    pub avail: u64,
}

pub struct SysReader {
    prev: Option<(u64, u64)>,
    prev_per: Vec<(u64, u64)>,
    paths: Vec<(String, String)>,
}

impl SysReader {
    pub fn new() -> Self {
        let home = dirs::home_dir().map(|h| h.to_string_lossy().into_owned()).unwrap_or_default();
        Self {
            prev: None,
            prev_per: Vec::new(),
            paths: vec![("/".into(), "/".into()), ("~".into(), home), ("/tmp".into(), "/tmp".into())],
        }
    }

    pub fn read(&mut self) -> Sys {
        let mut s = Sys::default();
        if let Ok(stat) = fs::read_to_string("/proc/stat") {
            let mut per = Vec::new();
            for line in stat.lines().filter(|l| l.starts_with("cpu")) {
                let v: Vec<u64> = line.split_whitespace().skip(1).filter_map(|x| x.parse().ok()).collect();
                let total: u64 = v.iter().take(8).sum();
                let idle = v.get(3).unwrap_or(&0) + v.get(4).unwrap_or(&0);
                if line.starts_with("cpu ") {
                    if let Some((pt, pi)) = self.prev {
                        s.cpu = busy(total, idle, pt, pi);
                    }
                    self.prev = Some((total, idle));
                } else {
                    per.push((total, idle));
                }
            }
            s.ncpu = per.len();
            s.per_cpu = per
                .iter()
                .enumerate()
                .map(|(i, &(t, id))| self.prev_per.get(i).map(|&(pt, pi)| busy(t, id, pt, pi)).unwrap_or(0.0))
                .collect();
            self.prev_per = per;
        }
        if let Ok(la) = fs::read_to_string("/proc/loadavg") {
            for (i, v) in la.split_whitespace().take(3).enumerate() {
                s.load[i] = v.parse().unwrap_or(0.0);
            }
        }
        if let Ok(mi) = fs::read_to_string("/proc/meminfo") {
            let get = |k: &str| -> u64 {
                mi.lines()
                    .find(|l| l.starts_with(k))
                    .and_then(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok())
                    .unwrap_or(0)
                    * 1024
            };
            s.mem_total = get("MemTotal:");
            s.mem_used = s.mem_total.saturating_sub(get("MemAvailable:"));
            s.swap_total = get("SwapTotal:");
            s.swap_used = s.swap_total.saturating_sub(get("SwapFree:"));
        }
        let mut seen = HashSet::new();
        for (label, path) in &self.paths {
            let Ok(m) = fs::metadata(path) else { continue };
            if !seen.insert(m.dev()) {
                continue;
            }
            if let Some((used, avail)) = statvfs(path) {
                if s.disks.iter().any(|d| d.used == used && d.avail == avail) {
                    continue;
                }
                s.disks.push(Disk { label: label.clone(), used, avail });
            }
        }
        s
    }
}

fn busy(total: u64, idle: u64, pt: u64, pi: u64) -> f64 {
    let dt = total.saturating_sub(pt) as f64;
    let di = idle.saturating_sub(pi) as f64;
    if dt > 0.0 { (1.0 - di / dt) * 100.0 } else { 0.0 }
}

fn statvfs(path: &str) -> Option<(u64, u64)> {
    let c = CString::new(path).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    // Same arithmetic as df: used = blocks - bfree; Use% = used / (used + bavail).
    let bs = st.f_frsize as u64;
    let used = (st.f_blocks as u64).saturating_sub(st.f_bfree as u64) * bs;
    Some((used, st.f_bavail as u64 * bs))
}

pub fn human(b: u64) -> String {
    const U: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{b}B")
    } else if v < 10.0 {
        format!("{v:.1}{}", U[i])
    } else {
        format!("{v:.0}{}", U[i])
    }
}
