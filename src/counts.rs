//! Persistent all-time counters: programs Claude asked for (from transcripts),
//! programs that actually ran (from kernel exec events), skills and slash
//! commands. Stored per local day in ~/.local/state/clop/counts.json.

use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Local, NaiveDate};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::transcript::programs;

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct Day {
    #[serde(default)]
    pub asked: HashMap<String, u64>,
    #[serde(default)]
    pub ran: HashMap<String, u64>,
    #[serde(default)]
    pub skills: HashMap<String, u64>,
    #[serde(default)]
    pub slash: HashMap<String, u64>,
    #[serde(default)]
    pub tools: HashMap<String, u64>,
    /// Exec events whose process was gone before we could read its name.
    #[serde(default)]
    pub missed: u64,
}

#[derive(Serialize, Deserialize, Default)]
pub struct Store {
    /// Transcript path → bytes already counted, so restarts never double count.
    #[serde(default)]
    pub offsets: HashMap<String, u64>,
    #[serde(default)]
    pub days: BTreeMap<String, Day>,
    #[serde(skip)]
    pub dirty: bool,
    #[serde(skip)]
    pub indexed: bool,
}

pub type Shared = Arc<Mutex<Store>>;

/// Shell builtins that aren't worth counting as programs.
const SKIP: [&str; 9] = ["cd", "export", "source", ".", "set", "unset", "local", "return", "exit"];

/// Only one clop per machine may count: two would both see every exec and
/// both index the same transcript lines. The first takes this lock and
/// becomes the counter; others just re-read the file to display it.
pub fn try_lead() -> Option<File> {
    use std::os::fd::AsRawFd;
    let p = state_path().with_file_name("counter.lock");
    if let Some(d) = p.parent() {
        let _ = fs::create_dir_all(d);
    }
    let f = fs::OpenOptions::new().create(true).truncate(false).write(true).open(p).ok()?;
    (unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0).then_some(f)
}

fn state_path() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".local/state"));
    base.join("clop").join("counts.json")
}

pub fn day_key(t: DateTime<Local>) -> String {
    t.format("%Y-%m-%d").to_string()
}

