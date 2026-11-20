//! Wave widgets as extensions: what each one needs on disk, and installing that on another computer.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::wave::{HOME_TOKEN, detokenize, home, home_str, tokenize};

const BUNDLE_LIMIT: u64 = 2 * 1024 * 1024;
const SYSTEM_DIRS: [&str; 7] = ["/usr/bin/", "/bin/", "/usr/sbin/", "/sbin/", "/System/", "/usr/libexec/", "C:/Windows/"];

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Ext {
    pub id: String,
    pub label: String,
    pub deps: Vec<Dep>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Dep {
    Git { url: String, path: String, build: Vec<String> },
    Brew { formula: String },
    Npm { package: String },
    Cargo { name: String },
    /// A file copied as is; its bytes live at `files/<path without {HOME}/>` in the profile.
    File { path: String, exec: bool },
    Manual { note: String },
}

impl Dep {
    pub fn summary(&self) -> String {
        match self {
            Dep::Git { url, build, .. } => {
                let repo = url.trim_end_matches(".git").rsplit('/').take(2).collect::<Vec<_>>();
                let name = repo.into_iter().rev().collect::<Vec<_>>().join("/");
                if build.is_empty() { format!("git {name}") } else { format!("git {name} + {}", build.join(", ")) }
            }
            Dep::Brew { formula } => format!("brew {formula}"),
            Dep::Npm { package } => format!("npm -g {package}"),
            Dep::Cargo { name } => format!("cargo {name}"),
            Dep::File { path, .. } => format!("file {}", path.replace(HOME_TOKEN, "~")),
            Dep::Manual { note } => note.clone(),
        }
    }
}

fn git_out(dir: &Path, args: &[&str]) -> Option<String> {
    let o = Command::new("git").arg("-C").arg(dir).args(args).output().ok()?;
    o.status.success().then(|| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

fn git_dep(dir: &Path) -> Option<Dep> {
    let top = PathBuf::from(git_out(dir, &["rev-parse", "--show-toplevel"])?);
    let url = git_out(&top, &["remote", "get-url", "origin"])?;
    Some(Dep::Git { url, path: tokenize(&top.to_string_lossy().replace('\\', "/")), build: build_steps(&top) })
}

pub fn build_steps(dir: &Path) -> Vec<String> {
    let has = |f: &str| dir.join(f).exists();
    let mut steps = Vec::new();
    if has("package.json") {
        steps.push(if has("package-lock.json") { "npm ci" } else if has("pnpm-lock.yaml") { "pnpm install" } else { "npm install" }.into());
        let pkg: Value = fs::read(dir.join("package.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        if pkg["scripts"]["build"].is_string() {
            steps.push("npm run build".into());
        }
    } else if has("Cargo.toml") {
        steps.push("cargo build --release".into());
    } else if has("go.mod") {
        steps.push("go build ./...".into());
    } else if has("requirements.txt") {
        steps.push("pip install -r requirements.txt".into());
    }
    steps
}

fn which(cmd: &str) -> Option<PathBuf> {
    let p = Path::new(cmd);
    if p.is_absolute() {
        return p.exists().then(|| p.to_path_buf());
    }
    let exts: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT").unwrap_or(".EXE;.CMD;.BAT".into()).split(';').map(|e| e.to_lowercase()).collect()
    } else {
        vec![String::new()]
    };
    std::env::split_paths(&std::env::var_os("PATH")?)
        .flat_map(|d| exts.iter().map(move |e| d.join(format!("{cmd}{e}"))))
        .find(|c| c.is_file())
}

/// Home-relative paths a wrapper script points at: `$HOME/x`, `${HOME}/x`, `~/x`, `%USERPROFILE%\x` or the literal home.
fn script_paths(text: &str) -> Vec<PathBuf> {
    let h = home_str();
    let mut out = Vec::new();
    for prefix in ["$HOME/", "${HOME}/", "~/", "%USERPROFILE%\\", "%USERPROFILE%/"].iter().map(|s| s.to_string()).chain([format!("{h}/")]) {
        for (i, _) in text.match_indices(&prefix) {
            let rest = &text[i + prefix.len()..];
            let end = rest.find(|c: char| c.is_whitespace() || "\"'`;)|&".contains(c)).unwrap_or(rest.len());
            let mut p = home().join(rest[..end].replace('\\', "/"));
            while !p.exists() && p.pop() {}
            if p != home() && p.starts_with(home()) {
                out.push(p);
            }
        }
    }
    out
}

fn add(deps: &mut Vec<Dep>, d: Dep) {
    if !deps.contains(&d) {
        deps.push(d);
    }
}

fn bundle(path: &Path, deps: &mut Vec<Dep>, files: &mut BTreeMap<String, Vec<u8>>, budget: &mut u64) {
    if path.is_dir() {
        let Ok(rd) = fs::read_dir(path) else { return };
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            if n.starts_with('.') || n == "node_modules" || n == "target" {
                continue;
            }
            bundle(&e.path(), deps, files, budget);
        }
        return;
    }
    let Ok(rel) = path.strip_prefix(home()) else { return };
    let Ok(bytes) = fs::read(path) else { return };
    if bytes.len() as u64 > *budget {
        add(deps, Dep::Manual { note: format!("too big to carry: {}", path.display()) });
        return;
    }
    *budget -= bytes.len() as u64;
    let rel = rel.to_string_lossy().replace('\\', "/");
    let exec = is_exec(path);
    let bytes = match String::from_utf8(bytes) {
        Ok(s) => tokenize(&s).into_bytes(),
        Err(e) => e.into_bytes(),
    };
    files.insert(rel.clone(), bytes);
    add(deps, Dep::File { path: format!("{HOME_TOKEN}/{rel}"), exec });
}

#[cfg(unix)]
fn is_exec(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(p).map(|m| m.permissions().mode() & 0o111 != 0).unwrap_or(false)
}
#[cfg(not(unix))]
fn is_exec(_: &Path) -> bool {
    false
}

fn local_dir_deps(dir: &Path, deps: &mut Vec<Dep>, files: &mut BTreeMap<String, Vec<u8>>, budget: &mut u64) {
    match git_dep(dir) {
        Some(d) => add(deps, d),
        None if dir.starts_with(home()) => bundle(dir, deps, files, budget),
        None => add(deps, Dep::Manual { note: format!("outside home: {}", dir.display()) }),
    }
}

fn command_deps(cmd: &str, deps: &mut Vec<Dep>, files: &mut BTreeMap<String, Vec<u8>>, budget: &mut u64) {
    let Some(found) = which(cmd) else { return };
    let real = fs::canonicalize(&found).unwrap_or(found.clone());
    let s = real.to_string_lossy().replace('\\', "/");
    if SYSTEM_DIRS.iter().any(|d| s.starts_with(d)) || s.starts_with("/Applications/") {
        return;
    }
    let after = |marker: &str| s.split_once(marker).map(|(_, r)| r.to_string());
    if let Some(r) = after("/Cellar/") {
        return add(deps, Dep::Brew { formula: r.split('/').next().unwrap_or_default().into() });
    }
    if let Some(r) = s.rfind("/node_modules/").map(|i| s[i + 14..].to_string()) {
        let mut parts = r.split('/');
        let first = parts.next().unwrap_or_default();
        let package = if first.starts_with('@') { format!("{first}/{}", parts.next().unwrap_or_default()) } else { first.into() };
        return add(deps, Dep::Npm { package });
    }
    if real.starts_with(home().join(".cargo/bin")) {
        return add(deps, Dep::Cargo { name: real.file_stem().unwrap().to_string_lossy().into() });
    }
    let small_text = fs::metadata(&real).map(|m| m.len() < 64 * 1024).unwrap_or(false)
        && fs::read_to_string(&real).is_ok();
    if small_text {
        let text = fs::read_to_string(&real).unwrap();
        for p in script_paths(&text) {
            local_dir_deps(if p.is_dir() { &p } else { p.parent().unwrap() }, deps, files, budget);
        }
        if found.starts_with(home()) {
            bundle(&found, deps, files, budget);
        }
        return;
    }
    local_dir_deps(real.parent().unwrap(), deps, files, budget);
}

/// Extension list for the given widgets plus the bytes of files that travel inside the profile.
pub fn detect(widgets: &Map<String, Value>) -> (Vec<Ext>, BTreeMap<String, Vec<u8>>) {
    let mut files = BTreeMap::new();
    let mut budget = BUNDLE_LIMIT;
    let mut exts = Vec::new();
    for (id, w) in widgets {
        let meta = &w["blockdef"]["meta"];
        let label = w["label"].as_str().unwrap_or(id).to_string();
        let mut deps = Vec::new();
        if let Some(url) = meta["url"].as_str().and_then(|u| u.strip_prefix("file://")) {
            let path = url.split(['?', '#']).next().unwrap_or_default().replace("%20", " ");
            let p = PathBuf::from(path);
            local_dir_deps(if p.is_dir() { &p } else { p.parent().unwrap_or(&p) }, &mut deps, &mut files, &mut budget);
        }
        if let Some(cmd) = meta["cmd"].as_str() {
            command_deps(cmd, &mut deps, &mut files, &mut budget);
        }
        // A web widget often has a CLI of the same name next to it (opencom, for one).
        for name in [id.as_str(), label.as_str()] {
            if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
                command_deps(name, &mut deps, &mut files, &mut budget);
            }
        }
        exts.push(Ext { id: id.clone(), label, deps });
    }
    (exts, files)
}

fn run(dir: Option<&Path>, line: &str) -> Result<(), String> {
    let mut c = if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.args(["/C", line]);
        c
    } else {
        let mut c = Command::new("sh");
        c.args(["-c", line]);
        c
    };
    if let Some(d) = dir {
        c.current_dir(d);
    }
    let o = c.output().map_err(|e| format!("{line}: {e}"))?;
    if o.status.success() {
        return Ok(());
    }
    let err = String::from_utf8_lossy(&o.stderr);
    Err(format!("{line}: {}", err.lines().last().unwrap_or("failed")))
}

