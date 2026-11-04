//! Incremental reader for Claude Code session transcripts (JSONL), turning
//! assistant tool calls into activity events.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;

use chrono::{DateTime, Local};
use serde_json::Value;

const KEEP: usize = 400;

#[derive(Clone, Debug)]
pub struct Event {
    pub at: DateTime<Local>,
    /// Tool name as shown: "Bash", "Edit", "mcp", "you", …
    pub tool: String,
    /// Short category: program names for Bash, server for MCP, agent type…
    pub verb: String,
    /// Specific detail: the Bash description, a file path, a pattern…
    pub detail: String,
    pub risky: bool,
    /// Subagent label when the event came from a subagent transcript.
    pub agent: Option<String>,
}

pub struct Tail {
    pub path: PathBuf,
    offset: u64,
    partial: Vec<u8>,
}

impl Tail {
    pub fn new(path: PathBuf) -> Self {
        Self { path, offset: 0, partial: Vec::new() }
    }

    /// New complete lines since last call, parsed as JSON.
    pub fn read_new(&mut self) -> Vec<Value> {
        let Ok(mut f) = File::open(&self.path) else { return vec![] };
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        if len < self.offset {
            // Truncated/rewritten: start over.
            self.offset = 0;
            self.partial.clear();
        }
        if len == self.offset || f.seek(SeekFrom::Start(self.offset)).is_err() {
            return vec![];
        }
        let mut buf = Vec::new();
        if f.read_to_end(&mut buf).is_err() {
            return vec![];
        }
        self.offset += buf.len() as u64;
        self.partial.extend_from_slice(&buf);
        let mut out = Vec::new();
        let mut start = 0;
        for i in 0..self.partial.len() {
            if self.partial[i] == b'\n' {
                if let Ok(v) = serde_json::from_slice::<Value>(&self.partial[start..i]) {
                    out.push(v);
                }
                start = i + 1;
            }
        }
        self.partial.drain(..start);
        out
    }
}

/// Everything clop tracks per session that comes from its transcript.
#[derive(Default)]
pub struct SessionLog {
    pub model: Option<String>,
    pub title: Option<String>,
    pub ctx_tokens: u64,
    pub events: VecDeque<Event>,
    /// Tool calls since the last edit or user prompt.
    pub tools_since_progress: u32,
    pub progress_at: Option<DateTime<Local>>,
}

impl SessionLog {
    pub fn ingest(&mut self, v: &Value, cwd: &str, agent: Option<&str>) {
        let at = v
            .get("timestamp")
            .and_then(|t| t.as_str())
            .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
            .map(|t| t.with_timezone(&Local));
        match v.get("type").and_then(|t| t.as_str()) {
            Some("ai-title") if agent.is_none() => {
                self.title = v.get("aiTitle").and_then(|t| t.as_str()).map(String::from);
            }
            Some("assistant") => {
                let msg = &v["message"];
                if agent.is_none() {
                    if let Some(m) = msg.get("model").and_then(|m| m.as_str()) {
                        if !m.starts_with('<') {
                            self.model = Some(m.to_string());
                        }
                    }
                    let u = &msg["usage"];
                    let n = |k: &str| u.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
                    let ctx = n("input_tokens") + n("cache_creation_input_tokens") + n("cache_read_input_tokens");
                    if ctx > 0 {
                        self.ctx_tokens = ctx;
                    }
                }
                let Some(parts) = msg.get("content").and_then(|c| c.as_array()) else { return };
                for p in parts.iter().filter(|p| p["type"] == "tool_use") {
                    let name = p["name"].as_str().unwrap_or("?");
                    let ev = describe(name, &p["input"], cwd, at, agent);
                    if agent.is_none() {
                        if matches!(name, "Edit" | "Write" | "NotebookEdit" | "MultiEdit") {
                            self.tools_since_progress = 0;
                            self.progress_at = Some(ev.at);
                        } else {
                            self.tools_since_progress += 1;
                        }
                    }
                    self.push(ev);
                }
            }
            Some("user") if agent.is_none() => {
                if let Some(text) = v["message"]["content"].as_str() {
                    let t = text.trim();
                    if t.is_empty() || t.starts_with('<') {
                        return;
                    }
                    self.tools_since_progress = 0;
                    let at = at.unwrap_or_else(Local::now);
                    self.progress_at = Some(at);
                    self.push(Event {
                        at,
                        tool: "you".into(),
                        verb: String::new(),
                        detail: one_line(t),
                        risky: false,
                        agent: None,
                    });
                }
            }
            _ => {}
        }
    }

