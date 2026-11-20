//! The sync repo: a git working copy WavySync owns, and the GitHub calls around it.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

use crate::wave::Files;

pub const API: &str = "https://api.github.com";
const META: &str = "wavysync.json";

fn b64(input: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for c in input.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            out.push(if i <= c.len() { T[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
        }
    }
    out
}

/// git with the token passed as an HTTP header through the environment, so it never lands in
/// `.git/config` or in the process list. Without a token git uses this computer's own credentials.
fn git(dir: &Path, args: &[&str], token: Option<&str>) -> Result<String, String> {
    let mut c = Command::new("git");
    c.args(args).current_dir(dir).env("GIT_TERMINAL_PROMPT", "0");
    if let Some(t) = token {
        c.env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "http.https://github.com/.extraheader")
            .env("GIT_CONFIG_VALUE_0", format!("AUTHORIZATION: basic {}", b64(format!("x-access-token:{t}").as_bytes())));
    }
    let o = c.output().map_err(|e| format!("git: {e}"))?;
    if o.status.success() {
        return Ok(String::from_utf8_lossy(&o.stdout).trim().to_string());
    }
    let err = String::from_utf8_lossy(&o.stderr).trim().to_string();
    Err(format!("git {}: {}", args.first().unwrap_or(&""), err.lines().last().unwrap_or("failed")))
}

fn is_github(url: &str) -> bool {
    url.starts_with("https://github.com/")
}

pub fn clone(url: &str, target: &Path, token: Option<&str>) -> Result<(), String> {
    fs::create_dir_all(target.parent().unwrap()).map_err(|e| e.to_string())?;
    let t = token.filter(|_| is_github(url));
    git(target.parent().unwrap(), &["clone", "--quiet", url, &target.to_string_lossy()], t).map(|_| ())
}

pub fn normalize(url: &str) -> String {
    let u = url.trim().trim_end_matches('/').trim_end_matches(".git");
    if u.starts_with("http") || u.starts_with("git@") || u.starts_with("file:") || u.starts_with('/') {
        u.to_string()
    } else {
        format!("https://github.com/{u}")
    }
}

pub struct Repo {
    pub dir: PathBuf,
    pub url: String,
    pub token: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct Profile {
    pub name: String,
    pub host: String,
    pub os: String,
    pub updated: u64,
}

impl Repo {
    fn git(&self, args: &[&str]) -> Result<String, String> {
        git(&self.dir, args, self.token.as_deref().filter(|_| is_github(&self.url)))
    }

    /// Clones on first use, then matches the remote exactly. The working copy is WavySync's own,
    /// so nothing in it is ever someone's unsaved work.
    pub fn refresh(&self) -> Result<(), String> {
        let current = self.dir.join(".git").exists().then(|| self.git(&["remote", "get-url", "origin"]).ok()).flatten();
        if current.as_deref().map(normalize) != Some(normalize(&self.url)) {
            let _ = fs::remove_dir_all(&self.dir);
            clone(&self.url, &self.dir, self.token.as_deref())?;
        }
        self.git(&["fetch", "--quiet", "origin"])?;
        let head = self.git(&["symbolic-ref", "--short", "refs/remotes/origin/HEAD"]).ok();
        let branch = head.or_else(|| self.git(&["rev-parse", "--abbrev-ref", "origin/main"]).ok());
        match branch {
            Some(b) => self.git(&["reset", "--hard", "--quiet", &b]).map(|_| ()),
            None => Ok(()),
        }
    }

