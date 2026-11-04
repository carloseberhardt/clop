//! The janitor: things Claude Code sessions left behind. Report-only; every
//! item carries the shell command that would clean it up.
//!
//! Evidence comes from transcripts (paths Claude wrote or mentioned, `git
//! worktree add`, fetch refspecs, container builds) and is confirmed against
//! the live system: a /tmp or ~/.cache entry only counts if it was created
//! after Claude first mentioned it.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use chrono::DateTime;
use serde_json::Value;

use crate::transcript::programs;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Kind {
    Orphan,
    Worktree,
    File,
    Scratch,
    Image,
    Container,
    Refs,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Orphan => "process",
            Kind::Worktree => "worktree",
            Kind::File => "file",
            Kind::Scratch => "scratch",
            Kind::Image => "image",
            Kind::Container => "container",
            Kind::Refs => "git refs",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Item {
    pub kind: Kind,
    /// What it is: a path, image name, "pid 1234 vite".
    pub what: String,
    /// Project (cwd basename) of the session that made it.
    pub project: String,
    #[allow(dead_code)]
    pub session: Option<String>,
    pub in_use: bool,
    pub size: Option<u64>,
    pub since: Option<SystemTime>,
    pub note: String,
    /// One-line cleanup command, or a commented-out one when it needs review.
    pub cmd: String,
    pub review: bool,
}

#[derive(Default)]
pub struct View {
    pub items: Vec<Item>,
    pub scanned: Option<SystemTime>,
    pub scanning: bool,
}

pub type Shared = Arc<Mutex<View>>;
pub type LiveIds = Arc<Mutex<HashSet<String>>>;

#[derive(Clone)]
struct Mention {
    first: SystemTime,
    last: SystemTime,
    /// Every mention time (capped), to check creation happened near one.
    times: Vec<SystemTime>,
    session: String,
    cwd: String,
}

#[derive(Default)]
struct Evidence {
    /// Root entry under /tmp or ~/.cache → first/last mention.
    paths: HashMap<PathBuf, Mention>,
    /// Worktree path from `git worktree add`.
    worktrees: HashMap<PathBuf, Mention>,
    /// (repo dir, ref prefix like refs/remotes/pr/)
    refs: HashMap<(PathBuf, String), Mention>,
    /// (engine, normalized image name)
    images: HashMap<(String, String), Mention>,
    containers: HashMap<(String, String), Mention>,
    /// Every directory a session ran in (repo candidates).
    cwds: HashMap<String, Mention>,
    /// Session id → its working directory.
    session_cwd: HashMap<String, String>,
}

pub fn spawn(view: Shared, live: LiveIds, rescan: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        let mut offsets: HashMap<PathBuf, u64> = HashMap::new();
        let mut ev = Evidence::default();
        let mut sizes: HashMap<PathBuf, (u64, SystemTime)> = HashMap::new();
        loop {
            if let Ok(mut v) = view.lock() {
                v.scanning = true;
            }
            read_transcripts(&mut offsets, &mut ev);
            let live_ids = live.lock().map(|l| l.clone()).unwrap_or_default();
            let items = scan(&ev, &live_ids, &mut sizes);
            if let Ok(mut v) = view.lock() {
                v.items = items;
                v.scanned = Some(SystemTime::now());
                v.scanning = false;
            }
            for _ in 0..60 {
                if rescan.swap(false, Ordering::SeqCst) {
                    break;
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    });
}

fn home() -> PathBuf {
    dirs::home_dir().unwrap_or_default()
}

fn uid() -> u32 {
    unsafe { libc::getuid() }
}

fn read_transcripts(offsets: &mut HashMap<PathBuf, u64>, ev: &mut Evidence) {
    let mut files = Vec::new();
    collect(&home().join(".claude/projects"), &mut files, 0);
    for f in files {
        let Ok(len) = fs::metadata(&f).map(|m| m.len()) else { continue };
        let off = offsets.get(&f).copied().unwrap_or(0);
        if len <= off {
            offsets.insert(f, len);
            continue;
        }
        let Ok(mut fh) = File::open(&f) else { continue };
        if fh.seek(SeekFrom::Start(off)).is_err() {
            continue;
        }
        let mut buf = Vec::new();
        if fh.read_to_end(&mut buf).is_err() {
            continue;
        }
        let Some(last) = buf.iter().rposition(|c| *c == b'\n') else { continue };
        for line in buf[..last].split(|c| *c == b'\n') {
            if let Ok(v) = serde_json::from_slice::<Value>(line) {
                ingest(&v, ev);
            }
        }
        offsets.insert(f, off + last as u64 + 1);
    }
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>, depth: u32) {
    if depth > 4 {
        return;
    }
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        match e.file_type() {
            Ok(t) if t.is_dir() => collect(&p, out, depth + 1),
            Ok(_) if p.extension().and_then(|x| x.to_str()) == Some("jsonl") => out.push(p),
            _ => {}
        }
    }
}

fn note<K: std::hash::Hash + Eq>(map: &mut HashMap<K, Mention>, key: K, m: &Mention) {
    map.entry(key)
        .and_modify(|e| {
            if e.times.len() < 500 {
                e.times.extend(m.times.iter().copied());
            }
            if m.first < e.first {
                e.first = m.first;
            }
            if m.last > e.last {
                e.last = m.last;
                e.session = m.session.clone();
                e.cwd = m.cwd.clone();
            }
        })
        .or_insert_with(|| m.clone());
}

fn ingest(v: &Value, ev: &mut Evidence) {
    if v.get("type").and_then(|t| t.as_str()) != Some("assistant") {
        return;
    }
    let Some(at) = v
        .get("timestamp")
        .and_then(|t| t.as_str())
        .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
        .map(SystemTime::from)
    else {
        return;
    };
    let session = v.get("sessionId").and_then(|s| s.as_str()).unwrap_or("").to_string();
    let cwd = v.get("cwd").and_then(|s| s.as_str()).unwrap_or("").to_string();
    let m = Mention { first: at, last: at, times: vec![at], session, cwd: cwd.clone() };
    if !cwd.is_empty() {
        note(&mut ev.cwds, cwd.clone(), &m);
        ev.session_cwd.entry(m.session.clone()).or_insert_with(|| cwd.clone());
    }
    let Some(parts) = v["message"]["content"].as_array() else { return };
    for p in parts.iter().filter(|p| p["type"] == "tool_use") {
        let input = &p["input"];
        match p["name"].as_str().unwrap_or("") {
            "Write" => {
                if let Some(root) = input["file_path"].as_str().and_then(cruft_root) {
                    note(&mut ev.paths, root, &m);
                }
            }
            "Bash" => {
                let cmd = input["command"].as_str().unwrap_or("");
                for path in path_mentions(cmd) {
                    if let Some(root) = cruft_root(&path) {
                        note(&mut ev.paths, root, &m);
                    }
                }
                // Follow `cd` so relative paths resolve where the command ran.
                let mut base = cwd.clone();
                for seg in segments(cmd) {
                    if seg.first().map(String::as_str) == Some("cd") {
                        if let Some(d) = seg.get(1) {
                            base = normalize(&expand(d, &base)).to_string_lossy().into_owned();
                        }
                        continue;
                    }
                    parse_segment(&seg, &base, &m, ev);
                }
            }
            _ => {}
        }
    }
}

/// Command split on && || ; | and newlines (quotes respected, heredoc bodies kept out).
fn segments(cmd: &str) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    for line in strip_heredoc_bodies(cmd).lines() {
        let mut cur: Vec<String> = Vec::new();
        let mut word = String::new();
        let (mut sq, mut dq) = (false, false);
        let flush = |w: &mut String, c: &mut Vec<String>| {
            if !w.is_empty() {
                c.push(std::mem::take(w));
            }
        };
        let chars: Vec<char> = line.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            match c {
                '\'' if !dq => sq = !sq,
                '"' if !sq => dq = !dq,
                ' ' | '\t' if !sq && !dq => flush(&mut word, &mut cur),
                '&' if !sq && !dq && (chars.get(i + 1) == Some(&'>') || i > 0 && chars[i - 1] == '>') => word.push(c),
                ';' | '|' | '&' | '(' | ')' if !sq && !dq => {
                    flush(&mut word, &mut cur);
                    if !cur.is_empty() {
                        out.push(std::mem::take(&mut cur));
                    }
                }
                _ => word.push(c),
            }
            i += 1;
        }
        flush(&mut word, &mut cur);
        if !cur.is_empty() {
            out.push(cur);
        }
    }
    out
}