impl Store {
    pub fn load() -> Self {
        fs::read(state_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn save(&mut self) {
        if !self.dirty {
            return;
        }
        let p = state_path();
        if let Some(d) = p.parent() {
            let _ = fs::create_dir_all(d);
        }
        let tmp = p.with_extension("json.tmp");
        if let Ok(b) = serde_json::to_vec(self) {
            if fs::write(&tmp, b).is_ok() && fs::rename(&tmp, &p).is_ok() {
                self.dirty = false;
            }
        }
    }

    pub fn day(&mut self, key: &str) -> &mut Day {
        self.dirty = true;
        self.days.entry(key.to_string()).or_default()
    }

    /// Sum one category over the last `days` days (None = all time).
    pub fn totals(&self, days: Option<i64>, pick: impl Fn(&Day) -> &HashMap<String, u64>) -> Vec<(String, u64)> {
        let cutoff = days.map(|d| day_key(Local::now() - chrono::Duration::days(d - 1)));
        let mut m: HashMap<String, u64> = HashMap::new();
        for (k, d) in &self.days {
            if cutoff.as_ref().is_some_and(|c| k < c) {
                continue;
            }
            for (name, n) in pick(d) {
                *m.entry(name.clone()).or_default() += n;
            }
        }
        let mut v: Vec<_> = m.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        v
    }

    /// Per-day counts for one name over the last `n` days, oldest first.
    pub fn history(&self, n: i64, name: &str, pick: impl Fn(&Day) -> &HashMap<String, u64>) -> Vec<u64> {
        let today = Local::now().date_naive();
        (0..n)
            .rev()
            .map(|i| {
                let k = (today - chrono::Duration::days(i)).format("%Y-%m-%d").to_string();
                self.days.get(&k).and_then(|d| pick(d).get(name).copied()).unwrap_or(0)
            })
            .collect()
    }

    pub fn missed(&self, days: Option<i64>) -> u64 {
        let cutoff = days.map(|d| day_key(Local::now() - chrono::Duration::days(d - 1)));
        self.days.iter().filter(|(k, _)| cutoff.as_ref().is_none_or(|c| *k >= c)).map(|(_, d)| d.missed).sum()
    }

    pub fn first_day(&self) -> Option<NaiveDate> {
        self.days.keys().next().and_then(|k| NaiveDate::parse_from_str(k, "%Y-%m-%d").ok())
    }
}

/// Background thread: every few seconds, count new transcript lines across
/// every project (including subagents), from the stored offsets onward.
pub fn spawn_indexer(store: Shared) {
    std::thread::spawn(move || loop {
        let root = dirs::home_dir().unwrap_or_default().join(".claude/projects");
        let mut files = Vec::new();
        collect_jsonl(&root, &mut files, 0);
        let offsets: HashMap<String, u64> = store.lock().map(|s| s.offsets.clone()).unwrap_or_default();
        for f in &files {
            let key = f.to_string_lossy().into_owned();
            let Ok(len) = fs::metadata(f).map(|m| m.len()) else { continue };
            let off = offsets.get(&key).copied().unwrap_or(0);
            if len == off {
                continue;
            }
            if len < off {
                // Rewritten file: don't recount what we've seen, just resync.
                if let Ok(mut s) = store.lock() {
                    s.offsets.insert(key, len);
                    s.dirty = true;
                }
                continue;
            }
            let (consumed, delta) = count_file(f, off);
            if consumed == 0 {
                continue;
            }
            if let Ok(mut s) = store.lock() {
                for (day, d) in delta {
                    let dst = s.day(&day);
                    merge(&mut dst.asked, d.asked);
                    merge(&mut dst.skills, d.skills);
                    merge(&mut dst.slash, d.slash);
                    merge(&mut dst.tools, d.tools);
                }
                s.offsets.insert(key, off + consumed);
                s.dirty = true;
            }
        }
        if let Ok(mut s) = store.lock() {
            // Forget offsets for transcripts that no longer exist.
            let live: std::collections::HashSet<String> = files.iter().map(|f| f.to_string_lossy().into_owned()).collect();
            let before = s.offsets.len();
            s.offsets.retain(|k, _| live.contains(k));
            if s.offsets.len() != before {
                s.dirty = true;
            }
            s.indexed = true;
        }
        std::thread::sleep(Duration::from_secs(5));
    });
}

fn merge(dst: &mut HashMap<String, u64>, src: HashMap<String, u64>) {
    for (k, v) in src {
        *dst.entry(k).or_default() += v;
    }
}

fn collect_jsonl(dir: &Path, out: &mut Vec<PathBuf>, depth: u32) {
    if depth > 4 {
        return;
    }
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_dir() {
            collect_jsonl(&p, out, depth + 1);
        } else if p.extension().and_then(|x| x.to_str()) == Some("jsonl") {
            out.push(p);
        }
    }
}

/// Count complete lines from `off`; returns bytes consumed and per-day deltas.
fn count_file(path: &Path, off: u64) -> (u64, HashMap<String, Day>) {
    let mut delta: HashMap<String, Day> = HashMap::new();
    let Ok(mut f) = File::open(path) else { return (0, delta) };
    if f.seek(SeekFrom::Start(off)).is_err() {
        return (0, delta);
    }
    let mut buf = Vec::new();
    if f.read_to_end(&mut buf).is_err() {
        return (0, delta);
    }
    let Some(last_nl) = buf.iter().rposition(|c| *c == b'\n') else { return (0, delta) };
    for line in buf[..last_nl].split(|c| *c == b'\n') {
        let Ok(v) = serde_json::from_slice::<Value>(line) else { continue };
        count_line(&v, &mut delta);
    }
    ((last_nl + 1) as u64, delta)
}

fn count_line(v: &Value, delta: &mut HashMap<String, Day>) {
    let at = v
        .get("timestamp")
        .and_then(|t| t.as_str())
        .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
        .map(|t| t.with_timezone(&Local));
    let Some(at) = at else { return };
    let day = delta.entry(day_key(at)).or_default();
    match v.get("type").and_then(|t| t.as_str()) {
        Some("assistant") => {
            let Some(parts) = v["message"]["content"].as_array() else { return };
            for p in parts.iter().filter(|p| p["type"] == "tool_use") {
                let name = p["name"].as_str().unwrap_or("?");
                let tool = if name.starts_with("mcp__") {
                    format!("mcp:{}", name.trim_start_matches("mcp__").split("__").next().unwrap_or(""))
                } else {
                    name.to_string()
                };
                *day.tools.entry(tool).or_default() += 1;
                match name {
                    "Bash" => {
                        for prog in programs(p["input"]["command"].as_str().unwrap_or("")) {
                            if !SKIP.contains(&prog.as_str()) {
                                *day.asked.entry(prog).or_default() += 1;
                            }
                        }
                    }
                    "Skill" => {
                        if let Some(s) = p["input"]["skill"].as_str() {
                            *day.skills.entry(s.to_string()).or_default() += 1;
                        }
                    }
                    _ => {}
                }
            }
        }
        Some("user") => {
            let text = match &v["message"]["content"] {
                Value::String(s) => s.clone(),
                Value::Array(a) => a.iter().filter_map(|x| x["text"].as_str()).collect::<Vec<_>>().join(" "),
                _ => return,
            };
            if let Some(i) = text.find("<command-name>") {
                let rest = &text[i + "<command-name>".len()..];
                if let Some(j) = rest.find("</command-name>") {
                    let cmd = rest[..j].trim().trim_start_matches('/');
                    if !cmd.is_empty() {
                        *day.slash.entry(cmd.to_string()).or_default() += 1;
                    }
                }
            }
        }
        _ => {}
    }
}

/// Skills installed on this machine, by the name Claude invokes them with.
pub fn installed_skills() -> Vec<String> {
    let claude = dirs::home_dir().unwrap_or_default().join(".claude");
    let mut out = Vec::new();
    let names_in = |dir: &Path| -> Vec<String> {
        fs::read_dir(dir)
            .map(|rd| {
                rd.flatten()
                    .filter(|e| e.path().join("SKILL.md").exists())
                    .filter_map(|e| e.file_name().to_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    };
    out.extend(names_in(&claude.join("skills")));
    // Skills synced from claude.ai show up as anthropic-skills:<name>.
    if let Ok(rd) = fs::read_dir(claude.join("skills/synced")) {
        for e in rd.flatten() {
            out.extend(names_in(&e.path()).into_iter().map(|n| format!("anthropic-skills:{n}")));
        }
    }
    let ip: Value = fs::read(claude.join("plugins/installed_plugins.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    if let Some(plugins) = ip["plugins"].as_object() {
        for (key, installs) in plugins {
            let plugin = key.split('@').next().unwrap_or(key);
            for inst in installs.as_array().into_iter().flatten() {
                if let Some(path) = inst["installPath"].as_str() {
                    out.extend(names_in(&Path::new(path).join("skills")).into_iter().map(|n| format!("{plugin}:{n}")));
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out
}