    pub fn profiles(&self) -> Vec<Profile> {
        let mut out: Vec<Profile> = fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let meta: Value = serde_json::from_slice(&fs::read(e.path().join(META)).ok()?).ok()?;
                Some(Profile {
                    name: e.file_name().to_string_lossy().into(),
                    host: meta["host"].as_str().unwrap_or_default().into(),
                    os: meta["os"].as_str().unwrap_or_default().into(),
                    updated: meta["updated"].as_u64().unwrap_or(0),
                })
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Replaces `folder` with `files` and pushes; files already there for which `keep` says yes stay
    /// as they are. Returns the new commit, or None when nothing changed.
    pub fn save(&self, folder: &str, mut files: Files, meta: Value, keep: impl Fn(&str) -> bool) -> Result<Option<String>, String> {
        if folder.is_empty() || folder.contains(['/', '\\']) || folder.starts_with('.') {
            return Err(format!("bad folder name: {folder:?}"));
        }
        for attempt in 0..2 {
            self.refresh()?;
            let dir = self.dir.join(folder);
            let old_meta = fs::read(dir.join(META)).ok();
            let mut old = Vec::new();
            crate::wave::walk(&dir, &dir, &mut old);
            for rel in old.iter().map(|p| crate::wave::rel_str(p)) {
                if rel != META && keep(&rel) && !files.contains_key(&rel) {
                    if let Ok(b) = fs::read(dir.join(&rel)) {
                        files.insert(rel, b);
                    }
                }
            }
            let _ = fs::remove_dir_all(&dir);
            if let Some(old) = &old_meta {
                files.insert(META.into(), old.clone());
            }
            write_all(&dir, &files)?;
            self.git(&["add", "-A", "--", folder])?;
            if self.git(&["diff", "--cached", "--quiet"]).is_ok() {
                return Ok(None);
            }
            let mut m = meta.clone();
            m["updated"] = Value::from(now());
            files.insert(META.into(), serde_json::to_vec_pretty(&m).unwrap());
            write_all(&dir, &files)?;
            self.git(&["add", "-A", "--", folder])?;
            let ident = ["-c", "user.name=WavySync", "-c", "user.email=wavysync@users.noreply.github.com"];
            let has_ident = self.git(&["config", "user.email"]).is_ok();
            let msg = format!("{folder}: sync from {}", m["host"].as_str().unwrap_or("?"));
            let mut args: Vec<&str> = if has_ident { vec![] } else { ident.to_vec() };
            args.extend(["commit", "--quiet", "-m", &msg]);
            self.git(&args)?;
            if self.git(&["branch", "--show-current"]).map(|b| b.is_empty()).unwrap_or(true) {
                self.git(&["checkout", "--quiet", "-B", "main"])?;
            }
            match self.git(&["push", "--quiet", "origin", "HEAD"]) {
                Ok(_) => return self.git(&["rev-parse", "--short", "HEAD"]).map(Some),
                Err(e) if attempt == 0 && (e.contains("rejected") || e.contains("fetch first")) => continue,
                Err(e) => return Err(e),
            }
        }
        Err("push kept being rejected".into())
    }
}

fn write_all(dir: &Path, files: &Files) -> Result<(), String> {
    for (rel, bytes) in files {
        let p = dir.join(rel);
        fs::create_dir_all(p.parent().unwrap()).map_err(|e| e.to_string())?;
        fs::write(&p, bytes).map_err(|e| e.to_string())?;
    }
    Ok(())
}

pub fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

// GitHub ---------------------------------------------------------------------------------------

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(20))).http_status_as_error(false).build().into()
}

fn api(method: &str, path: &str, token: &str, body: Option<Value>) -> Result<Value, String> {
    let url = format!("{API}{path}");
    let a = agent();
    let auth = format!("Bearer {token}");
    let res = match (method, body) {
        ("POST", Some(b)) => a.post(&url).header("Authorization", &auth).header("User-Agent", "WavySync").send_json(b),
        _ => a.get(&url).header("Authorization", &auth).header("User-Agent", "WavySync").call(),
    }
    .map_err(|e| e.to_string())?;
    let status = res.status().as_u16();
    let v: Value = res.into_body().read_json().unwrap_or_default();
    if status >= 300 {
        return Err(format!("GitHub {status}: {}", v["message"].as_str().unwrap_or("error")));
    }
    Ok(v)
}

pub fn user(token: &str) -> Result<String, String> {
    api("GET", "/user", token, None).map(|v| v["login"].as_str().unwrap_or_default().to_string())
}

#[derive(Clone, Debug)]
pub struct RepoInfo {
    pub full_name: String,
    pub private: bool,
}

pub fn repos(token: &str) -> Result<Vec<RepoInfo>, String> {
    let mut out = Vec::new();
    for page in 1..=3 {
        let v = api("GET", &format!("/user/repos?per_page=100&sort=updated&page={page}"), token, None)?;
        let arr = v.as_array().cloned().unwrap_or_default();
        out.extend(arr.iter().map(|r| RepoInfo {
            full_name: r["full_name"].as_str().unwrap_or_default().into(),
            private: r["private"].as_bool().unwrap_or(false),
        }));
        if arr.len() < 100 {
            break;
        }
    }
    Ok(out)
}

