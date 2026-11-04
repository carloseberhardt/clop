//! Live Claude Code sessions: the `~/.claude/sessions/<pid>.json` registry,
//! their transcripts, subagents, MCP server names, and disk footprint.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use serde::Deserialize;

use crate::transcript::{SessionLog, Tail};

#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Reg {
    pub pid: i32,
    pub session_id: String,
    pub cwd: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub started_at: Option<u64>,
    #[serde(default)]
    pub status_updated_at: Option<u64>,
    #[serde(default)]
    pub proc_start: Option<String>,
}

pub struct Subagent {
    pub id: String,
    pub agent_type: String,
    pub description: String,
    pub tail: Tail,
    pub modified: SystemTime,
    pub tools: u32,
}

impl Subagent {
    pub fn active(&self) -> bool {
        self.modified.elapsed().map(|d| d < Duration::from_secs(90)).unwrap_or(false)
    }
}

pub struct Session {
    pub reg: Reg,
    pub dir: Option<PathBuf>,
    pub tail: Option<Tail>,
    pub log: SessionLog,
    pub subagents: Vec<Subagent>,
    /// command line → MCP server name, from ~/.claude.json and <cwd>/.mcp.json.
    pub mcp: Vec<(String, Vec<String>, String)>,
}

pub fn claude_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_default().join(".claude")
}

/// `/home/me/my.app` → `-home-me-my-app`
pub fn mangle(cwd: &str) -> String {
    cwd.chars().map(|c| if c.is_alphanumeric() || c == '-' { c } else { '-' }).collect()
}

pub fn read_registry() -> Vec<Reg> {
    let mut out = Vec::new();
    let Ok(dir) = fs::read_dir(claude_dir().join("sessions")) else { return out };
    for e in dir.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        if let Some(r) = fs::read(&p).ok().and_then(|b| serde_json::from_slice::<Reg>(&b).ok()) {
            out.push(r);
        }
    }
    out
}

impl Session {
    pub fn new(reg: Reg) -> Self {
        let projects = claude_dir().join("projects");
        let file = format!("{}.jsonl", reg.session_id);
        let mut transcript = projects.join(mangle(&reg.cwd)).join(&file);
        if !transcript.exists() {
            // Mangling rules may differ; find it by session id.
            if let Ok(d) = fs::read_dir(&projects) {
                for e in d.flatten() {
                    let c = e.path().join(&file);
                    if c.exists() {
                        transcript = c;
                        break;
                    }
                }
            }
        }
        let dir = transcript.parent().map(|p| p.join(&reg.session_id));
        let tail = transcript.exists().then(|| Tail::new(transcript));
        let mcp = load_mcp(&reg.cwd);
        Self { reg, dir, tail, log: SessionLog::default(), subagents: Vec::new(), mcp }
    }

    pub fn transcript(&self) -> Option<&Path> {
        self.tail.as_ref().map(|t| t.path.as_path())
    }

    pub fn refresh(&mut self) {
        // The transcript may not exist yet when a session first starts.
        if self.tail.is_none() {
            let s = Session::new(self.reg.clone());
            self.tail = s.tail;
            self.dir = s.dir;
        }
        let cwd = self.reg.cwd.clone();
        if let Some(t) = self.tail.as_mut() {
            for v in t.read_new() {
                self.log.ingest(&v, &cwd, None);
            }
        }
        self.refresh_subagents();
    }

    fn refresh_subagents(&mut self) {
        let Some(dir) = self.dir.as_ref().map(|d| d.join("subagents")) else { return };
        let Ok(rd) = fs::read_dir(&dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            let Some(name) = p.file_name().and_then(|n| n.to_str()) else { continue };
            let Some(id) = name.strip_suffix(".jsonl") else { continue };
            let modified = e.metadata().and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
            if let Some(sa) = self.subagents.iter_mut().find(|s| s.id == id) {
                sa.modified = modified;
                continue;
            }
            let meta: serde_json::Value = fs::read(dir.join(format!("{id}.meta.json")))
                .ok()
                .and_then(|b| serde_json::from_slice(&b).ok())
                .unwrap_or_default();
            let txt = |k: &str| meta.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
            self.subagents.push(Subagent {
                id: id.to_string(),
                agent_type: txt("agentType"),
                description: txt("description"),
                tail: Tail::new(p.clone()),
                modified,
                tools: 0,
            });
        }
        let cwd = self.reg.cwd.clone();
        for sa in &mut self.subagents {
            let label = if sa.agent_type.is_empty() { "agent".to_string() } else { sa.agent_type.clone() };
            for v in sa.tail.read_new() {
                let before = self.log.events.len();
                self.log.ingest(&v, &cwd, Some(&label));
                sa.tools += self.log.events.len().saturating_sub(before) as u32;
            }
        }
    }

