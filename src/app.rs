//! WavySync's own settings, the sync and apply operations, and starting at login.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::repo::{self, Repo};
use crate::wave::{self, OWN_WIDGET};

/// Sync items in the order the wizard shows them.
pub const ITEMS: [(&str, bool); 6] = [
    ("settings", true),
    ("workspaces", true),
    ("widgets", true),
    ("themes", true),
    ("connections", true),
    ("ai", false),
];
/// What live sync pushes on its own, of the items above. Layouts change with every pane and tab,
/// so they wait for a manual Push unless the user turns them on.
pub const LIVE: [(&str, bool); 6] = [
    ("settings", true),
    ("workspaces", false),
    ("widgets", true),
    ("themes", true),
    ("connections", true),
    ("ai", true),
];
pub const KEYRING_SERVICE: &str = "WavySync";
/// GitHub OAuth app for the device-code sign-in. Empty until the app is registered; WAVYSYNC_CLIENT_ID overrides.
pub const CLIENT_ID: &str = "";

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Config {
    pub repo: String,
    pub guest: bool,
    pub login: String,
    /// This computer's own folder; it starts as the computer id and can be renamed.
    pub folder: String,
    /// Where changes go. Normally `folder`; after applying another profile it can be that one.
    pub target: String,
    pub items: BTreeMap<String, bool>,
    pub widgets: BTreeMap<String, bool>,
    /// Live sync on or off, and which of the synced items it pushes by itself.
    pub auto: bool,
    pub live: BTreeMap<String, bool>,
    pub interval: u64,
    pub setup_done: bool,
}

impl Default for Config {
    fn default() -> Self {
        let id = computer_id();
        Config {
            repo: String::new(),
            guest: false,
            login: String::new(),
            folder: id.clone(),
            target: id,
            items: ITEMS.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
            widgets: BTreeMap::new(),
            auto: true,
            live: LIVE.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
            interval: 30,
            setup_done: false,
        }
    }
}

pub fn dir() -> PathBuf {
    std::env::var_os("WAVYSYNC_HOME").map(PathBuf::from).unwrap_or_else(|| wave::home().join(".config/wavysync"))
}