fn strip_heredoc_bodies(cmd: &str) -> String {
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
            let word: String =
                rest.trim_start_matches(['\'', '"']).chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
            if !word.is_empty() && !rest.starts_with('<') {
                delim = Some(word);
            }
        }
    }
    out
}

fn expand(p: &str, base: &str) -> PathBuf {
    let h = home();
    let hs = h.to_string_lossy();
    let p = p.replace("${HOME}", &hs).replace("$HOME", &hs);
    let p = if let Some(r) = p.strip_prefix("~/") { format!("{hs}/{r}") } else { p };
    let pb = PathBuf::from(&p);
    if pb.is_absolute() || base.is_empty() {
        pb
    } else {
        Path::new(base).join(pb)
    }
}

/// Paths inside the configured temp folders mentioned anywhere in a command.
pub fn path_mentions(cmd: &str) -> Vec<String> {
    let roots = crate::config::get().janitor.temp_roots();
    let mut out = Vec::new();
    for tok in strip_heredoc_bodies(cmd).split(|c: char| c.is_whitespace() || "=<>|;&()`\"',".contains(c)) {
        let t = tok.trim_end_matches([':', '.', ']', '}']).trim_start_matches(['+', '@']);
        if !(t.starts_with('/') || t.starts_with("~/") || t.starts_with("$HOME/") || t.starts_with("${HOME}/")) {
            continue;
        }
        if t.contains(['*', '?', '[']) {
            continue;
        }
        let e = expand(t, "");
        if e.to_string_lossy().contains('$') {
            continue;
        }
        if roots.iter().any(|r| e.starts_with(r) && e != *r) {
            out.push(e.to_string_lossy().into_owned());
        }
    }
    out
}

