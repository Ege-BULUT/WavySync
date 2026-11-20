//! Reading Wave Terminal's state into a profile folder and writing a profile back.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use rusqlite::{Connection, OpenFlags};
use serde_json::{Map, Value};

use crate::ext;

pub const HOME_TOKEN: &str = "{HOME}";
pub const OWN_WIDGET: &str = "wavysync";
/// Rows are JSON documents keyed by oid; these four make up workspaces, tabs and their layouts.
const TABLES: [(&str, &str); 4] =
    [("db_workspace", "workspaces"), ("db_tab", "tabs"), ("db_block", "blocks"), ("db_layout", "layouts")];
const SECRET_WORDS: [&str; 7] = ["token", "secret", "password", "passwd", "apikey", "api_key", "privatekey"];

pub type Files = BTreeMap<String, Vec<u8>>;

pub struct WavePaths {
    pub config: PathBuf,
    pub data: PathBuf,
}

pub fn home() -> PathBuf {
    #[allow(deprecated)]
    std::env::home_dir().expect("no home directory")
}

/// Forward slashes on every OS, so `{HOME}` maps the same way in paths and file:// URLs.
pub fn home_str() -> String {
    home().to_string_lossy().replace('\\', "/")
}

fn env_path(key: &str) -> Option<PathBuf> {
    std::env::var_os(key).filter(|v| !v.is_empty()).map(PathBuf::from)
}

/// Same lookup order as Wave's emain-platform.ts.
pub fn paths() -> WavePaths {
    let legacy = env_path("WAVETERM_HOME").unwrap_or_else(|| home().join(".waveterm"));
    if legacy.join("wave.lock").exists() {
        return WavePaths { config: legacy.join("config"), data: legacy };
    }
    let config = env_path("WAVETERM_CONFIG_HOME")
        .or_else(|| env_path("XDG_CONFIG_HOME").map(|p| p.join("waveterm")))
        .unwrap_or_else(|| home().join(".config").join("waveterm"));
    let data = env_path("WAVETERM_DATA_HOME")
        .or_else(|| env_path("XDG_DATA_HOME").map(|p| p.join("waveterm")))
        .unwrap_or_else(|| {
            if cfg!(target_os = "macos") {
                home().join("Library/Application Support/waveterm")
            } else if cfg!(windows) {
                env_path("LOCALAPPDATA").unwrap_or_else(|| home().join("AppData/Local")).join("waveterm/Data")
            } else {
                home().join(".local/share/waveterm")
            }
        });
    WavePaths { config, data }
}

/// Which sync item a config file belongs to.
pub fn category(rel: &str) -> &'static str {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    match name {
        "settings.json" => "settings",
        "widgets.json" => "widgets",
        "connections.json" => "connections",
        _ if name.starts_with("waveai") || name.starts_with("ai") => "ai",
        _ => "themes",
    }
}

pub fn tokenize(text: &str) -> String {
    let h = home_str();
    text.replace(&h, HOME_TOKEN).replace(&h.replace('/', "\\\\"), HOME_TOKEN)
}

pub fn detokenize(text: &str) -> String {
    text.replace(HOME_TOKEN, &home_str())
}

fn strip_secrets(v: &mut Value) {
    match v {
        Value::Object(m) => {
            m.retain(|k, _| {
                let k = k.to_lowercase();
                !SECRET_WORDS.iter().any(|w| k.contains(w))
            });
            m.values_mut().for_each(strip_secrets);
        }
        Value::Array(a) => a.iter_mut().for_each(strip_secrets),
        _ => {}
    }
}

fn pretty(v: &Value) -> Vec<u8> {
    let mut s = serde_json::to_string_pretty(v).unwrap();
    s.push('\n');
    tokenize(&s).into_bytes()
}

fn walk(dir: &Path, base: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        let name = e.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        if p.is_dir() {
            walk(&p, base, out);
        } else if name.ends_with(".json") {
            out.push(p.strip_prefix(base).unwrap().to_path_buf());
        }
    }
}