    fn push(&mut self, e: Event) {
        self.events.push_back(e);
        while self.events.len() > KEEP {
            self.events.pop_front();
        }
    }

    /// "Spinning?": a long run of tool calls with no edits since the user last spoke.
    pub fn spinning(&self) -> bool {
        let long = self
            .progress_at
            .map(|t| (Local::now() - t).num_minutes() >= 15)
            .unwrap_or(false);
        self.tools_since_progress >= 30 && long
    }
}

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn short_path(p: &str, cwd: &str) -> String {
    if let Some(r) = p.strip_prefix(cwd).and_then(|r| r.strip_prefix('/')) {
        return r.to_string();
    }
    if let Some(h) = dirs::home_dir() {
        if let Some(r) = p.strip_prefix(h.to_string_lossy().as_ref()) {
            return format!("~{r}");
        }
    }
    p.to_string()
}

fn describe(name: &str, input: &Value, cwd: &str, at: Option<DateTime<Local>>, agent: Option<&str>) -> Event {
    let s = |k: &str| input.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
    let at = at.unwrap_or_else(Local::now);
    let agent = agent.map(String::from);
    let (tool, verb, detail, risky) = match name {
        "Bash" => {
            let cmd = s("command");
            let mut progs: Vec<String> = Vec::new();
            for p in programs(&cmd) {
                if p != "cd" && !progs.contains(&p) {
                    progs.push(p);
                }
            }
            let verb = match progs.len() {
                0 => String::new(),
                1..=3 => progs.join("|"),
                _ => format!("{}|…", progs[..3].join("|")),
            };
            let desc = s("description");
            let detail = if desc.is_empty() { one_line(&cmd) } else { desc };
            ("Bash".to_string(), verb, detail, is_risky(&cmd))
        }
        "Read" | "Edit" | "Write" | "MultiEdit" | "NotebookEdit" => {
            let p = if input.get("file_path").is_some() { s("file_path") } else { s("notebook_path") };
            (name.to_string(), String::new(), short_path(&p, cwd), false)
        }
        "Grep" | "Glob" => (name.to_string(), String::new(), s("pattern"), false),
        "Agent" | "Task" => ("Agent".to_string(), s("subagent_type"), s("description"), false),
        "Skill" => ("Skill".to_string(), s("skill"), s("args"), false),
        "WebFetch" => ("WebFetch".to_string(), String::new(), s("url"), false),
        "WebSearch" => ("WebSearch".to_string(), String::new(), s("query"), false),
        n if n.starts_with("mcp__") => {
            let mut it = n.trim_start_matches("mcp__").splitn(2, "__");
            let server = it.next().unwrap_or("").to_string();
            let t = it.next().unwrap_or("").to_string();
            ("mcp".to_string(), server, t, false)
        }
        n => (n.to_string(), String::new(), String::new(), false),
    };
    Event { at, tool, verb, detail, risky, agent }
}

/// Program names a shell command line runs, in order: `cd x && rg foo | head`
/// → ["cd", "rg", "head"]. Skips env assignments and wrappers like sudo/timeout.
/// Drop heredoc bodies so their lines aren't mistaken for commands.
fn strip_heredocs(cmd: &str) -> String {
    let mut out = String::new();
    let mut delim: Option<String> = None;
    for line in cmd.lines() {
        if let Some(d) = &delim {
            if line.trim() == d {
                delim = None;
            }
            continue;
        }
        out.push_str(line);
        out.push('\n');
        if let Some(i) = line.find("<<") {
            let rest = line[i + 2..].trim_start_matches(['-', '~']).trim_start();
            let word: String = rest
                .trim_start_matches(['\'', '"'])
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if !word.is_empty() && !rest.starts_with('<') {
                delim = Some(word);
            }
        }
    }
    out
}

