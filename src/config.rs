//! Settings from ~/.config/clop/config.toml. Everything is optional; a
//! missing file means the defaults below.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde::Deserialize;

#[derive(Deserialize, Clone, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub janitor: Janitor,
    pub processes: Processes,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct Processes {
    /// Seconds a finished process stays in the tree, fading. 0 = drop at once.
    pub fade_seconds: u64,
}

impl Default for Processes {
    fn default() -> Self {
        Self { fade_seconds: 10 }
    }
}

#[derive(Deserialize, Clone, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct Janitor {
    /// Folders where Claude's throwaway files land. Anything directly inside
    /// one that Claude created is a leftover candidate. Replaces the default.
    pub temp_dirs: Vec<String>,
    /// Never report these paths, or anything under them. Added to the defaults.
    pub ignore_paths: Vec<String>,
    /// Never report processes with these names. Added to the defaults.
    pub ignore_processes: Vec<String>,
    /// Never report refs under these prefixes. Added to the defaults.
    pub ignore_refs: Vec<String>,
    /// A temp entry counts as Claude's only if it was created within this
    /// many minutes after Claude wrote or ran something naming it.
    pub created_within_minutes: u64,
}

pub const DEFAULT_IGNORE_PROCESSES: [&str; 10] =
    ["catatonit", "conmon", "gpg-agent", "ssh-agent", "keyboxd", "dirmngr", "scdaemon", "dbus-daemon", "dbus-broker", "podman"];
pub const DEFAULT_IGNORE_REFS: [&str; 4] = ["refs/heads/", "refs/tags/", "refs/remotes/origin/", "refs/remotes/upstream/"];

impl Default for Config {
    fn default() -> Self {
        Self { janitor: Janitor::default(), processes: Processes::default() }
    }
}

impl Default for Janitor {
    fn default() -> Self {
        Self {
            temp_dirs: vec!["/tmp".into(), "~/.cache".into()],
            ignore_paths: Vec::new(),
            ignore_processes: Vec::new(),
            ignore_refs: Vec::new(),
            created_within_minutes: 30,
        }
    }
}

/// Where the config lives, and how loading went (shown in the UI).
pub struct Loaded {
    pub path: PathBuf,
    pub status: String,
}

static CONFIG: OnceLock<Config> = OnceLock::new();
static LOADED: OnceLock<Loaded> = OnceLock::new();

pub fn path() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".config"))
        .join("clop/config.toml")
}

/// Read the file once at startup. A broken file falls back to defaults and
/// says why, rather than refusing to start.
pub fn load() {
    let p = path();
    let (cfg, status) = match std::fs::read_to_string(&p) {
        Err(_) => (Config::default(), "defaults (no config file)".to_string()),
        Ok(text) => match toml::from_str::<Config>(&text) {
            Ok(c) => (c, "loaded".to_string()),
            Err(e) => (Config::default(), format!("error, using defaults: {}", e.message())),
        },
    };
    let _ = CONFIG.set(cfg);
    let _ = LOADED.set(Loaded { path: p, status });
}

pub fn get() -> &'static Config {
    CONFIG.get_or_init(Config::default)
}

pub fn loaded() -> Option<&'static Loaded> {
    LOADED.get()
}

pub fn expand(p: &str) -> PathBuf {
    let home = dirs::home_dir().unwrap_or_default();
    let hs = home.to_string_lossy();
    let p = p.replace("${HOME}", &hs).replace("$HOME", &hs);
    match p.strip_prefix("~/") {
        Some(r) => home.join(r),
        None if p == "~" => home,
        None => PathBuf::from(p),
    }
}

impl Janitor {
    pub fn temp_roots(&self) -> Vec<PathBuf> {
        self.temp_dirs.iter().map(|d| expand(d)).collect()
    }

    pub fn ignored_path(&self, p: &Path) -> bool {
        self.ignore_paths.iter().any(|i| p.starts_with(expand(i)))
    }

    pub fn ignored_process(&self, comm: &str) -> bool {
        DEFAULT_IGNORE_PROCESSES.contains(&comm) || self.ignore_processes.iter().any(|n| n == comm)
    }

    pub fn ignored_ref(&self, prefix: &str) -> bool {
        DEFAULT_IGNORE_REFS.iter().any(|i| prefix.starts_with(i)) || self.ignore_refs.iter().any(|i| prefix.starts_with(i.as_str()))
    }
}

/// A commented starting file: `clop --default-config > ~/.config/clop/config.toml`.
pub const TEMPLATE: &str = r#"# clop settings. Every key is optional; delete what you don't change.

[processes]
# Seconds a finished process stays in the process tree, fading out.
# 0 removes it as soon as it ends.
fade_seconds = 10

[janitor]
# Folders where Claude's throwaway files land. Anything directly inside one
# that a Claude session created shows up in the cleanup panel.
# Setting this replaces the default list.
temp_dirs = ["/tmp", "~/.cache"]

# Never report these paths or anything under them. Good for caches your own
# tools keep on purpose. Added to the built-in rules.
ignore_paths = [
  # "~/.cache/my-build-cache",
]

# Never report leftover processes with these names. Built in already:
# catatonit, conmon, gpg-agent, ssh-agent, keyboxd, dirmngr, scdaemon,
# dbus-daemon, dbus-broker, podman.
ignore_processes = []

# Never report git refs under these prefixes. Built in already:
# refs/heads/, refs/tags/, refs/remotes/origin/, refs/remotes/upstream/.
ignore_refs = []

# A temp file or folder counts as Claude's only if it was created within this
# many minutes after Claude wrote or ran something that names it.
created_within_minutes = 30
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_parses_and_matches_defaults() {
        let c: Config = toml::from_str(TEMPLATE).unwrap();
        assert_eq!(c.janitor.temp_dirs, Janitor::default().temp_dirs);
        assert_eq!(c.janitor.created_within_minutes, 30);
        assert_eq!(c.processes.fade_seconds, Processes::default().fade_seconds);
    }

    #[test]
    fn partial_and_typos() {
        let c: Config = toml::from_str("[janitor]\nignore_paths = [\"~/.cache/my-build-cache\"]\n").unwrap();
        assert!(c.janitor.ignored_path(&expand("~/.cache/my-build-cache/main-wt")));
        assert!(!c.janitor.ignored_path(&expand("~/.cache/other")));
        assert_eq!(c.janitor.temp_dirs.len(), 2);
        assert!(toml::from_str::<Config>("[janitor]\ntemp_dir = []\n").is_err());
        assert!(c.janitor.ignored_ref("refs/remotes/origin/"));
        assert!(!c.janitor.ignored_ref("refs/remotes/pr/"));
    }
}