pub fn create_repo(token: &str, name: &str, private: bool) -> Result<String, String> {
    let body = serde_json::json!({"name": name, "private": private, "auto_init": true,
        "description": "Wave Terminal profiles synced by WavySync"});
    api("POST", "/user/repos", token, Some(body)).map(|v| v["full_name"].as_str().unwrap_or_default().to_string())
}

#[derive(Deserialize, Clone, Debug)]
pub struct DeviceCode {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub interval: u64,
}

pub fn device_start(client_id: &str) -> Result<DeviceCode, String> {
    agent()
        .post("https://github.com/login/device/code")
        .header("Accept", "application/json")
        .send_form([("client_id", client_id), ("scope", "repo")])
        .map_err(|e| e.to_string())?
        .into_body()
        .read_json::<DeviceCode>()
        .map_err(|e| format!("GitHub sign-in: {e}"))
}

/// Polls until the user enters the code. Blocks; call it off the UI thread.
pub fn device_finish(client_id: &str, dc: &DeviceCode) -> Result<String, String> {
    let mut wait = dc.interval.max(5);
    loop {
        std::thread::sleep(Duration::from_secs(wait));
        let v: Value = agent()
            .post("https://github.com/login/oauth/access_token")
            .header("Accept", "application/json")
            .send_form([
                ("client_id", client_id),
                ("device_code", dc.device_code.as_str()),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ])
            .map_err(|e| e.to_string())?
            .into_body()
            .read_json()
            .map_err(|e| e.to_string())?;
        if let Some(t) = v["access_token"].as_str() {
            return Ok(t.into());
        }
        match v["error"].as_str() {
            Some("authorization_pending") => {}
            Some("slow_down") => wait += 5,
            Some(e) => return Err(format!("GitHub sign-in: {e}")),
            None => return Err("GitHub sign-in: no answer".into()),
        }
    }
}

/// The token of an already signed-in GitHub CLI, if there is one.
pub fn gh_token() -> Option<String> {
    let o = Command::new("gh").args(["auth", "token"]).output().ok()?;
    let t = String::from_utf8_lossy(&o.stdout).trim().to_string();
    (o.status.success() && !t.is_empty()).then_some(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_rfc4648() {
        assert_eq!(b64(b""), "");
        assert_eq!(b64(b"f"), "Zg==");
        assert_eq!(b64(b"fo"), "Zm8=");
        assert_eq!(b64(b"foo"), "Zm9v");
        assert_eq!(b64(b"foobar"), "Zm9vYmFy");
    }

    /// A live push carries only some items; the files of the others must stay as the last Push left them.
    #[test]
    fn kept_files_survive_a_partial_save() {
        let root = std::env::temp_dir().join(format!("wavysync-keep-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let bare = root.join("remote.git");
        fs::create_dir_all(&bare).unwrap();
        git(&bare, &["init", "--quiet", "--bare", "-b", "main"], None).unwrap();
        let r = Repo { dir: root.join("work"), url: bare.to_string_lossy().into(), token: None };
        let files = |pairs: &[(&str, &str)]| pairs.iter().map(|(k, v)| (k.to_string(), v.as_bytes().to_vec())).collect::<Files>();
        let meta = serde_json::json!({ "host": "test" });

        r.save("pc", files(&[("config/settings.json", "1"), ("workspaces.json", "layout")]), meta.clone(), |_| false).unwrap();
        let live = |rel: &str| crate::wave::folder_category(rel) == "workspaces";
        assert!(r.save("pc", files(&[("config/settings.json", "2")]), meta.clone(), live).unwrap().is_some());
        r.refresh().unwrap();
        assert_eq!(fs::read_to_string(r.dir.join("pc/workspaces.json")).unwrap(), "layout");
        assert_eq!(fs::read_to_string(r.dir.join("pc/config/settings.json")).unwrap(), "2");

        // A full push (nothing kept) still drops what is no longer picked.
        r.save("pc", files(&[("config/settings.json", "2")]), meta, |_| false).unwrap();
        r.refresh().unwrap();
        assert!(!r.dir.join("pc/workspaces.json").exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn short_names_become_github_urls() {
        assert_eq!(normalize("me/profiles"), "https://github.com/me/profiles");
        assert_eq!(normalize("https://github.com/me/p.git/"), "https://github.com/me/p");
    }
}