pub fn programs(cmd: &str) -> Vec<String> {
    let cmd = strip_heredocs(cmd);
    let mut out = Vec::new();
    // Split on control operators, outside of quotes.
    let mut segs = Vec::new();
    let mut cur = String::new();
    let (mut sq, mut dq) = (false, false);
    let chars: Vec<char> = cmd.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\'' if !dq => sq = !sq,
            '"' if !sq => dq = !dq,
            // `2>&1`, `&>file`, `>&2` are redirections, not separators.
            '&' if !sq && !dq && (chars.get(i.wrapping_sub(1)) == Some(&'>') || chars.get(i + 1) == Some(&'>')) => {}
            '|' | ';' | '&' | '\n' | '(' | ')' if !sq && !dq => {
                segs.push(std::mem::take(&mut cur));
                i += 1;
                continue;
            }
            _ => {}
        }
        cur.push(c);
        i += 1;
    }
    segs.push(cur);
    const WRAP: [&str; 8] = ["sudo", "timeout", "nice", "env", "time", "exec", "command", "xargs"];
    const KEYWORDS: [&str; 12] = ["then", "do", "else", "fi", "done", "if", "for", "while", "in", "{", "}", "!"];
    for seg in segs {
        let mut words = seg.split_whitespace().peekable();
        while let Some(w) = words.next() {
            if w.contains('=') && !w.starts_with('-') && !w.starts_with('=') {
                continue; // FOO=bar
            }
            if KEYWORDS.contains(&w) || w.starts_with('$') || w.starts_with('>') || w.starts_with('<') {
                if matches!(w, "for" | "in") {
                    break; // `for x in a b c` — the list isn't programs
                }
                continue;
            }
            let base = w.rsplit('/').next().unwrap_or(w).trim_matches(|c| c == '"' || c == '\'');
            if WRAP.contains(&base) {
                // skip wrapper and its numeric/flag args (timeout 15, nice -n 5)
                while let Some(n) = words.peek() {
                    if n.starts_with('-') || n.chars().all(|c| c.is_ascii_digit() || c == '.' || c == 's') {
                        words.next();
                    } else {
                        break;
                    }
                }
                continue;
            }
            if !base.is_empty() && base.chars().next().map(|c| c.is_alphanumeric() || c == '.' || c == '_').unwrap_or(false) {
                out.push(base.to_string());
            }
            break;
        }
    }
    out
}

/// Commands worth a red highlight in the feed.
pub fn is_risky(cmd: &str) -> bool {
    let c = one_line(cmd);
    let has = |p: &str| c.contains(p);
    let rm_rf = c.split(|ch| ch == '|' || ch == ';' || ch == '&').any(|seg| {
        let w: Vec<&str> = seg.split_whitespace().collect();
        let i = w.iter().position(|x| *x == "rm");
        i.map(|i| {
            w[i + 1..].iter().any(|a| {
                a.starts_with('-') && !a.starts_with("--") && a.contains('r') && a.contains('f')
                    || *a == "--recursive"
            })
        })
        .unwrap_or(false)
    });
    rm_rf
        || has("git push --force")
        || has("git push -f")
        || has("--force-with-lease")
        || has("reset --hard")
        || has("git clean -f")
        || has("git clean -df")
        || has("git branch -D")
        || has("git checkout -- .")
        || has("git restore .")
        || has("git stash drop")
        || has("git stash clear")
        || has("mkfs")
        || has("dd if=")
        || has("chmod -R 777")
        || has("> /dev/sd")
        || has("> /dev/nvme")
        || has("DROP TABLE")
        || has("DROP DATABASE")
        || has("kill -9")
        || has("pkill")
        || has("killall")
        || c.starts_with("sudo ")
        || has("&& sudo ")
        || has("| sudo ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn programs_split() {
        assert_eq!(programs("cd x && rg foo | head -5"), ["cd", "rg", "head"]);
        assert_eq!(programs("FOO=1 timeout 15 ssh host 'ls | wc -l'"), ["ssh"]);
        assert_eq!(programs("/usr/bin/git -C r status; echo \"a|b\""), ["git", "echo"]);
        assert_eq!(programs("for f in a b; do cat $f; done"), ["cat"]);
        assert_eq!(programs("cargo build 2>&1 | tail -2"), ["cargo", "tail"]);
        assert_eq!(programs("make &> log && ls"), ["make", "ls"]);
        assert_eq!(programs("python3 - <<'EOF'\nimport os; print(1)\nEOF\ncargo test"), ["python3", "cargo"]);
    }

    #[test]
    fn risky() {
        assert!(is_risky("rm -rf /tmp/x"));
        assert!(is_risky("cd r && git push --force origin main"));
        assert!(is_risky("git reset --hard HEAD~1"));
        assert!(!is_risky("rm -f foo.log"));
        assert!(!is_risky("grep -rf patterns.txt ."));
        assert!(!is_risky("git push origin main"));
    }
}