impl Config {
    pub fn load() -> Config {
        fs::read(dir().join("config.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }
    pub fn save(&self) {
        let _ = fs::create_dir_all(dir());
        let _ = fs::write(dir().join("config.json"), serde_json::to_vec_pretty(self).unwrap());
    }
    /// The items live sync pushes: synced at all, and switched on for live.
    pub fn live_items(&self) -> BTreeMap<String, bool> {
        self.items.iter().map(|(k, on)| (k.clone(), *on && self.live.get(k).copied().unwrap_or(k != "workspaces"))).collect()
    }
    pub fn repo(&self) -> Repo {
        Repo { dir: dir().join("repo"), url: repo::normalize(&self.repo), token: if self.guest { None } else { token() } }
    }
}

fn sanitize(s: &str) -> String {
    let s: String = s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' }).collect();
    s.trim_matches('-').to_string()
}

pub fn hostname() -> String {
    std::env::var("COMPUTERNAME")
        .ok()
        .or_else(|| Command::new("hostname").arg("-s").output().ok().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()))
        .or_else(|| Command::new("hostname").output().ok().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "computer".into())
}

fn machine_uuid() -> String {
    let out = |c: &str, a: &[&str]| Command::new(c).args(a).output().ok().map(|o| String::from_utf8_lossy(&o.stdout).to_string());
    let raw = if cfg!(target_os = "macos") {
        out("ioreg", &["-rd1", "-c", "IOPlatformExpertDevice"])
            .and_then(|s| s.lines().find(|l| l.contains("IOPlatformUUID")).map(|l| l.rsplit('"').nth(1).unwrap_or_default().to_string()))
    } else if cfg!(windows) {
        out("reg", &["query", r"HKLM\SOFTWARE\Microsoft\Cryptography", "/v", "MachineGuid"])
            .and_then(|s| s.split_whitespace().last().map(String::from))
    } else {
        fs::read_to_string("/etc/machine-id").ok()
    };
    raw.unwrap_or_default().trim().to_lowercase().replace('-', "")
}

/// `<host>-<first 4 of the machine id>`: readable, and two computers named alike still differ.
pub fn computer_id() -> String {
    let id = machine_uuid();
    let host = sanitize(&hostname());
    if id.len() >= 4 { format!("{host}-{}", &id[..4]) } else { host }
}

pub fn token() -> Option<String> {
    // Set but empty: no token, and the keychain is left alone (tests, screenshots).
    if let Ok(t) = std::env::var("WAVYSYNC_TOKEN") {
        return (!t.is_empty()).then_some(t);
    }
    keyring::Entry::new(KEYRING_SERVICE, "github").ok()?.get_password().ok()
}

pub fn set_token(t: Option<&str>) -> Result<(), String> {
    let e = keyring::Entry::new(KEYRING_SERVICE, "github").map_err(|e| e.to_string())?;
    match t {
        Some(t) => e.set_password(t).map_err(|e| e.to_string()),
        None => e.delete_credential().or(Ok(())),
    }
}

pub fn client_id() -> String {
    std::env::var("WAVYSYNC_CLIENT_ID").unwrap_or_else(|_| CLIENT_ID.into())
}

// Status the background loop writes and the window reads ----------------------------------------

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct Status {
    pub last_check: u64,
    pub last_push: u64,
    pub last_pull: u64,
    pub last_hash: u64,
    pub error: String,
    pub feed: Vec<(u64, String)>,
}

impl Status {
    pub fn load() -> Status {
        fs::read(dir().join("status.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }
    fn save(&self) {
        let _ = fs::create_dir_all(dir());
        let _ = fs::write(dir().join("status.json"), serde_json::to_vec_pretty(self).unwrap());
    }
    fn note(&mut self, msg: String) {
        self.feed.insert(0, (repo::now(), msg));
        self.feed.truncate(30);
    }
}

/// One git operation at a time across the window, the background loop and an apply.
fn with_lock<T>(f: impl FnOnce() -> T) -> T {
    let _ = fs::create_dir_all(dir());
    let lock = fs::OpenOptions::new().create(true).truncate(false).write(true).open(dir().join("sync.lock"));
    if let Ok(l) = &lock {
        let _ = l.lock();
    }
    f()
}

fn meta() -> Value {
    json!({ "format": 1, "host": hostname(), "os": std::env::consts::OS, "pc": computer_id() })
}

/// Exports this computer's Wave and pushes it to the target folder if anything changed.
/// `force` pushes even when the export matches the last one (the folder may have changed remotely).
/// `live` pushes only the live items and leaves the rest of the folder as the last Push left it.
pub fn sync_once(cfg: &Config, force: bool, live: bool) -> Result<Option<String>, String> {
    let items = if live { cfg.live_items() } else { cfg.items.clone() };
    let keep = |rel: &str| live && !items.get(wave::folder_category(rel)).copied().unwrap_or(false);
    with_lock(|| {
        let mut st = Status::load();
        st.last_check = repo::now();
        let res = wave::export(&items, &cfg.widgets).and_then(|files| {
            let hash = hash_of(&files, &cfg.target);
            if !force && hash == st.last_hash {
                return Ok(None);
            }
            st.last_hash = hash;
            let changed: Vec<String> = changed_files(&cfg.repo().dir.join(&cfg.target), &files);
            cfg.repo().save(&cfg.target, files, meta(), keep).map(|c| c.map(|c| (c, changed)))
        });
        let out = match res {
            Ok(Some((commit, changed))) => {
                st.last_push = repo::now();
                st.error.clear();
                let what = if changed.len() > 2 { format!("{} files", changed.len()) } else { changed.join(", ") };
                st.note(format!("{what} · push {commit}"));
                Ok(Some(commit))
            }
            Ok(None) => {
                st.error.clear();
                Ok(None)
            }
            Err(e) => {
                st.error = e.clone();
                st.last_hash = 0;
                Err(e)
            }
        };
        st.save();
        out
    })
}

fn hash_of(files: &wave::Files, target: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (files, target).hash(&mut h);
    h.finish()
}

fn changed_files(dir: &Path, files: &wave::Files) -> Vec<String> {
    files
        .iter()
        .filter(|(rel, b)| fs::read(dir.join(rel)).ok().as_ref() != Some(*b))
        .map(|(rel, _)| rel.rsplit('/').next().unwrap_or(rel).to_string())
        .collect()
}

/// Backs up this Wave, closes it, writes `profile` into it, reopens it and points future syncs at `save_to`.
pub fn apply_profile(profile: &str, save_to: &str) -> Result<Vec<String>, String> {
    let mut cfg = Config::load();
    let report = with_lock(|| -> Result<Vec<String>, String> {
        let repo = cfg.repo();
        repo.refresh()?;
        let src = repo.dir.join(profile);
        if !src.join("wavysync.json").exists() {
            return Err(format!("no profile named {profile}"));
        }
        let backup = dir().join("backups").join(repo::now().to_string());
        let all_widgets = BTreeMap::new();
        let local = wave::export(&cfg.items, &all_widgets)?;
        for (rel, bytes) in &local {
            let p = backup.join(rel);
            fs::create_dir_all(p.parent().unwrap()).map_err(|e| e.to_string())?;
            fs::write(p, bytes).map_err(|e| e.to_string())?;
        }
        // Layouts live in Wave's database, so writing them means closing Wave. Skip that when the
        // profile's layouts are the ones this Wave already has.
        let mut items = cfg.items.clone();
        let as_json = |b: &[u8]| serde_json::from_slice::<Value>(b).ok();
        let incoming = fs::read(src.join("workspaces.json")).ok();
        if incoming.as_deref().map(as_json) == Some(local.get("workspaces.json").and_then(|b| as_json(b))) || incoming.is_none() {
            items.insert("workspaces".into(), false);
        }
        let quit = items.get("workspaces").copied().unwrap_or(true);
        let was_running = wave::wave_running();
        if quit {
            wave::quit_wave()?;
        }
        let r = wave::apply(&src, &items, repo.token.as_deref());
        if quit && was_running {
            wave::launch_wave();
        }
        let mut r = r?;
        r.insert(0, format!("backup: {}", backup.display()));
        Ok(r)
    })?;
    cfg.target = save_to.to_string();
    cfg.save();
    let mut st = Status::load();
    st.last_pull = repo::now();
    st.note(if profile == save_to { format!("pulled {profile}") } else { format!("applied {profile}, saving to {save_to}") });
    st.save();
    Ok(report)
}

/// Brings the folder this computer saves to back into this Wave: what another computer pushed there.
pub fn pull() -> Result<Vec<String>, String> {
    let target = Config::load().target;
    apply_profile(&target, &target)
}

// Start at login, the Wave widget, and detached children ----------------------------------------

pub fn exe() -> String {
    std::env::current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| "wavysync".into())
}

/// Starts this program again with `args`, outside any terminal or Wave block that launched us.
pub fn spawn_detached(args: &[&str]) {
    let mut c = Command::new(exe());
    c.args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        c.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0000_0008 | 0x0000_0200); // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP
    }
    let _ = c.spawn();
}

pub fn install_autostart() -> Result<(), String> {
    let exe = exe();
    if cfg!(target_os = "macos") {
        let plist = wave::home().join("Library/LaunchAgents/app.wavysync.plist");
        let body = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict>\n<key>Label</key><string>app.wavysync</string>\n<key>ProgramArguments</key><array><string>{exe}</string><string>--daemon</string></array>\n<key>RunAtLoad</key><true/>\n<key>ProcessType</key><string>Background</string>\n</dict></plist>\n"
        );
        fs::create_dir_all(plist.parent().unwrap()).map_err(|e| e.to_string())?;
        fs::write(&plist, body).map_err(|e| e.to_string())?;
    } else if cfg!(windows) {
        let value = format!("\"{exe}\" --daemon");
        Command::new("reg")
            .args(["add", r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run", "/v", "WavySync", "/t", "REG_SZ", "/d", &value, "/f"])
            .status()
            .map_err(|e| e.to_string())?;
    } else {
        let f = wave::home().join(".config/autostart/wavysync.desktop");
        fs::create_dir_all(f.parent().unwrap()).map_err(|e| e.to_string())?;
        fs::write(&f, format!("[Desktop Entry]\nType=Application\nName=WavySync\nExec=\"{exe}\" --daemon\nX-GNOME-Autostart-enabled=true\n"))
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Adds WavySync's own widget to Wave's sidebar; it opens this window.
pub fn install_widget() -> Result<(), String> {
    let cfg_dir = wave::paths().config;
    let mut w = wave::read_widgets(&cfg_dir);
    w.insert(
        OWN_WIDGET.into(),
        json!({
            "display:order": 99, "icon": "water", "label": "WavySync",
            "description": "Sync this Wave to GitHub, or bring another computer's Wave here",
            "blockdef": { "meta": { "view": "term", "controller": "cmd", "cmd": exe(), "cmd:args": ["--open"],
                "cmd:shell": false, "cmd:closeonexit": true, "cmd:closeonexitdelay": 0 } }
        }),
    );
    fs::create_dir_all(&cfg_dir).map_err(|e| e.to_string())?;
    fs::write(cfg_dir.join("widgets.json"), serde_json::to_vec_pretty(&Value::Object(w)).unwrap()).map_err(|e| e.to_string())
}

/// The background loop: checks every `interval` seconds while auto sync is on. One instance per user.
pub fn daemon() {
    let _ = fs::create_dir_all(dir());
    let Ok(lock) = fs::OpenOptions::new().create(true).truncate(false).write(true).open(dir().join("daemon.lock")) else { return };
    if lock.try_lock().is_err() {
        return;
    }
    loop {
        let cfg = Config::load();
        if cfg.setup_done && cfg.auto && !cfg.repo.is_empty() {
            let _ = sync_once(&cfg, false, true);
        }
        std::thread::sleep(std::time::Duration::from_secs(cfg.interval.max(10)));
    }
}