fn have(cmd: &str) -> bool {
    which(cmd).is_some()
}

/// Installs what is missing. Returns one line per dependency for the report.
pub fn install(e: &Ext, files_dir: &Path, token: Option<&str>) -> Vec<String> {
    let mut out = Vec::new();
    for d in &e.deps {
        let res: Result<&str, String> = match d {
            Dep::Git { url, path, build } => {
                let target = PathBuf::from(detokenize(path));
                if target.exists() {
                    Ok("already there")
                } else {
                    crate::repo::clone(url, &target, token).and_then(|_| {
                        build.iter().try_for_each(|b| run(Some(&target), b)).map(|_| if build.is_empty() { "cloned" } else { "cloned and built" })
                    })
                }
            }
            Dep::Brew { formula } if have("brew") => {
                if run(None, &format!("brew list --formula {formula}")).is_ok() {
                    Ok("already there")
                } else {
                    run(None, &format!("brew install {formula}")).map(|_| "installed")
                }
            }
            Dep::Brew { formula } => Err(format!("no Homebrew here: install {formula} by hand")),
            Dep::Npm { package } => {
                if run(None, &format!("npm ls -g {package}")).is_ok() {
                    Ok("already there")
                } else {
                    run(None, &format!("npm install -g {package}")).map(|_| "installed")
                }
            }
            Dep::Cargo { name } => {
                if home().join(".cargo/bin").join(name).exists() || have(name) {
                    Ok("already there")
                } else {
                    run(None, &format!("cargo install {name}")).map(|_| "installed")
                }
            }
            Dep::File { path, exec } => write_file(path, *exec, files_dir).map(|_| "written"),
            Dep::Manual { note } => Err(format!("by hand: {note}")),
        };
        out.push(match res {
            Ok(s) => format!("{} · {}: {s}", e.label, d.summary()),
            Err(s) => format!("{} · {}: FAILED {s}", e.label, d.summary()),
        });
    }
    out
}