fn rel_str(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

pub fn read_widgets(config: &Path) -> Map<String, Value> {
    fs::read(config.join("widgets.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

/// Everything the selected items cover, as files relative to a profile folder.
pub fn export(items: &BTreeMap<String, bool>, widgets_on: &BTreeMap<String, bool>) -> Result<Files, String> {
    let wp = paths();
    let on = |k: &str| items.get(k).copied().unwrap_or(true);
    let mut files = Files::new();
    let mut found = Vec::new();
    walk(&wp.config, &wp.config, &mut found);
    for rel in found {
        let r = rel_str(&rel);
        let cat = category(&r);
        if !on(cat) {
            continue;
        }
        let Ok(bytes) = fs::read(wp.config.join(&rel)) else { continue };
        let Ok(mut v) = serde_json::from_slice::<Value>(&bytes) else { continue };
        strip_secrets(&mut v);
        if cat == "widgets" {
            if let Value::Object(m) = &mut v {
                m.retain(|k, _| k != OWN_WIDGET && widgets_on.get(k).copied().unwrap_or(true));
            }
        }
        files.insert(format!("config/{r}"), pretty(&v));
    }
    if on("widgets") {
        let mut widgets = read_widgets(&wp.config);
        widgets.retain(|k, _| k != OWN_WIDGET && widgets_on.get(k).copied().unwrap_or(true));
        let (exts, bundled) = ext::detect(&widgets);
        files.insert("extensions.json".into(), pretty(&serde_json::to_value(&exts).unwrap()));
        for (k, b) in bundled {
            files.insert(format!("files/{k}"), b);
        }
    }
    if on("workspaces") {
        let db = wp.data.join("db/waveterm.db");
        if db.exists() {
            files.insert("workspaces.json".into(), pretty(&read_workspaces(&db)?));
        }
    }
    Ok(files)
}

/// Reads through SQLite (WAL readers never block Wave's writer), so the rows are one consistent snapshot.
fn read_workspaces(db: &Path) -> Result<Value, String> {
    let c = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(|e| e.to_string())?;
    let mut out = Map::new();
    for (table, key) in TABLES {
        let mut st = c.prepare(&format!("SELECT data FROM {table} ORDER BY oid")).map_err(|e| e.to_string())?;
        let rows: Vec<Value> = st
            .query_map([], |r| r.get::<_, Vec<u8>>(0))
            .map_err(|e| e.to_string())?
            .flatten()
            .filter_map(|b| serde_json::from_slice::<Value>(&b).ok())
            .map(clean_row)
            .collect();
        out.insert(key.into(), Value::Array(rows));
    }
    let mut st = c.prepare("SELECT data FROM db_window ORDER BY oid").map_err(|e| e.to_string())?;
    let shown: Vec<Value> = st
        .query_map([], |r| r.get::<_, Vec<u8>>(0))
        .map_err(|e| e.to_string())?
        .flatten()
        .filter_map(|b| serde_json::from_slice::<Value>(&b).ok())
        .map(|w| w["workspaceid"].clone())
        .collect();
    out.insert("shown".into(), Value::Array(shown));
    Ok(Value::Object(out))
}

/// Drops what changes on every keystroke or resize and what is private browsing history.
fn clean_row(mut v: Value) -> Value {
    if let Value::Object(m) = &mut v {
        m.remove("version");
        m.remove("runtimeopts");
        if let Some(Value::Object(meta)) = m.get_mut("meta") {
            meta.remove("history");
            meta.remove("history:forward");
        }
    }
    v
}

pub fn wave_running() -> bool {
    let lock = paths().data.join("wave.lock");
    let Ok(f) = fs::OpenOptions::new().read(true).write(true).open(&lock) else { return false };
    let busy = f.try_lock().is_err();
    if !busy {
        let _ = f.unlock();
    }
    busy
}

pub fn quit_wave() -> Result<(), String> {
    if !wave_running() {
        return Ok(());
    }
    let _ = if cfg!(target_os = "macos") {
        Command::new("osascript").args(["-e", "quit app \"Wave\""]).status()
    } else if cfg!(windows) {
        Command::new("taskkill").args(["/IM", "Wave.exe"]).status()
    } else {
        Command::new("pkill").args(["-x", "waveterm"]).status()
    };
    let start = Instant::now();
    while wave_running() {
        if start.elapsed() > Duration::from_secs(60) {
            return Err("Wave did not close within 60 s".into());
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    Ok(())
}

pub fn launch_wave() {
    let _ = if cfg!(target_os = "macos") {
        Command::new("open").args(["-a", "Wave"]).status()
    } else if cfg!(windows) {
        let exe = env_path("LOCALAPPDATA").unwrap_or_default().join("Programs/waveterm/Wave.exe");
        Command::new(exe).spawn().map(|_| Default::default())
    } else {
        Command::new("waveterm").spawn().map(|_| Default::default())
    };
}

/// Writes a profile folder into this computer's Wave. Wave must be closed when workspaces are included.
pub fn apply(profile: &Path, items: &BTreeMap<String, bool>, token: Option<&str>) -> Result<Vec<String>, String> {
    let wp = paths();
    let on = |k: &str| items.get(k).copied().unwrap_or(true);
    let mut report = Vec::new();
    let cfg_root = profile.join("config");
    let mut incoming = Vec::new();
    walk(&cfg_root, &cfg_root, &mut incoming);
    let incoming: BTreeSet<String> = incoming.iter().map(|p| rel_str(p)).filter(|r| on(category(r))).collect();

    let mut local = Vec::new();
    walk(&wp.config, &wp.config, &mut local);
    for rel in local.iter().map(|p| rel_str(p)) {
        let cat = category(&rel);
        if on(cat) && cat != "widgets" && !incoming.contains(&rel) {
            let _ = fs::remove_file(wp.config.join(&rel));
        }
    }
    let own = read_widgets(&wp.config).remove(OWN_WIDGET);
    for rel in &incoming {
        let text = fs::read_to_string(cfg_root.join(rel)).map_err(|e| e.to_string())?;
        let mut out = detokenize(&text);
        if rel == "widgets.json" {
            if let (Some(own), Ok(Value::Object(mut m))) = (own.clone(), serde_json::from_str::<Value>(&out)) {
                m.insert(OWN_WIDGET.into(), own);
                out = serde_json::to_string_pretty(&Value::Object(m)).unwrap();
            }
        }
        let dst = wp.config.join(rel);
        fs::create_dir_all(dst.parent().unwrap()).map_err(|e| e.to_string())?;
        fs::write(&dst, out).map_err(|e| e.to_string())?;
        report.push(format!("config/{rel}"));
    }
    if on("workspaces") {
        if let Ok(text) = fs::read_to_string(profile.join("workspaces.json")) {
            let v: Value = serde_json::from_str(&detokenize(&text)).map_err(|e| e.to_string())?;
            write_workspaces(&wp.data.join("db/waveterm.db"), &v)?;
            report.push("workspaces, tabs, layouts".into());
        }
    }
    if on("widgets") {
        if let Ok(text) = fs::read_to_string(profile.join("extensions.json")) {
            let exts: Vec<ext::Ext> = serde_json::from_str(&text).map_err(|e| e.to_string())?;
            for e in &exts {
                report.extend(ext::install(e, &profile.join("files"), token));
            }
        }
    }
    Ok(report)
}

fn write_workspaces(db: &Path, v: &Value) -> Result<(), String> {
    let mut c = Connection::open(db).map_err(|e| e.to_string())?;
    let tx = c.transaction().map_err(|e| e.to_string())?;
    let err = |e: rusqlite::Error| e.to_string();
    for (table, key) in TABLES {
        tx.execute(&format!("DELETE FROM {table}"), []).map_err(err)?;
        for row in v[key].as_array().into_iter().flatten() {
            let mut row = row.clone();
            row["version"] = Value::from(1);
            let oid = row["oid"].as_str().unwrap_or_default().to_string();
            tx.execute(&format!("INSERT INTO {table} (oid, version, data) VALUES (?1, 1, ?2)"), (oid, row.to_string().into_bytes()))
                .map_err(err)?;
        }
    }
    // ponytail: every window but the first is dropped and that one shows the workspace the source showed;
    // the rest stay in Wave's workspace switcher.
    let has = |id: &str| v["workspaces"].as_array().into_iter().flatten().any(|w| w["oid"] == id);
    let shown = v["shown"].as_array().into_iter().flatten().filter_map(|w| w.as_str()).find(|w| has(w));
    let Some(first) = shown.or(v["workspaces"][0]["oid"].as_str()) else { return tx.commit().map_err(err) };
    // Wave stores each document as a JSON blob.
    let windows: Vec<(String, Vec<u8>)> = {
        let mut st = tx.prepare("SELECT oid, data FROM db_window ORDER BY oid").map_err(err)?;
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).map_err(err)?.flatten().collect()
    };
    let keep = windows.first().map(|w| w.0.clone());
    for (oid, data) in &windows {
        if Some(oid) != keep.as_ref() {
            tx.execute("DELETE FROM db_window WHERE oid = ?1", [oid]).map_err(err)?;
            continue;
        }
        let mut w: Value = serde_json::from_slice(data).map_err(|e| e.to_string())?;
        w["workspaceid"] = Value::from(first);
        tx.execute("UPDATE db_window SET data = ?1 WHERE oid = ?2", (w.to_string().into_bytes(), oid)).map_err(err)?;
    }
    let client: Option<(String, Vec<u8>)> =
        tx.query_row("SELECT oid, data FROM db_client LIMIT 1", [], |r| Ok((r.get(0)?, r.get(1)?))).ok();
    if let Some((oid, data)) = client {
        let mut cl: Value = serde_json::from_slice(&data).map_err(|e| e.to_string())?;
        cl["windowids"] = Value::Array(keep.into_iter().map(Value::from).collect());
        tx.execute("UPDATE db_client SET data = ?1 WHERE oid = ?2", (cl.to_string().into_bytes(), oid)).map_err(err)?;
    }
    tx.commit().map_err(err)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_are_stripped_at_any_depth() {
        let mut v = serde_json::json!({"ai:apitoken": "x", "term:fontsize": 12, "m": {"Password": 1, "keep": [{"secretname": 2, "a": 3}]}});
        strip_secrets(&mut v);
        assert_eq!(v, serde_json::json!({"term:fontsize": 12, "m": {"keep": [{"a": 3}]}}));
    }

    #[test]
    fn home_round_trips() {
        let s = format!("file://{}/x and {}", home_str(), home_str().replace('/', "\\\\"));
        let t = tokenize(&s);
        assert!(!t.contains(&home_str()) && t.contains(HOME_TOKEN));
        assert!(detokenize(&t).starts_with(&format!("file://{}/x", home_str())));
    }

    #[test]
    fn categories() {
        assert_eq!(category("settings.json"), "settings");
        assert_eq!(category("presets/ai.json"), "ai");
        assert_eq!(category("termthemes.json"), "themes");
    }
}