    pub fn mcp_name(&self, cmdline: &[String]) -> Option<String> {
        if cmdline.is_empty() {
            return None;
        }
        for (cmd, args, name) in &self.mcp {
            let base = cmd.rsplit('/').next().unwrap_or(cmd);
            let cmd_hit = cmdline.iter().any(|a| a.rsplit('/').next() == Some(base));
            let args_hit = args.iter().filter(|a| !a.starts_with('-')).all(|a| cmdline.iter().any(|c| c.contains(a.as_str())));
            if cmd_hit && args_hit {
                return Some(name.clone());
            }
        }
        // Heuristic for plugin/unknown servers: an argument mentioning "mcp".
        cmdline.iter().skip(1).find(|a| a.to_lowercase().contains("mcp") && !a.starts_with('-')).map(|a| {
            let b = a.rsplit('/').next().unwrap_or(a);
            let b = b.trim_start_matches('@');
            let b = b.split('@').next().unwrap_or(b);
            b.trim_end_matches(".js").trim_end_matches(".py").to_string()
        })
    }
}

fn load_mcp(cwd: &str) -> Vec<(String, Vec<String>, String)> {
    let mut out = Vec::new();
    let mut add = |servers: &serde_json::Value| {
        if let Some(m) = servers.as_object() {
            for (name, cfg) in m {
                if let Some(cmd) = cfg.get("command").and_then(|c| c.as_str()) {
                    let args = cfg
                        .get("args")
                        .and_then(|a| a.as_array())
                        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
                        .unwrap_or_default();
                    out.push((cmd.to_string(), args, name.clone()));
                }
            }
        }
    };
    let home = dirs::home_dir().unwrap_or_default();
    if let Some(cfg) = fs::read(home.join(".claude.json")).ok().and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok()) {
        add(&cfg["mcpServers"]);
        add(&cfg["projects"][cwd]["mcpServers"]);
    }
    if let Some(cfg) = fs::read(Path::new(cwd).join(".mcp.json")).ok().and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok()) {
        add(&cfg["mcpServers"]);
    }
    out
}

/// Per-session disk footprint, computed off the UI thread.
#[derive(Default, Clone, Copy)]
pub struct Footprint {
    pub transcript: u64,
    pub scratch: u64,
    pub history: u64,
}

impl Footprint {
    pub fn total(&self) -> u64 {
        self.transcript + self.scratch + self.history
    }
}

pub type Footprints = Arc<Mutex<HashMap<String, Footprint>>>;

/// Paths to measure for a session: (transcript file, session dir, scratchpad, file-history).
pub fn footprint_paths(s: &Session) -> (Option<PathBuf>, Option<PathBuf>, PathBuf, PathBuf) {
    let uid = unsafe { libc::getuid() };
    let scratch = PathBuf::from(format!("/tmp/claude-{uid}")).join(mangle(&s.reg.cwd)).join(&s.reg.session_id);
    let hist = claude_dir().join("file-history").join(&s.reg.session_id);
    (s.transcript().map(Path::to_path_buf), s.dir.clone(), scratch, hist)
}

pub fn du(p: &Path) -> u64 {
    let Ok(m) = fs::symlink_metadata(p) else { return 0 };
    if m.is_file() {
        return m.len();
    }
    if !m.is_dir() {
        return 0;
    }
    let mut total = 0;
    let mut stack = vec![p.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let Ok(m) = e.metadata() else { continue };
            if m.is_dir() {
                stack.push(e.path());
            } else {
                total += m.len();
            }
        }
    }
    total
}