fn write_file(path: &str, exec: bool, files_dir: &Path) -> Result<(), String> {
    let rel = path.trim_start_matches(HOME_TOKEN).trim_start_matches('/');
    let bytes = fs::read(files_dir.join(rel)).map_err(|e| e.to_string())?;
    let bytes = match String::from_utf8(bytes) {
        Ok(s) => detokenize(&s).into_bytes(),
        Err(e) => e.into_bytes(),
    };
    let dst = PathBuf::from(detokenize(path));
    fs::create_dir_all(dst.parent().unwrap()).map_err(|e| e.to_string())?;
    fs::write(&dst, bytes).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    if exec {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&dst, fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
    }
    let _ = exec;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_paths_follow_home_forms() {
        let h = home();
        let found = script_paths("exec node \"$HOME/.\" ~/. x");
        assert!(found.is_empty(), "home itself is never a dependency: {found:?}");
        let dir = h.join(".cargo");
        if dir.exists() {
            assert_eq!(script_paths("run ${HOME}/.cargo/nope/deeper.js"), vec![dir]);
        }
    }

    #[test]
    fn build_steps_for_node() {
        let d = std::env::temp_dir().join(format!("wvysy-build-{}", std::process::id()));
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("package.json"), r#"{"scripts":{"build":"tsc"}}"#).unwrap();
        fs::write(d.join("package-lock.json"), "{}").unwrap();
        assert_eq!(build_steps(&d), vec!["npm ci", "npm run build"]);
        fs::remove_dir_all(&d).unwrap();
    }
}