/// The entry directly inside a temp folder that a path lives in.
fn cruft_root(path: &str) -> Option<PathBuf> {
    let p = normalize(&expand(path, ""));
    for base in crate::config::get().janitor.temp_roots() {
        if let Ok(rest) = p.strip_prefix(&base) {
            let first = rest.components().next()?;
            let name = first.as_os_str().to_string_lossy();
            if name.starts_with("claude-") || name.is_empty() || name == "." || name == ".." {
                return None;
            }
            return Some(base.join(first));
        }
    }
    None
}

fn parse_segment(words: &[String], cwd: &str, m: &Mention, ev: &mut Evidence) {
    let joined = words.join(" ");
    let progs = programs(&joined);
    let Some(prog) = progs.first() else { return };
    // `git -C repo …`
    let mut repo = cwd.to_string();
    if let Some(i) = words.iter().position(|w| w == "-C") {
        if let Some(d) = words.get(i + 1) {
            repo = expand(d, cwd).to_string_lossy().into_owned();
        }
    }
    match prog.as_str() {
        "git" => {
            if let Some(i) = words.windows(2).position(|w| w[0] == "worktree" && w[1] == "add") {
                let mut it = words[i + 2..].iter();
                while let Some(w) = it.next() {
                    if matches!(w.as_str(), "-b" | "-B" | "--reason") {
                        it.next();
                    } else if !w.starts_with('-') {
                        let wt = expand(w, &repo);
                        note(&mut ev.worktrees, normalize(&wt), m);
                        note(&mut ev.cwds, repo.clone(), m);
                        break;
                    }
                }
            }
            for w in words {
                if let Some(i) = w.find(":refs/") {
                    let dest = &w[i + 1..];
                    let Some(slash) = dest.rfind('/') else { continue };
                    let prefix = &dest[..=slash];
                    if crate::config::get().janitor.ignored_ref(prefix) {
                        continue;
                    }
                    note(&mut ev.refs, (PathBuf::from(&repo), prefix.to_string()), m);
                    note(&mut ev.cwds, repo.clone(), m);
                }
            }
        }
        "podman" | "docker" => {
            let sub = words.iter().skip_while(|w| !w.ends_with(prog.as_str())).nth(1).map(String::as_str);
            let take = |flags: &[&str]| -> Option<String> {
                let mut it = words.iter();
                while let Some(w) = it.next() {
                    for f in flags {
                        if w == f {
                            return it.next().cloned();
                        }
                        if let Some(v) = w.strip_prefix(&format!("{f}=")) {
                            return Some(v.to_string());
                        }
                    }
                }
                None
            };
            match sub {
                Some("build") | Some("buildx") => {
                    if let Some(t) = take(&["-t", "--tag"]) {
                        note(&mut ev.images, (prog.clone(), norm_image(&t)), m);
                    }
                }
                Some("run") | Some("create") => {
                    if let Some(n) = take(&["--name"]) {
                        note(&mut ev.containers, (prog.clone(), n), m);
                    }
                }
                _ => {}
            }
        }
        _ => {}
    }
}

fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            c => out.push(c),
        }
    }
    out
}

fn norm_image(n: &str) -> String {
    let n = n.trim_start_matches("localhost/").trim_start_matches("docker.io/library/");
    if n.rsplit('/').next().is_some_and(|last| last.contains(':')) {
        n.to_string()
    } else {
        format!("{n}:latest")
    }
}

pub fn quote(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_alphanumeric() || "_./-:@%+=".contains(c)) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

fn step(cmd: &str, ok: &str, what: &str) -> String {
    format!("{cmd} && echo {} || echo {}", quote(&format!("{ok} {what}")), quote(&format!("note: {what} not removed")))
}

fn git(dir: &str, args: &[&str]) -> Option<String> {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

fn project(cwd: &str) -> String {
    cwd.rsplit('/').next().unwrap_or(cwd).to_string()
}

fn size_of(p: &Path, cache: &mut HashMap<PathBuf, (u64, SystemTime)>) -> u64 {
    if let Some((s, at)) = cache.get(p) {
        if at.elapsed().map(|d| d < Duration::from_secs(600)).unwrap_or(false) {
            return *s;
        }
    }
    let s = crate::sessions::du(p);
    cache.insert(p.to_path_buf(), (s, SystemTime::now()));
    s
}

fn scan(ev: &Evidence, live: &HashSet<String>, sizes: &mut HashMap<PathBuf, (u64, SystemTime)>) -> Vec<Item> {
    let cfg = &crate::config::get().janitor;
    let window = Duration::from_secs(cfg.created_within_minutes * 60);
    let mut items = Vec::new();

    // Files and folders under /tmp and ~/.cache, created after Claude first mentioned them.
    for (root, m) in &ev.paths {
        if cfg.ignored_path(root) {
            continue;
        }
        let Ok(meta) = fs::symlink_metadata(root) else { continue };
        let Ok(born) = meta.created() else { continue };
        // Claude made it only if it appeared around a time Claude wrote or
        // ran something naming it. Looking at a path later doesn't count, and
        // neither does another program recreating it afterward.
        let near = m.times.iter().any(|t| {
            born + Duration::from_secs(120) >= *t && born <= *t + window
        });
        if !near {
            continue;
        }
        // Worktrees under ~/.cache are reported as worktrees, not files.
        if root.join(".git").is_file() {
            continue;
        }
        let what = root.to_string_lossy().into_owned();
        let flag = if meta.is_dir() { "-rf" } else { "-f" };
        items.push(Item {
            kind: Kind::File,
            what: what.clone(),
            project: project(&m.cwd),
            session: Some(m.session.clone()),
            in_use: live.contains(&m.session),
            size: Some(size_of(root, sizes)),
            since: Some(born),
            note: if meta.is_dir() { "folder".into() } else { String::new() },
            cmd: step(&format!("rm {flag} -- {}", quote(&what)), "removed", &what),
            review: false,
        });
    }

    // Worktrees: every linked worktree of every repo Claude worked in, kept if
    // Claude added it (transcript) or it lives under .claude/worktrees.
    let mut seen_common = HashSet::new();
    let mut prune_repos = Vec::new();
    let mut dirs: Vec<&String> = ev.cwds.keys().collect();
    dirs.sort();
    for dir in dirs {
        if !Path::new(dir).is_dir() {
            continue;
        }
        let Some(common) = git(dir, &["rev-parse", "--path-format=absolute", "--git-common-dir"]) else { continue };
        if !seen_common.insert(common.trim().to_string()) {
            continue;
        }
        let Some(list) = git(dir, &["worktree", "list", "--porcelain"]) else { continue };
        let mut wts: Vec<(String, String)> = Vec::new(); // (path, branch)
        for block in list.split("\n\n") {
            let path = block.lines().find_map(|l| l.strip_prefix("worktree ")).unwrap_or("").to_string();
            let branch = block
                .lines()
                .find_map(|l| l.strip_prefix("branch refs/heads/"))
                .map(String::from)
                .unwrap_or_else(|| if block.contains("detached") { "detached".into() } else { String::new() });
            if !path.is_empty() {
                wts.push((path, branch));
            }
        }
        let Some((main, _)) = wts.first().cloned() else { continue };
        let mut any = false;
        let mut any_ended = false;
        for (wt, branch) in wts.iter().skip(1) {
            let wp = PathBuf::from(wt);
            let m = ev.worktrees.get(&wp).cloned().or_else(|| {
                ev.worktrees.iter().find(|(k, _)| k.ends_with(wp.file_name().unwrap_or_default()) && wp.ends_with(k.file_name().unwrap_or_default())).map(|(_, m)| m.clone())
            });
            let claude_dir = wt.contains("/.claude/worktrees/");
            if (m.is_none() && !claude_dir) || cfg.ignored_path(&wp) {
                continue;
            }
            any = true;
            let session = m.as_ref().map(|m| m.session.clone());
            let in_use = session.as_ref().is_some_and(|s| live.contains(s));
            any_ended |= !in_use;
            let status = git(wt, &["status", "--porcelain"]);
            let dirty = status.as_ref().map(|s| s.lines().count()).unwrap_or(0);
            let missing = !wp.exists();
            let mut note = if branch.is_empty() { String::new() } else { branch.clone() };
            let review = dirty > 0;
            let cmd = if missing {
                note = format!("{note} · directory already gone");
                format!("git -C {} worktree prune && echo 'pruned stale worktree entry'", quote(&main))
            } else if review {
                note = format!("{note} · {dirty} uncommitted");
                format!("# {dirty} uncommitted change(s) — review first:\n# git -C {} worktree remove --force {}", quote(&main), quote(wt))
            } else {
                step(&format!("git -C {} worktree remove {}", quote(&main), quote(wt)), "removed worktree", wt)
            };
            items.push(Item {
                kind: Kind::Worktree,
                what: wt.clone(),
                project: project(&main),
                session,
                in_use,
                size: (!missing).then(|| size_of(&wp, sizes)),
                since: m.map(|m| m.first).or_else(|| fs::metadata(&wp).and_then(|x| x.created()).ok()),
                note,
                cmd,
                review,
            });
        }
        if any {
            prune_repos.push((main, !any_ended));
        }
    }

    // Custom ref namespaces Claude fetched into (refs/remotes/pr/…).
    for ((repo, prefix), m) in &ev.refs {
        if cfg.ignored_ref(prefix) || cfg.ignored_path(repo) {
            continue;
        }
        let Some(out) = git(&repo.to_string_lossy(), &["for-each-ref", "--format=%(refname)", prefix]) else { continue };
        let n = out.lines().count();
        if n == 0 {
            continue;
        }
        let r = repo.to_string_lossy().into_owned();
        items.push(Item {
            kind: Kind::Refs,
            what: format!("{prefix}* ({n})"),
            project: project(&r),
            session: Some(m.session.clone()),
            in_use: live.contains(&m.session),
            size: None,
            since: Some(m.first),
            note: format!("in {}", project(&r)),
            cmd: format!(
                "git -C {r} for-each-ref --format='delete %(refname)' {prefix} | git -C {r} update-ref --stdin && echo {} || echo {}",
                quote(&format!("deleted {n} refs under {prefix}")),
                quote(&format!("note: refs under {prefix} not deleted")),
                r = quote(&r),
                prefix = quote(prefix),
            ),
            review: false,
        });
    }

    // Container images and containers.
    let engines: HashSet<&String> = ev.images.keys().chain(ev.containers.keys()).map(|(e, _)| e).collect();
    for engine in engines {
        let imgs = Command::new(engine)
            .args(["images", "--format", "{{.Repository}}:{{.Tag}}|{{.Size}}"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();
        for line in imgs.lines() {
            let mut it = line.splitn(2, '|');
            let name = norm_image(it.next().unwrap_or(""));
            let size = it.next().unwrap_or("").to_string();
            if let Some(m) = ev.images.get(&(engine.clone(), name.clone())) {
                items.push(Item {
                    kind: Kind::Image,
                    what: name.clone(),
                    project: project(&m.cwd),
                    session: Some(m.session.clone()),
                    in_use: live.contains(&m.session),
                    size: None,
                    since: Some(m.first),
                    note: format!("{engine} · {size}"),
                    cmd: step(&format!("{engine} rmi {}", quote(&name)), "removed image", &name),
                    review: false,
                });
            }
        }
        let ps = Command::new(engine)
            .args(["ps", "-a", "--format", "{{.Names}}|{{.State}}"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();
        for line in ps.lines() {
            let mut it = line.splitn(2, '|');
            let name = it.next().unwrap_or("").to_string();
            let state = it.next().unwrap_or("").to_string();
            if let Some(m) = ev.containers.get(&(engine.clone(), name.clone())) {
                items.push(Item {
                    kind: Kind::Container,
                    what: name.clone(),
                    project: project(&m.cwd),
                    session: Some(m.session.clone()),
                    in_use: live.contains(&m.session),
                    size: None,
                    since: Some(m.first),
                    note: format!("{engine} · {state}"),
                    cmd: step(&format!("{engine} rm -f {}", quote(&name)), "removed container", &name),
                    review: false,
                });
            }
        }
    }

    // Scratchpads of sessions that are no longer running, one row per project.
    let tmp = PathBuf::from(format!("/tmp/claude-{}", uid()));
    if let Ok(rd) = fs::read_dir(&tmp) {
        for proj in rd.flatten() {
            if cfg.ignored_path(&proj.path()) {
                continue;
            }
            let Ok(sessions) = fs::read_dir(proj.path()) else { continue };
            let mut dead: Vec<(PathBuf, String, Option<SystemTime>)> = Vec::new();
            let mut live_dirs: Vec<(PathBuf, String)> = Vec::new();
            for s in sessions.flatten() {
                let name = s.file_name().to_string_lossy().into_owned();
                if name.len() != 36 || name.matches('-').count() != 4 {
                    continue; // not a session id
                }
                if live.contains(&name) {
                    live_dirs.push((s.path(), name));
                } else {
                    dead.push((s.path(), name, s.metadata().and_then(|m| m.modified()).ok()));
                }
            }
            let pname = |sid: &str| {
                let dir = proj.file_name().to_string_lossy().into_owned();
                ev.session_cwd
                    .get(sid)
                    .or_else(|| ev.cwds.keys().find(|c| crate::sessions::mangle(c) == dir))
                    .map(|c| project(c))
                    .unwrap_or(dir)
            };
            if !dead.is_empty() {
                let size: u64 = dead.iter().map(|(p, _, _)| size_of(p, sizes)).sum();
                let newest = dead.iter().filter_map(|(_, _, t)| *t).max();
                let paths: Vec<String> = dead.iter().map(|(p, _, _)| quote(&p.to_string_lossy())).collect();
                let label = format!("{} ({} ended sessions)", proj.path().to_string_lossy(), dead.len());
                items.push(Item {
                    kind: Kind::Scratch,
                    what: label,
                    project: pname(&dead[0].1),
                    session: None,
                    in_use: false,
                    size: Some(size),
                    since: newest,
                    note: format!("{} scratchpad(s)", dead.len()),
                    cmd: format!(
                        "rm -rf -- {} && echo {} || echo {}",
                        paths.join(" "),
                        quote(&format!("removed {} scratchpads in {}", dead.len(), proj.file_name().to_string_lossy())),
                        quote("note: some scratchpads not removed")
                    ),
                    review: false,
                });
            }
            for (p, sid) in live_dirs {
                let what = p.to_string_lossy().into_owned();
                items.push(Item {
                    kind: Kind::Scratch,
                    what: what.clone(),
                    project: pname(&sid),
                    session: Some(sid),
                    in_use: true,
                    size: Some(size_of(&p, sizes)),
                    since: None,
                    note: String::new(),
                    cmd: step(&format!("rm -rf -- {}", quote(&what)), "removed", &what),
                    review: false,
                });
            }
        }
    }

    // One prune per repo after worktree removals.
    for (main, in_use) in prune_repos {
        items.push(Item {
            kind: Kind::Worktree,
            what: format!("{main} (prune)"),
            project: project(&main),
            session: None,
            in_use,
            size: None,
            since: None,
            note: "tidy stale worktree entries".into(),
            cmd: format!("git -C {} worktree prune", quote(&main)),
            review: false,
        });
    }
    items
}

/// Processes carrying the CLAUDE_PID of a session that no longer exists.
pub fn orphans(app: &crate::app::App) -> Vec<Item> {
    let me = std::process::id() as i32;
    let live: HashSet<i32> = app.sessions.iter().map(|s| s.reg.pid).collect();
    let mut mine = HashSet::new();
    let mut cur = me;
    while let Some(p) = app.procs.get(&cur) {
        mine.insert(cur);
        if p.ppid <= 1 {
            break;
        }
        cur = p.ppid;
    }
    let dead = |p: &crate::procs::Proc| {
        p.claude_pid.is_some_and(|c| {
            !live.contains(&c) && app.procs.get(&c).map(|q| q.comm != "claude" && !q.comm.chars().all(|x| x.is_ascii_digit() || x == '.')).unwrap_or(true)
        })
    };
    let mut out = Vec::new();
    let now = std::time::Instant::now();
    for p in app.procs.values() {
        if mine.contains(&p.pid) || !dead(p) || managed(p) {
            continue;
        }
        // Only the top of each orphaned subtree.
        if app.procs.get(&p.ppid).is_some_and(|q| dead(q) && !mine.contains(&q.pid)) {
            continue;
        }
        let args = p.cmdline.join(" ");
        let what = format!("{} {}", p.pid, if args.is_empty() { p.comm.clone() } else { args });
        let watched = now.duration_since(p.first_seen);
        out.push(Item {
            kind: Kind::Orphan,
            what,
            project: String::new(),
            session: None,
            in_use: false,
            size: Some(p.rss),
            since: SystemTime::now().checked_sub(watched),
            note: format!("session pid {} is gone", p.claude_pid.unwrap_or(0)),
            cmd: format!(
                "kill {} && echo {} || echo {}",
                p.pid,
                quote(&format!("killed {} ({})", p.pid, p.comm)),
                quote(&format!("note: {} not killed", p.pid))
            ),
            review: false,
        });
    }
    out
}

/// Long-lived helpers a session may have started but that aren't leftovers:
/// rootless podman's pause process, agents, and anything systemd manages.
fn managed(p: &crate::procs::Proc) -> bool {
    if crate::config::get().janitor.ignored_process(&p.comm) {
        return true;
    }
    let cg = fs::read_to_string(format!("/proc/{}/cgroup", p.pid)).unwrap_or_default();
    let last = cg.trim().rsplit('/').next().unwrap_or("");
    last.ends_with(".service") || last.starts_with("podman-pause")
}

/// The whole list as a script in the style Carlos's hand-made ones use:
/// each step independent, a note instead of an abort when something's gone.
pub fn script(items: &[&Item], host: &str) -> String {
    let mut s = String::new();
    s.push_str("#!/usr/bin/env bash\n");
    s.push_str(&format!("# clop cleanup · {host} · {}\n", chrono::Local::now().format("%Y-%m-%d %H:%M")));
    s.push_str("# What ended Claude Code sessions left behind. Review before running.\n");
    s.push_str("# Each step runs independently; a missing item prints a note instead of aborting.\n");
    s.push_str("set -u\n");
    let mut last: Option<Kind> = None;
    for it in items {
        if last != Some(it.kind) {
            s.push_str(&format!("\n# --- {}\n", it.kind.label()));
            last = Some(it.kind);
        }
        s.push_str(&it.cmd);
        s.push('\n');
    }
    s.push_str("\necho done\n");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mentions() {
        let h = home().to_string_lossy().into_owned();
        assert_eq!(path_mentions("cat > /tmp/pr2209.diff && ls"), ["/tmp/pr2209.diff"]);
        assert_eq!(path_mentions("rm -rf ~/.cache/pr123-review"), [format!("{h}/.cache/pr123-review")]);
        assert_eq!(path_mentions("cmd --out=/tmp/x.json 2>/tmp/err.log"), ["/tmp/x.json", "/tmp/err.log"]);
        assert!(path_mentions("ls /tmp/*.log").is_empty());
        assert_eq!(cruft_root("/tmp/a/b/c"), Some(PathBuf::from("/tmp/a")));
        assert_eq!(cruft_root("/tmp/claude-1000/x"), None);
        assert_eq!(cruft_root("/etc/passwd"), None);
    }

    #[test]
    fn worktree_add() {
        let mut ev = Evidence::default();
        let m = Mention { first: SystemTime::now(), last: SystemTime::now(), times: vec![], session: "s".into(), cwd: "/r".into() };
        for seg in segments("cd /r && git -C /repo worktree add -b feat ../wt/pr-1 origin/main") {
            parse_segment(&seg, "/r", &m, &mut ev);
        }
        assert!(ev.worktrees.contains_key(Path::new("/wt/pr-1")));
        for seg in segments("git fetch origin '+refs/pull/*/head:refs/remotes/pr/*'") {
            parse_segment(&seg, "/r", &m, &mut ev);
        }
        assert!(ev.refs.contains_key(&(PathBuf::from("/r"), "refs/remotes/pr/".to_string())));
        for seg in segments("podman build -t pr123-runner . && podman run --name web -d x") {
            parse_segment(&seg, "/r", &m, &mut ev);
        }
        assert!(ev.images.contains_key(&("podman".into(), "pr123-runner:latest".into())));
        assert!(ev.containers.contains_key(&("podman".into(), "web".into())));
    }

    #[test]
    fn quoting() {
        assert_eq!(quote("/tmp/a.b"), "/tmp/a.b");
        assert_eq!(quote("it's here"), "'it'\\''s here'");
    }
}
