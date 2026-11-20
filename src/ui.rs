//! The launcher: a borderless, transparent window shaped like a wide ~.
//! Upper-left lobe: the three setup steps (they stay as settings). Lower-right lobe: live sync.

use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

use rune::tok::TextStyle;
use rune::{
    Align, App, AppSpec, Color, Cursor, Cx, El, LineCap, Path, ScrollSpec, Shape, Side, TextEdit, Tx, Waker,
    WindowOptions, canvas, col, el, row, spacer, text,
};
use rune_kit::{Kit, PaletteSeed, TextInput};

use crate::app::{self, Config, ITEMS, Status};
use crate::ext::Ext;
use crate::repo::{self, DeviceCode, Profile, RepoInfo};
use crate::wave;

const W: f32 = 1200.0;
const H: f32 = 720.0;
const NEW: &str = "\u{0}new";

// 60 % plum ground, 30 % muted violet, 10 % Huawei red. Rune colours are linear light, so these are
// the sRGB hex values (#17111f, #43355a, #cf0a2c, ...) decoded.
const BG: Color = Color { r: 0.00857, g: 0.00561, b: 0.01370, a: 0.97 };
const PANEL: Color = Color { r: 0.01764, g: 0.01096, b: 0.03071, a: 0.82 };
const PANEL2: Color = Color { r: 0.02843, g: 0.01764, b: 0.05127, a: 1.0 };
const FIELD: Color = Color { r: 0.00700, g: 0.00478, b: 0.01161, a: 1.0 };
const EDGE: Color = Color { r: 0.05613, g: 0.03560, b: 0.10224, a: 1.0 };
const VIOLET: Color = Color { r: 0.14703, g: 0.09084, b: 0.27889, a: 1.0 };
const LILAC: Color = Color { r: 0.47353, g: 0.33245, b: 0.87137, a: 1.0 };
const RED: Color = Color { r: 0.62396, g: 0.00304, b: 0.02519, a: 1.0 };
const RED2: Color = Color { r: 1.00000, g: 0.03310, b: 0.09084, a: 1.0 };
const INK: Color = Color { r: 0.84687, g: 0.79130, b: 0.91310, a: 1.0 };
const SOFT: Color = Color { r: 0.62396, g: 0.55201, b: 0.76052, a: 1.0 };
const MUTED: Color = Color { r: 0.32314, g: 0.27468, b: 0.42327, a: 1.0 };
const DIM: Color = Color { r: 0.15896, g: 0.12477, b: 0.23840, a: 1.0 };
const OK: Color = Color { r: 0.07819, g: 0.63760, b: 0.25818, a: 1.0 };

pub fn is_tooling_arg(a: &str) -> bool {
    ["--snap", "--script", "--tree", "--list-scenes", "--scene", "--size", "--video", "--desktop", "--native", "--trace"]
        .contains(&a)
}

enum Job {
    Scan(Vec<Ext>, [usize; 3], bool),
    Signed(Result<(String, Vec<RepoInfo>), String>),
    Device(Result<DeviceCode, String>),
    Token(Result<String, String>),
    Profiles(Result<Vec<Profile>, String>),
    Synced(Result<Option<String>, String>),
    Applied(Result<Vec<String>, String>),
    Created(Result<String, String>),
}

pub struct S {
    cfg: Config,
    tr: bool,
    now: f32,
    step: u8,
    exts: Vec<Ext>,
    counts: [usize; 3],
    ext_open: bool,
    gh_cli: bool,
    login: String,
    repos: Vec<RepoInfo>,
    repo_open: bool,
    guest_url: TextEdit,
    new_repo: TextEdit,
    creating: bool,
    create_private: bool,
    device: Option<DeviceCode>,
    folder: TextEdit,
    profiles: Vec<Profile>,
    pick: String,
    pick_open: bool,
    modal: bool,
    save_to: String,
    save_open: bool,
    new_profile: TextEdit,
    status: Status,
    status_at: f32,
    busy: String,
    err: String,
    report: Vec<String>,
    jobs: (Sender<Job>, Receiver<Job>),
    waker: Option<Waker>,
    /// Screenshot scenes: background results from this computer never land.
    demo: bool,
    /// When a demo scene's pretend sync finishes (0: never).
    demo_done: f32,
    /// The red lines run while a sync or an apply is in progress.
    flow: Flow,
    flowing: bool,
    flow_min: f32,
}

/// Where the background dashes are along their curves. A sync speeds them up; when it ends they
/// glide to the next quarter cycle. Each curve carries `DASHES` evenly spaced dashes, so a quarter
/// cycle on looks exactly like where they started.
#[derive(Default)]
struct Flow {
    phase: f32,
    speed: f32,
    decel: Option<f32>,
    target: f32,
}

const FLOW_V: f32 = 0.3;
const DASHES: usize = 4;

impl Flow {
    fn env(&self) -> f32 {
        (self.speed / FLOW_V).clamp(0.0, 1.0)
    }

    fn step(&mut self, on: bool, dt: f32) {
        if on {
            self.decel = None;
            self.speed = (self.speed + FLOW_V / 0.8 * dt).min(FLOW_V);
        } else if self.speed > 0.0 {
            let decel = *self.decel.get_or_insert_with(|| {
                let step = 1.0 / DASHES as f32;
                self.target = ((self.phase + self.speed * 0.5) / step).ceil() * step;
                self.speed * self.speed / (2.0 * (self.target - self.phase))
            });
            self.speed = (self.speed - decel * dt).max(0.0);
        }
        self.phase += self.speed * dt;
        if self.decel.is_some() && (self.speed == 0.0 || self.phase >= self.target) {
            *self = Flow::default();
        }
    }
}

fn system_turkish() -> bool {
    let env = ["LC_ALL", "LC_MESSAGES", "LANG"].iter().filter_map(|k| std::env::var(k).ok()).find(|v| !v.is_empty());
    if let Some(v) = env {
        return v.starts_with("tr");
    }
    cfg!(target_os = "macos")
        && std::process::Command::new("defaults")
            .args(["read", "-g", "AppleLocale"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).starts_with("tr"))
            .unwrap_or(false)
}

fn init() -> S {
    let cfg = Config::load();
    S {
        tr: system_turkish(),
        now: 0.0,
        step: 1,
        exts: Vec::new(),
        counts: [0; 3],
        ext_open: true,
        gh_cli: false,
        login: cfg.login.clone(),
        repos: Vec::new(),
        repo_open: false,
        guest_url: TextEdit::new(if cfg.guest { cfg.repo.clone() } else { String::new() }),
        new_repo: TextEdit::new("wave-profiles"),
        creating: false,
        create_private: true,
        device: None,
        folder: TextEdit::new(cfg.folder.clone()),
        profiles: Vec::new(),
        pick: String::new(),
        pick_open: false,
        modal: false,
        save_to: String::new(),
        save_open: false,
        new_profile: TextEdit::new(""),
        status: Status::load(),
        status_at: 0.0,
        busy: String::new(),
        err: String::new(),
        report: Vec::new(),
        jobs: channel(),
        waker: None,
        demo: false,
        demo_done: 0.0,
        flow: Flow::default(),
        flowing: false,
        flow_min: 0.0,
        cfg,
    }
}

impl S {
    fn t(&self, en: &str, tr: &str) -> String {
        if self.tr { tr.into() } else { en.into() }
    }

    fn spawn(&mut self, busy: String, f: impl FnOnce() -> Job + Send + 'static) {
        if self.demo {
            return;
        }
        if !busy.is_empty() {
            self.busy = busy;
        }
        let tx = self.jobs.0.clone();
        let w = self.waker.clone();
        std::thread::spawn(move || {
            let _ = tx.send(f());
            if let Some(w) = w {
                w();
            }
        });
    }

    fn scan(&mut self) {
        self.spawn(String::new(), || {
            let wp = wave::paths();
            let (exts, _) = crate::ext::detect(&wave::read_widgets(&wp.config));
            let counts = workspace_counts(&wp.data.join("db/waveterm.db"));
            Job::Scan(exts, counts, repo::gh_token().is_some())
        });
    }

    fn load_profiles(&mut self) {
        if self.cfg.repo.is_empty() {
            return;
        }
        let r = self.cfg.repo();
        let busy = self.t("Reading profiles…", "Profiller okunuyor…");
        self.spawn(busy, move || Job::Profiles(r.refresh().map(|_| r.profiles())));
    }

    fn signed_in(&mut self, token: String) {
        if self.demo {
            return;
        }
        let _ = app::set_token(Some(&token));
        self.list_repos(token);
    }

    fn list_repos(&mut self, token: String) {
        let busy = self.t("Listing your repos…", "Depoların listeleniyor…");
        self.spawn(busy, move || Job::Signed(repo::user(&token).and_then(|u| repo::repos(&token).map(|r| (u, r)))));
    }

    fn start_flow(&mut self) {
        self.flowing = true;
        self.flow_min = self.now + 1.4;
    }

    fn sync_now(&mut self) {
        self.start_flow();
        let busy = self.t("Syncing…", "Senkronlanıyor…");
        if self.demo {
            self.busy = busy;
            self.demo_done = self.now + 2.6;
            return;
        }
        let cfg = self.cfg.clone();
        self.spawn(busy, move || Job::Synced(app::sync_once(&cfg, true)));
    }

    fn finish_setup(&mut self) {
        if self.cfg.repo.is_empty() {
            self.step = 2;
            self.err = self.t("Pick a repo first.", "Önce bir depo seç.");
            return;
        }
        self.cfg.setup_done = true;
        self.cfg.save();
        if self.demo {
            self.sync_now();
            return;
        }
        if let Err(e) = app::install_widget().and_then(|_| app::install_autostart()) {
            self.err = e;
        }
        app::spawn_detached(&["--daemon"]);
        self.sync_now();
    }

    fn open_apply(&mut self) {
        if self.pick.is_empty() {
            return;
        }
        self.save_to = self.cfg.folder.clone();
        self.new_profile = TextEdit::new("");
        self.modal = true;
        self.save_open = false;
    }

    fn confirm_apply(&mut self) {
        let save_to = if self.save_to == NEW { self.new_profile.text.trim().to_string() } else { self.save_to.clone() };
        if save_to.is_empty() {
            self.err = self.t("Name the new profile.", "Yeni profile bir ad ver.");
            return;
        }
        self.modal = false;
        self.cfg.save();
        self.start_flow();
        let pick = self.pick.clone();
        let busy = self.t("Applying: confirm Wave's quit question…", "Uygulanıyor: Wave'in çıkış sorusunu onayla…");
        if self.demo {
            self.busy = busy;
            self.demo_done = self.now + 3.2;
            return;
        }
        self.spawn(busy, move || {
            let r = app::apply_profile(&pick, &save_to);
            if r.is_ok() {
                let _ = app::sync_once(&Config::load(), true);
            }
            Job::Applied(r)
        });
    }

    fn land(&mut self, job: Job) {
        if self.demo {
            return;
        }
        self.busy.clear();
        let set_err = |r: Result<(), String>, s: &mut S| match r {
            Ok(()) => s.err.clear(),
            Err(e) => s.err = e,
        };
        match job {
            Job::Scan(exts, counts, gh) => {
                self.exts = exts;
                self.counts = counts;
                self.gh_cli = gh;
            }
            Job::Signed(r) => set_err(
                r.map(|(login, repos)| {
                    self.login = login.clone();
                    self.cfg.login = login;
                    self.cfg.guest = false;
                    self.repos = repos;
                    self.cfg.save();
                }),
                self,
            ),
            Job::Device(r) => match r {
                Ok(d) => {
                    let (id, code) = (app::client_id(), d.clone());
                    self.device = Some(d);
                    self.spawn(String::new(), move || Job::Token(repo::device_finish(&id, &code)));
                }
                Err(e) => self.err = e,
            },
            Job::Token(r) => {
                self.device = None;
                match r {
                    Ok(t) => self.signed_in(t),
                    Err(e) => self.err = e,
                }
            }
            Job::Profiles(r) => set_err(
                r.map(|p| {
                    if self.pick.is_empty() || !p.iter().any(|x| x.name == self.pick) {
                        self.pick = p.iter().find(|x| x.name != self.cfg.folder).or(p.first()).map(|x| x.name.clone()).unwrap_or_default();
                    }
                    self.profiles = p;
                }),
                self,
            ),
            Job::Synced(r) => {
                self.flowing = false;
                set_err(r.map(|_| ()), self);
                self.status = Status::load();
                self.load_profiles();
            }
            Job::Applied(r) => {
                self.flowing = false;
                self.cfg = Config::load();
                self.folder = TextEdit::new(self.cfg.folder.clone());
                self.status = Status::load();
                set_err(r.map(|lines| self.report = lines), self);
            }
            Job::Created(r) => set_err(
                r.map(|name| {
                    self.creating = false;
                    self.cfg.repo = name;
                    self.cfg.guest = false;
                    self.cfg.save();
                    self.load_profiles();
                }),
                self,
            ),
        }
    }
}

fn workspace_counts(db: &std::path::Path) -> [usize; 3] {
    let Ok(c) = rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY) else { return [0; 3] };
    let n = |t: &str| c.query_row(&format!("SELECT count(*) FROM {t}"), [], |r| r.get::<_, i64>(0)).unwrap_or(0) as usize;
    [n("db_workspace"), n("db_tab"), n("db_block")]
}

fn setup(s: &mut S, waker: Waker) {
    s.waker = Some(waker);
    s.scan();
    if !s.cfg.guest {
        // The keychain can ask the user first, so read it off the main thread.
        s.spawn(String::new(), || match app::token() {
            Some(t) => Job::Signed(repo::user(&t).and_then(|u| repo::repos(&t).map(|r| (u, r)))),
            None => Job::Signed(Err(String::new())),
        });
    }
    s.load_profiles();
}

fn poll(s: &mut S, now: f64, tx: &mut Tx) -> bool {
    let now = now as f32;
    let dt = (now - s.now).clamp(0.0, 0.1);
    s.now = now;
    while let Ok(job) = s.jobs.1.try_recv() {
        s.land(job);
    }
    if s.demo && s.flowing && s.demo_done > 0.0 && now > s.demo_done {
        s.flowing = false;
        s.busy.clear();
    }
    s.flow.step(s.flowing || now < s.flow_min, dt);
    if !s.demo && s.now - s.status_at > 2.0 {
        s.status_at = s.now;
        s.status = Status::load();
    }
    tx.wake_after(Duration::ZERO);
    true
}

// Drawing ----------------------------------------------------------------------------------------

fn style(size: f32, weight: f32) -> TextStyle {
    TextStyle::new(size, weight)
}
fn label(s: impl Into<String>, size: f32, weight: f32, c: Color) -> El {
    text(s.into()).font(style(size, weight)).color(c)
}
fn mono(s: impl Into<String>, size: f32, c: Color) -> El {
    text(s.into()).font(style(size, 500.0).mono()).color(c)
}
fn caps(s: impl Into<String>, c: Color) -> El {
    text(s.into()).font(style(11.5, 700.0).mono().tracking(1.4)).color(c)
}

/// The body: a wide, soft tilde. Start point, then (control 1, control 2, end) per cubic.
const BODY_START: [f32; 2] = [58.0, 140.0];
const BODY: [[[f32; 2]; 3]; 14] = [
    [[58.0, 60.0], [80.0, 32.0], [140.0, 30.0]],
    [[290.0, 24.0], [440.0, 14.0], [530.0, 38.0]],
    [[610.0, 60.0], [636.0, 118.0], [666.0, 168.0]],
    [[696.0, 214.0], [740.0, 226.0], [810.0, 220.0]],
    [[920.0, 210.0], [1020.0, 204.0], [1096.0, 218.0]],
    [[1136.0, 226.0], [1160.0, 256.0], [1160.0, 300.0]],
    [[1164.0, 400.0], [1164.0, 520.0], [1158.0, 612.0]],
    [[1154.0, 666.0], [1120.0, 698.0], [1056.0, 700.0]],
    [[950.0, 706.0], [850.0, 712.0], [752.0, 694.0]],
    [[676.0, 680.0], [636.0, 640.0], [608.0, 592.0]],
    [[580.0, 546.0], [548.0, 530.0], [476.0, 532.0]],
    [[370.0, 536.0], [250.0, 548.0], [162.0, 538.0]],
    [[98.0, 532.0], [60.0, 504.0], [56.0, 446.0]],
    [[50.0, 360.0], [50.0, 230.0], [58.0, 140.0]],
];

fn body_path() -> Path {
    BODY.iter().fold(Path::new().move_to(BODY_START), |p, [a, b, c]| p.cubic_to(*a, *b, *c)).close()
}

fn bezier(p0: [f32; 2], [a, b, c]: [[f32; 2]; 3], t: f32) -> [f32; 2] {
    let u = 1.0 - t;
    let k = [u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t];
    [
        k[0] * p0[0] + k[1] * a[0] + k[2] * b[0] + k[3] * c[0],
        k[0] * p0[1] + k[1] * a[1] + k[2] * b[1] + k[3] * c[1],
    ]
}

/// Distance from `p` to the outline, positive inside the body.
fn depth(poly: &[[f32; 2]], p: [f32; 2]) -> f32 {
    let (mut inside, mut d) = (false, f32::MAX);
    for (i, a) in poly.iter().enumerate() {
        let b = poly[(i + 1) % poly.len()];
        if (a[1] > p[1]) != (b[1] > p[1]) && p[0] < a[0] + (p[1] - a[1]) / (b[1] - a[1]) * (b[0] - a[0]) {
            inside = !inside;
        }
        let (vx, vy) = (b[0] - a[0], b[1] - a[1]);
        let t = (((p[0] - a[0]) * vx + (p[1] - a[1]) * vy) / (vx * vx + vy * vy).max(1e-6)).clamp(0.0, 1.0);
        d = d.min((a[0] + vx * t - p[0]).hypot(a[1] + vy * t - p[1]));
    }
    if inside { d } else { -d }
}

const LINES: usize = 15;
/// Text that sits straight on the body (x0, y0, x1, y1): dashes dim over it.
const TEXT_ZONES: [[f32; 4]; 4] = [
    [90.0, 36.0, 440.0, 104.0],
    [84.0, 470.0, 380.0, 504.0],
    [750.0, 236.0, 1130.0, 284.0],
    [680.0, 598.0, 920.0, 634.0],
];
const SAMPLES: usize = 180;

/// A curve that follows the tilde from one lobe to the other, sampled, with how deep inside the
/// body each sample is. Rune has no path clipping, so the lines are cut and faded by this.
struct FlowLine {
    pts: Vec<[f32; 2]>,
    depth: Vec<f32>,
    inside: Vec<std::ops::Range<usize>>,
    /// Where the first of the evenly spaced dashes rests (0..1 along the curve), and their length.
    offset: f32,
    len: f32,
    /// Whole number, so a quarter-cycle step moves every dash onto a neighbour's place.
    speed: f32,
}

fn hash(n: u32) -> f32 {
    let mut x = n.wrapping_mul(0x9E37_79B9) ^ 0x85EB_CA6B;
    x ^= x >> 15;
    x = x.wrapping_mul(0x2C1B_3C6D);
    x ^= x >> 12;
    (x & 0xFFFF) as f32 / 65535.0
}

fn flow_lines() -> &'static [FlowLine] {
    static LINES_: std::sync::OnceLock<Vec<FlowLine>> = std::sync::OnceLock::new();
    LINES_.get_or_init(|| {
        let mut poly = Vec::new();
        let mut p0 = BODY_START;
        for seg in BODY {
            poly.extend((1..=16).map(|i| bezier(p0, seg, i as f32 / 16.0)));
            p0 = seg[2];
        }
        (0..LINES)
            .map(|i| {
                let o = -1.0 + 2.0 * i as f32 / (LINES - 1) as f32;
                let (y0, y1) = (282.0 + o * 236.0, 458.0 + o * 230.0);
                let wobble = 5.0 + 6.0 * hash(i as u32 * 7 + 1);
                let shift = hash(i as u32 * 13 + 5) * std::f32::consts::TAU;
                let pts: Vec<[f32; 2]> = (0..SAMPLES)
                    .map(|j| {
                        let x = -20.0 + 1240.0 * j as f32 / (SAMPLES - 1) as f32;
                        let t = ((x - 520.0) / 300.0).clamp(0.0, 1.0);
                        let y = y0 + (y1 - y0) * t * t * (3.0 - 2.0 * t) + wobble * (x / 170.0 + shift).sin();
                        [x, y]
                    })
                    .collect();
                let depth: Vec<f32> = pts.iter().map(|p| depth(&poly, *p)).collect();
                let mut inside = Vec::new();
                let mut from = None;
                for (j, d) in depth.iter().chain([&-1.0]).enumerate() {
                    match (from, *d > 6.0) {
                        (None, true) => from = Some(j),
                        (Some(a), false) => {
                            if j - a > 1 {
                                inside.push(a..j);
                            }
                            from = None;
                        }
                        _ => {}
                    }
                }
                let (offset, len) = (hash(i as u32 * 61 + 9), 0.028 + 0.022 * hash(i as u32 * 17 + 3));
                FlowLine { pts, depth, inside, offset, len, speed: if i % 3 == 0 { 2.0 } else { 1.0 } }
            })
            .collect()
    })
}

fn sample_range(u: f32, len: f32) -> (usize, usize) {
    let last = (SAMPLES - 1) as f32;
    ((u * last) as usize, (((u + len) * last) as usize).min(SAMPLES - 1))
}

/// The long curves outside the body, for the window's silhouette.
fn swift_paths() -> [Path; 3] {
    [
        Path::new().move_to([-20.0, 610.0]).cubic_to([260.0, 610.0], [340.0, 430.0], [600.0, 400.0]).cubic_to([860.0, 370.0], [960.0, 200.0], [1220.0, 190.0]),
        Path::new().move_to([10.0, 150.0]).cubic_to([90.0, 40.0], [260.0, 8.0], [430.0, 14.0]),
        Path::new().move_to([760.0, 712.0]).cubic_to([900.0, 728.0], [1060.0, 716.0], [1190.0, 640.0]),
    ]
}

fn full(shape: Shape) -> El {
    canvas(shape.view_box(W, H)).size(W, H).absolute(0.0, 0.0)
}

/// A dash with a fading tail, `head` running 0 → 1 along the path.
fn dash(key: &str, p: &Path, head: f32, len: f32, width: f32, c: Color) -> Vec<El> {
    (0..4)
        .map(|i| {
            let hi = head - len * i as f32 / 4.0;
            let lo = hi - len / 4.0;
            let a = c.a * (1.0 - i as f32 * 0.24);
            full(Shape::new().stroke_path(p, width, LineCap::Round, Color { a, ..c }).trimmed(lo.max(0.0), hi.clamp(0.0, 1.0))).key(format!("{key}-{i}"))
        })
        .collect()
}

fn backdrop(s: &S) -> Vec<El> {
    let env = s.flow.env();
    let paths = swift_paths();
    let mut out = Vec::new();
    let mut quiet = Shape::new();
    for p in &paths {
        quiet = quiet.stroke_path(p, 1.0, LineCap::Round, Color { a: 0.3, ..RED });
    }
    // Every layer is keyed: the flow adds and removes layers, and unkeyed siblings would trade
    // places (and blur) with each other.
    out.push(full(quiet).key("quiet"));
    {
        for (i, p) in paths.iter().enumerate() {
            let head = (s.flow.phase * 1.6 + 0.35 * i as f32).rem_euclid(1.3) - 0.15;
            out.extend(dash(&format!("swift-{i}"), p, head, 0.24, 2.5, Color { a: env, ..if i == 1 { LILAC } else { RED2 } }));
        }
    }
    let body = body_path();
    out.push(full(Shape::new().fill_path(&body, BG)).window_drag().key("body"));

    // Texture: every flow line, faint, where it is inside; then the neon dashes on top.
    let (mut faint, mut glow, mut core) = (Shape::new(), Shape::new(), Shape::new());
    for (i, l) in flow_lines().iter().enumerate() {
        let tint = if i % 4 == 1 { LILAC } else { RED2 };
        for r in &l.inside {
            faint = faint.stroke(l.pts[r.clone()].to_vec(), 1.0, Color { a: 0.06 + 0.06 * env, ..tint });
        }
        for j in 0..DASHES {
            let u = (l.offset + j as f32 / DASHES as f32 + l.speed * s.flow.phase).rem_euclid(1.0);
            let (a, b) = sample_range(u, l.len);
            if b <= a + 1 {
                continue;
            }
            let pts = l.pts[a..=b].to_vec();
            let on_text = pts.iter().any(|p| TEXT_ZONES.iter().any(|z| p[0] > z[0] && p[0] < z[2] && p[1] > z[1] && p[1] < z[3]));
            let edge = ((l.depth[a..=b].iter().copied().fold(f32::MAX, f32::min) - 2.0) / 28.0).clamp(0.0, 1.0);
            let fade = edge * if on_text { 0.15 } else { 1.0 };
            if fade == 0.0 {
                continue;
            }
            glow = glow.stroke(pts.clone(), 7.0, Color { a: (0.2 + 0.45 * env) * fade, ..tint });
            core = core.stroke(pts, 1.6, Color { a: (0.42 + 0.5 * env) * fade, ..tint });
        }
    }
    out.push(full(faint).key("faint"));
    out.push(full(glow).blur(6.0).key("glow"));
    out.push(full(core).blur(0.6).key("core"));

    out.push(full(Shape::new().stroke_path(&body, 2.0, LineCap::Round, EDGE)).key("edge"));
    // The outline lights up while the lines run. Always in the tree, faded by `env`.
    out.push(full(Shape::new().stroke_path(&body, 3.0, LineCap::Round, Color { a: 0.24 * env, ..RED2 })).blur(9.0).key("outline-glow"));
    out.push(full(Shape::new().stroke_path(&body, 1.0, LineCap::Round, Color { a: 0.55 * env, ..RED2 })).key("outline-line"));
    out.push(full(Shape::new().line([70.0, 160.0], [70.0, 430.0], 4.0, LineCap::Round, RED)).key("accent-l"));
    out.push(full(Shape::new().line([1144.0, 320.0], [1144.0, 600.0], 4.0, LineCap::Round, VIOLET)).key("accent-r"));
    out
}

fn card(h: f32) -> El {
    col().w_full().h(h).fill(PANEL).border(1.0, EDGE).radius(14.0).px(14.0).py(12.0).gap(6.0)
}

fn check(on: bool) -> El {
    let b = el().size(15.0, 15.0).radius(4.0).center();
    if on { b.fill(RED).child(label("✓", 11.0, 800.0, INK)) } else { b.border(1.5, VIOLET) }
}

fn chip(s: impl Into<String>) -> El {
    el().fill(PANEL2).radius(6.0).px(6.0).py(1.0).child(mono(s, 10.5, Color { r: 0.58408, g: 0.47932, b: 0.79130, a: 1.0 }))
}

fn button(s: impl Into<String>, primary: bool) -> El {
    el().px(12.0)
        .py(7.0)
        .radius(9.0)
        .fill(if primary { RED } else { PANEL2 })
        .border(1.0, if primary { RED } else { EDGE })
        .hover(if primary { RED2 } else { EDGE })
        .cursor(Cursor::Pointer)
        .child(label(s, 12.5, 600.0, INK))
}

fn trigger(key: &str, s: impl Into<String>) -> El {
    row().key(key).w_full().h(32.0).px(10.0).center_y().fill(FIELD).border(1.0, EDGE).radius(8.0).cursor(Cursor::Pointer)
        .child(mono(s, 13.0, INK))
        .child(spacer())
        .child(label("▾", 12.0, 600.0, MUTED))
}

fn menu(key: &str, items: Vec<(String, bool, El)>) -> El {
    let mut m = col().fill(Color { r: 0.01096, g: 0.00700, b: 0.01938, a: 1.0 }).border(1.0, EDGE).radius(10.0).padding(4.0).w(420.0).shadow(18.0, 40.0, Color { r: 0.0, g: 0.0, b: 0.0, a: 0.5 });
    for (_, add, e) in items {
        let _ = add;
        m = m.child(e);
    }
    m.anchored(key, Side::Bottom, Align::Start, 4.0)
}

fn menu_row(s: impl Into<String>, add: bool) -> El {
    row().w_full().px(9.0).py(6.0).radius(7.0).hover(PANEL2).cursor(Cursor::Pointer)
        .child(label(s, 12.5, if add { 700.0 } else { 500.0 }, if add { RED2 } else { INK }))
}

fn kit() -> Kit {
    Kit::from_palette(PaletteSeed { bg: FIELD, fg: INK, accent: RED })
}

fn item_names(s: &S, key: &str) -> (String, String) {
    let [ws, tabs, blocks] = s.counts;
    match key {
        "settings" => (s.t("Settings", "Ayarlar"), "settings.json".into()),
        "workspaces" => (s.t("Workspaces, tabs, layouts", "Çalışma alanları, sekmeler, düzenler"), format!("{ws} · {tabs} · {blocks}")),
        "widgets" => (s.t("Widgets and extensions", "Widget'lar ve eklentiler"), s.exts.len().to_string()),
        "themes" => (s.t("Themes, presets, backgrounds", "Temalar, hazır ayarlar, arka planlar"), String::new()),
        "connections" => (s.t("Connections (SSH)", "Bağlantılar (SSH)"), s.t("no secrets", "gizli bilgiler hariç")),
        _ => (s.t("AI modes", "AI modları"), s.t("no API keys", "API anahtarları hariç")),
    }
}

fn step_sync(s: &S) -> El {
    let mut c = col().w_full().gap(2.0);
    for (key, _) in ITEMS {
        let on = s.cfg.items.get(key).copied().unwrap_or(true);
        let (name, sub) = item_names(s, key);
        let k = key.to_string();
        let mut r = row().w_full().h(26.0).gap(8.0).center_y()
            .child(check(on).on_press(move |s: &mut S| {
                let v = s.cfg.items.entry(k.clone()).or_insert(true);
                *v = !*v;
                s.cfg.save();
            }).cursor(Cursor::Pointer));
        if key == "widgets" {
            r = r.child(label(if s.ext_open { "▾" } else { "▸" }, 11.0, 700.0, MUTED).key("ext-toggle").cursor(Cursor::Pointer)
                .on_press(|s: &mut S| s.ext_open = !s.ext_open));
        }
        c = c.child(r.child(label(name, 13.0, 500.0, INK)).child(spacer()).child(label(sub, 11.5, 500.0, MUTED)));
        if key == "widgets" && s.ext_open {
            let mut list = col().w_full().pl(23.0).gap(1.0);
            for e in &s.exts {
                let on = s.cfg.widgets.get(&e.id).copied().unwrap_or(true);
                let id = e.id.clone();
                let deps = match e.deps.len() {
                    0 => s.t("no dependencies", "bağımlılık yok"),
                    1 => e.deps[0].summary(),
                    n => format!("{} +{}", e.deps[0].summary(), n - 1),
                };
                list = list.child(row().w_full().h(24.0).gap(8.0).center_y()
                    .child(check(on).cursor(Cursor::Pointer).on_press(move |s: &mut S| {
                        let v = s.cfg.widgets.entry(id.clone()).or_insert(true);
                        *v = !*v;
                        s.cfg.save();
                    }))
                    .child(label(e.label.clone(), 12.5, 500.0, INK))
                    .child(spacer())
                    .child(chip(deps)));
            }
            if s.exts.is_empty() {
                list = list.child(label(s.t("No widgets of your own yet.", "Henüz kendi widget'ın yok."), 12.0, 500.0, MUTED));
            }
            c = c.child(list);
        }
    }
    c.scroll(ScrollSpec { stick_bottom: false }).scroll_edge_fade(12.0).h(246.0)
}

fn step_repo(s: &S, k: &Kit) -> El {
    let mut c = col().w_full().gap(8.0);
    if s.login.is_empty() || s.cfg.guest {
        let mut r = row().gap(8.0).center_y();
        if !app::client_id().is_empty() {
            r = r.child(button(s.t("Sign in with GitHub", "GitHub ile giriş yap"), true).on_press(|s: &mut S| {
                let id = app::client_id();
                let busy = s.t("Asking GitHub for a code…", "GitHub'dan kod isteniyor…");
                s.spawn(busy, move || Job::Device(repo::device_start(&id)));
            }));
        }
        if s.gh_cli {
            r = r.child(button(s.t("Use gh account", "gh hesabını kullan"), app::client_id().is_empty()).on_press(|s: &mut S| {
                match repo::gh_token() {
                    Some(t) => s.signed_in(t),
                    None => s.err = "gh auth token failed".into(),
                }
            }));
        }
        c = c.child(r.child(spacer()).child(label(s.t("for private repos", "özel depolar için"), 11.5, 500.0, MUTED)));
        if let Some(d) = &s.device {
            let uri = d.verification_uri.clone();
            c = c.child(row().gap(10.0).center_y()
                .child(mono(d.user_code.clone(), 22.0, INK))
                .child(button(s.t("Open GitHub", "GitHub'ı aç"), false).on_press_tx(move |_: &mut S, tx: &mut Tx| { let _ = tx.open_url(&uri); }))
                .child(label(s.t("waiting for you…", "bekleniyor…"), 11.5, 500.0, MUTED)));
        }
    } else {
        let current = if s.cfg.repo.is_empty() { s.t("Pick a repo", "Bir depo seç") } else { s.cfg.repo.clone() };
        c = c.child(row().gap(8.0).center_y()
            .child(el().size(8.0, 8.0).radius(4.0).fill(OK))
            .child(label(s.login.clone(), 13.0, 600.0, INK))
            .child(spacer())
            .child(label(s.t("Sign out", "Çıkış yap"), 11.5, 600.0, MUTED).cursor(Cursor::Pointer).on_press(|s: &mut S| {
                if !s.demo {
                    let _ = app::set_token(None);
                }
                s.login.clear();
                s.cfg.login.clear();
                s.repos.clear();
                s.cfg.save();
            })));
        c = c.child(trigger("repo-dd", current).on_press(|s: &mut S| s.repo_open = !s.repo_open));
        if s.creating {
            c = c.child(row().gap(8.0).center_y()
                .child(TextInput::new(&s.new_repo).width(Some(200.0)).on_edit(|s: &mut S| &mut s.new_repo).build(k))
                .child(check(s.create_private).cursor(Cursor::Pointer).on_press(|s: &mut S| s.create_private = !s.create_private))
                .child(label(s.t("private", "özel"), 12.0, 500.0, SOFT))
                .child(button(s.t("Create", "Oluştur"), true).on_press(|s: &mut S| {
                    let (name, private) = (s.new_repo.text.trim().to_string(), s.create_private);
                    let Some(t) = app::token() else { return };
                    let busy = s.t("Creating the repo…", "Depo oluşturuluyor…");
                    s.spawn(busy, move || Job::Created(repo::create_repo(&t, &name, private)));
                })));
        }
    }
    c = c.child(row().mt(6.0).gap(6.0).center_y()
        .child(caps(s.t("GUEST", "MİSAFİR"), RED2))
        .child(label(s.t("public repo, this computer's git login", "public depo, bu bilgisayarın git kimliği"), 11.5, 500.0, MUTED)));
    c.child(row().gap(8.0).center_y()
        .child(TextInput::new(&s.guest_url).placeholder("https://github.com/someone/wave-profiles").width(Some(320.0))
            .on_edit(|s: &mut S| &mut s.guest_url).build(k))
        .child(button(s.t("Use", "Kullan"), false).on_press(|s: &mut S| {
            let url = s.guest_url.text.trim().to_string();
            if url.is_empty() {
                return;
            }
            s.cfg.repo = url;
            s.cfg.guest = true;
            s.cfg.save();
            s.load_profiles();
        })))
}

fn step_profile(s: &S, k: &Kit) -> El {
    let mut c = col().w_full().gap(6.0)
        .child(label(s.t("This computer's folder (you can rename it)", "Bu bilgisayarın klasörü (değiştirilebilir)"), 12.0, 500.0, MUTED))
        .child(TextInput::new(&s.folder).width(Some(420.0)).on_edit(|s: &mut S| &mut s.folder).on_change(|s: &mut S, _| {
            let name: String = s.folder.text.trim().to_string();
            if s.cfg.target == s.cfg.folder {
                s.cfg.target = name.clone();
            }
            s.cfg.folder = name;
            s.cfg.save();
        }).build(k))
        .child(label(s.t("Profile to apply", "Uygulanacak profil"), 12.0, 500.0, MUTED).mt(6.0));
    if s.profiles.is_empty() {
        let note = if s.cfg.repo.is_empty() {
            s.t("Pick a repo in step 02.", "02. adımda bir depo seç.")
        } else {
            s.t("No profiles here yet. The first sync creates yours.", "Burada henüz profil yok. İlk senkron seninkini oluşturur.")
        };
        return c.child(label(note, 12.5, 500.0, SOFT));
    }
    let shown = if s.pick == s.cfg.folder { format!("{} {}", s.pick, s.t("(this computer)", "(bu bilgisayar)")) } else { s.pick.clone() };
    c = c.child(trigger("pick-dd", shown).on_press(|s: &mut S| s.pick_open = !s.pick_open));
    c.child(row().mt(8.0).gap(10.0).center_y()
        .child(button(s.t("Apply", "Uygula"), true).key("apply").on_press(|s: &mut S| s.open_apply()))
        .child(label(s.t("Wave restarts; a backup is kept", "Wave yeniden başlar; yedek alınır"), 11.5, 500.0, MUTED)))
}

fn left_lobe(s: &S, k: &Kit) -> El {
    let names = [("01", "SYNC"), ("02", "REPO"), ("03", "PROFILE")];
    let mut steps = row().w_full().gap(4.0);
    for (i, (n, name)) in names.iter().enumerate() {
        let on = s.step == i as u8 + 1;
        steps = steps.child(el().key(format!("step-{}", i + 1)).grow(1.0).h(28.0).center().radius(4.0)
            .fill(if on { RED } else { PANEL }).border(1.0, if on { RED } else { EDGE }).cursor(Cursor::Pointer)
            .child(caps(format!("{n} {name}"), if on { INK } else { MUTED }))
            .on_press(move |s: &mut S| s.step = i as u8 + 1));
    }
    let (title, content) = match s.step {
        1 => (s.t("WHAT TO SYNC", "NE SENKRONLANSIN"), step_sync(s)),
        2 => (s.t("REPO", "DEPO"), step_repo(s, k)),
        _ => (s.t("PROFILE", "PROFİL"), step_profile(s, k)),
    };
    let body = card(300.0).child(row().gap(6.0).child(caps(format!("0{} {title}", s.step), RED2))).child(content);
    col().absolute(92.0, 118.0).w(450.0).gap(10.0).child(steps).child(body)
}

fn ago(s: &S, secs: u64) -> String {
    let d = repo::now().saturating_sub(secs);
    if secs == 0 {
        "-".into()
    } else if d < 60 {
        format!("{d} s")
    } else if d < 3600 {
        format!("{} {}", d / 60, s.t("min", "dk"))
    } else {
        format!("{} {}", d / 3600, s.t("h", "sa"))
    }
}

fn clock(secs: u64) -> String {
    let local = secs as i64 + utc_offset();
    format!("{:02}:{:02}", (local / 3600).rem_euclid(24), (local / 60).rem_euclid(60))
}

fn utc_offset() -> i64 {
    static OFF: std::sync::OnceLock<i64> = std::sync::OnceLock::new();
    *OFF.get_or_init(|| {
        let o = std::process::Command::new("date").arg("+%z").output().ok();
        let z = o.map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
        let (sign, digits) = if let Some(d) = z.strip_prefix('-') { (-1, d) } else { (1, z.trim_start_matches('+')) };
        let h: i64 = digits.get(..2).and_then(|v| v.parse().ok()).unwrap_or(0);
        let m: i64 = digits.get(2..4).and_then(|v| v.parse().ok()).unwrap_or(0);
        sign * (h * 3600 + m * 60)
    })
}

fn right_lobe(s: &S) -> El {
    let mut c = card(300.0);
    if !s.cfg.setup_done {
        let tips = [
            s.t("Tick what should travel. Each widget is its own box; WavySync also installs what it needs on the other computer.",
                "Neyin taşınacağını işaretle. Her widget ayrı bir kutu; WavySync gerektirdiklerini diğer bilgisayara da kurar."),
            s.t("Sign in to see your private repos, or paste a public repo as a guest.",
                "Özel depolarını görmek için giriş yap ya da misafir olarak public bir depo yapıştır."),
            s.t("This computer gets its own folder. Apply another computer's profile here if you like, then finish.",
                "Bu bilgisayara ait bir klasör açılır. İstersen başka bir bilgisayarın profilini buraya uygula, sonra bitir."),
        ];
        c = c.child(caps(s.t("SETUP", "KURULUM"), RED2))
            .child(label(tips[(s.step - 1) as usize].clone(), 13.5, 500.0, SOFT).wrap().w_full())
            .child(spacer());
        let mut dots = row().gap(6.0);
        for i in 1..=3u8 {
            dots = dots.child(el().size(if i == s.step { 22.0 } else { 8.0 }, 8.0).radius(4.0).fill(if i <= s.step { RED } else { EDGE }));
        }
        let next = if s.step < 3 { s.t("Next", "İleri") } else { s.t("Finish setup", "Kurulumu bitir") };
        return c.child(row().w_full().center_y().child(dots).child(spacer()).child(button(next, true).on_press(|s: &mut S| {
            if s.step < 3 { s.step += 1 } else { s.finish_setup() }
        })));
    }
    let repo_name = s.cfg.repo.trim_start_matches("https://github.com/").to_string();
    c = c.child(row().gap(8.0).center_y()
        .child(el().size(8.0, 8.0).radius(4.0).fill(if s.status.error.is_empty() { OK } else { RED2 }))
        .child(caps(s.t("LIVE SYNC", "CANLI SENKRON"), RED2))
        .child(mono(repo_name, 11.5, MUTED)));
    c = c.child(row().w_full().center_y().child(label(s.t("Saving to", "Kaydedilen klasör"), 13.0, 500.0, INK)).child(spacer())
        .child(mono(s.cfg.target.clone(), 12.0, INK)));
    let auto = s.cfg.auto;
    c = c.child(row().w_full().center_y()
        .child(label(s.t("Auto sync", "Otomatik senkron"), 13.0, 500.0, INK))
        .child(spacer())
        .child(el().size(34.0, 19.0).radius(10.0).fill(if auto { RED } else { EDGE }).cursor(Cursor::Pointer)
            .child(el().size(15.0, 15.0).radius(8.0).fill(INK).absolute(if auto { 17.0 } else { 2.0 }, 2.0))
            .on_press(|s: &mut S| {
                s.cfg.auto = !s.cfg.auto;
                s.cfg.save();
            })));
    let mut feed = col().gap(3.0).mt(4.0);
    let lines: Vec<(String, Color)> = if !s.report.is_empty() {
        s.report.iter().rev().take(6).map(|l| (l.clone(), if l.contains("FAILED") { RED2 } else { SOFT })).collect()
    } else {
        s.status.feed.iter().take(6).map(|(t, m)| (format!("{}  {m}", clock(*t)), SOFT)).collect()
    };
    for (l, col_) in lines {
        feed = feed.child(mono(l, 11.5, col_));
    }
    c = c.child(feed).child(spacer());
    if !s.status.error.is_empty() {
        c = c.child(label(s.status.error.clone(), 11.5, 600.0, RED2).wrap().w_full());
    }
    c
}

fn hx(hex: &str) -> Color {
    rune::color::hex(hex).unwrap_or(RED)
}

fn orb(s: &S) -> Vec<El> {
    let (cx, cy) = (607.0, 372.0);
    let env = s.flow.env();
    let (big, small) = if !s.busy.is_empty() {
        ("···".to_string(), String::new())
    } else if s.cfg.setup_done {
        ("SYNC".to_string(), ago(s, s.status.last_push))
    } else {
        ("START".to_string(), format!("{}/3", s.step))
    };
    let white = |a: f32| Color { a, ..INK };
    // A soft light band crosses the button every six seconds.
    let p = (s.now % 6.0) / 1.6 * 1.3 - 0.15;
    let sheen_a = if (0.0..=1.0).contains(&p) { 0.26 * (std::f32::consts::PI * p).sin() } else { 0.0 };
    let sheen = el().absolute(0.0, 0.0).size(128.0, 128.0).radius(64.0).linear_gradient(
        2.3,
        [(0.0, white(0.0)), ((p - 0.16).clamp(0.0, 1.0), white(0.0)), (p.clamp(0.0, 1.0), white(sheen_a)), ((p + 0.16).clamp(0.0, 1.0), white(0.0)), (1.0, white(0.0))],
    );
    let specular = el().absolute(0.0, 0.0).size(128.0, 128.0).radius(64.0)
        .radial_gradient([0.34, 0.24], 60.0, [(0.0, white(0.4)), (0.5, white(0.08)), (1.0, white(0.0))]);
    vec![
        el().key("bloom").absolute(cx - 92.0, cy - 92.0).size(184.0, 184.0).radius(92.0).fill(Color { a: 0.3 + 0.35 * env, ..RED }).blur(30.0),
        col().key("orb").absolute(cx - 64.0, cy - 64.0).size(128.0, 128.0).radius(64.0).center()
            .fill(RED)
            .linear_gradient(std::f32::consts::PI, [
                (0.0, hx("#ff5d78")),
                (0.4, hx("#d90f34")),
                (0.5, hx("#b30a28")),
                (0.8, hx("#88061d")),
                (1.0, hx("#5a0313")),
            ])
            .border_gradient(1.5, hx("#ffb7c4"), hx("#4a0210"))
            .shadow(12.0, 36.0, Color { a: 0.5, ..RED })
            .cursor(Cursor::Pointer)
            .child(specular)
            .child(sheen)
            .child(label(big, 17.0, 800.0, INK))
            .child(mono(small, 9.5, INK))
            .on_press(|s: &mut S| {
                if !s.busy.is_empty() {
                    return;
                }
                if s.cfg.setup_done {
                    s.sync_now();
                } else if s.step < 3 {
                    s.step += 1;
                } else {
                    s.finish_setup();
                }
            }),
    ]
}

fn modal(s: &S) -> El {
    let this_pc = s.cfg.folder.clone();
    let label_for = |v: &str| -> String {
        if v == NEW {
            s.t("+ New profile", "+ Yeni Profil")
        } else if v == s.pick && v != this_pc {
            format!("{} {}", s.t("Update the picked profile:", "Seçilen profili güncelle:"), v)
        } else if v == this_pc {
            format!("{v} {}", s.t("(this computer)", "(bu bilgisayar)"))
        } else {
            v.to_string()
        }
    };
    let overwrite = s.save_to != NEW && s.save_to != s.pick;
    let k = kit();
    let mut card = col().w(560.0).fill(Color { r: 0.01229, g: 0.00750, b: 0.02217, a: 1.0 }).border(1.0, EDGE).radius(16.0).padding(22.0).gap(10.0)
        .shadow(30.0, 80.0, Color { r: 0.0, g: 0.0, b: 0.0, a: 0.6 })
        .child(label(s.t("This Wave will be set to the profile you picked", "Bu Wave seçtiğiniz profile ayarlanacaktır"), 17.0, 700.0, INK))
        .child(label(format!("{}: {}", s.t("Profile", "Profil"), s.pick), 12.5, 500.0, MUTED))
        .child(label(s.t("Where should your future changes be saved?", "İleride yapacağınız değişiklikler nereye kaydedilsin?"), 13.5, 500.0, SOFT))
        .child(trigger("save-dd", label_for(&s.save_to)).on_press(|s: &mut S| s.save_open = !s.save_open));
    if s.save_to == NEW {
        card = card.child(TextInput::new(&s.new_profile).placeholder(s.t("new profile name", "yeni profil adı")).width(Some(516.0))
            .on_edit(|s: &mut S| &mut s.new_profile).build(&k));
    }
    card = card.child(el().w_full().padding(10.0).radius(10.0).border(1.0, RED).fill(Color { a: 0.12, ..RED }).opacity(if overwrite { 1.0 } else { 0.0 })
        .child(label(s.t("Careful: if you pick this option your previous profile is overwritten and you can't go back!",
            "Dikkat, bu şıkkı seçerseniz önceki profilinizin üzerine yazılır ve geri dönemezsiniz!"), 13.0, 600.0, Color { r: 1.00000, g: 0.53948, b: 0.60383, a: 1.0 }).wrap().w_full()));
    card = card.child(row().w_full().gap(10.0).child(spacer())
        .child(button(s.t("Cancel", "Vazgeç"), false).on_press(|s: &mut S| s.modal = false))
        .child(button(s.t("Apply", "Uygula"), true).on_press(|s: &mut S| s.confirm_apply())));
    let dim = Shape::new().fill_path(&body_path(), Color { r: 0.00304, g: 0.00182, b: 0.00439, a: 0.72 });
    el().absolute(0.0, 0.0).size(W, H)
        .child(full(dim).on_press(|s: &mut S| s.save_open = false))
        .child(col().absolute(320.0, 230.0).child(card))
}

fn overlays(s: &S) -> Vec<El> {
    let mut out = Vec::new();
    let open = s.repo_open || s.pick_open || s.save_open;
    if open {
        out.push(el().absolute(0.0, 0.0).size(W, H).on_press(|s: &mut S| {
            s.repo_open = false;
            s.pick_open = false;
            s.save_open = false;
        }));
    }
    if s.repo_open {
        let mut items = vec![(String::new(), true, menu_row(s.t("+ Create new repo", "+ Yeni depo oluştur"), true).on_press(|s: &mut S| {
            s.creating = true;
            s.repo_open = false;
        }))];
        for r in s.repos.iter().take(12) {
            let name = r.full_name.clone();
            let mark = if r.private { "◆" } else { "◇" };
            items.push((name.clone(), false, menu_row(format!("{mark} {name}"), false).on_press(move |s: &mut S| {
                s.cfg.repo = name.clone();
                s.cfg.guest = false;
                s.repo_open = false;
                s.creating = false;
                s.cfg.save();
                s.load_profiles();
            })));
        }
        out.push(menu("repo-dd", items));
    }
    if s.pick_open {
        let items = s.profiles.iter().map(|p| {
            let name = p.name.clone();
            let key = format!("pick-{name}");
            let text = if name == s.cfg.folder { format!("{name} {}", s.t("(this computer)", "(bu bilgisayar)")) } else { format!("{name}   {} · {}", p.host, p.os) };
            (name.clone(), false, menu_row(text, false).key(key).on_press(move |s: &mut S| {
                s.pick = name.clone();
                s.pick_open = false;
            }))
        }).collect();
        out.push(menu("pick-dd", items));
    }
    if s.save_open {
        let mut opts = vec![NEW.to_string()];
        if s.pick != s.cfg.folder {
            opts.push(s.pick.clone());
        }
        opts.push(s.cfg.folder.clone());
        opts.extend(s.profiles.iter().map(|p| p.name.clone()).filter(|n| *n != s.pick && *n != s.cfg.folder));
        let items = opts.into_iter().map(|v| {
            let text = if v == NEW {
                s.t("+ New profile", "+ Yeni Profil")
            } else if v == s.pick {
                format!("{} {v}", s.t("Update the picked profile:", "Seçilen profili güncelle:"))
            } else if v == s.cfg.folder {
                format!("{v} {}", s.t("(this computer)", "(bu bilgisayar)"))
            } else {
                v.clone()
            };
            let add = v == NEW;
            let key = if add { "save-new".to_string() } else { format!("save-{v}") };
            (v.clone(), add, menu_row(text, add).key(key).on_press(move |s: &mut S| {
                s.save_to = v.clone();
                s.save_open = false;
            }))
        }).collect();
        out.push(menu("save-dd", items));
    }
    out
}

fn view(s: &S, _: &mut Cx<S>) -> El {
    let k = kit();
    let mut root = el().key("root").size(W, H).children(backdrop(s));
    root = root
        .child(row().absolute(102.0, 52.0).gap(12.0).center_y().window_drag()
            .child(row().child(label("Wavy", 30.0, 800.0, INK)).child(label("~", 30.0, 800.0, RED)).child(label("Sync", 30.0, 800.0, INK)))
            .child(mono(format!("WVYSY · v{}", env!("CARGO_PKG_VERSION")), 11.0, MUTED)))
        .child(mono(format!("SYNC.{} // PROFILES.{} // GIT.{}", if s.cfg.auto { "LIVE" } else { "HOLD" }, s.profiles.len(),
            if s.status.error.is_empty() { "OK" } else { "ERR" }), 10.5, DIM).absolute(764.0, 252.0).window_drag())
        .child(mono(format!("~/.config/waveterm → {}", s.cfg.target), 10.5, DIM).absolute(96.0, 484.0))
        .child(row().absolute(1000.0, 244.0).gap(8.0)
            .child(button(if s.tr { "EN" } else { "TR" }, false).on_press(|s: &mut S| s.tr = !s.tr))
            .child(button("▁", false).on_press_tx(|_: &mut S, tx: &mut Tx| tx.minimize()))
            .child(button("✕", false).on_press_tx(|_: &mut S, tx: &mut Tx| tx.quit())))
        .child(left_lobe(s, &k))
        .child(col().absolute(672.0, 290.0).w(442.0).child(right_lobe(s)))
        .children(orb(s));
    let status = if !s.busy.is_empty() { (s.busy.clone(), SOFT) } else if !s.err.is_empty() { (s.err.clone(), RED2) } else { (String::new(), SOFT) };
    root = root.child(label(status.0, 12.0, 600.0, status.1).absolute(690.0, 612.0));
    if s.modal {
        root = root.child(modal(s));
    }
    rune_kit::root(root.children(overlays(s)), Vec::<El>::new())
}

// Demo states for README screenshots: made-up names, nothing from this computer.
fn demo(s: &mut S) {
    use crate::ext::Dep;
    s.demo = true;
    s.busy.clear();
    s.err.clear();
    s.tr = false;
    s.counts = [2, 5, 14];
    s.exts = vec![
        Ext { id: "clock".into(), label: "clock".into(), deps: vec![Dep::Git { url: "https://github.com/someone/wave-clock".into(), path: "{HOME}/code/wave-clock".into(), build: vec![] }] },
        Ext { id: "chat".into(), label: "chat".into(), deps: vec![Dep::Git { url: "https://github.com/someone/chat-cli".into(), path: "{HOME}/code/chat-cli".into(), build: vec!["npm ci".into(), "npm run build".into()] }, Dep::File { path: "{HOME}/.local/bin/chat".into(), exec: true }] },
        Ext { id: "btop".into(), label: "btop".into(), deps: vec![Dep::Brew { formula: "btop".into() }] },
    ];
    s.cfg.folder = "studio-mac-7f3a".into();
    s.cfg.target = s.cfg.folder.clone();
    s.folder = TextEdit::new(s.cfg.folder.clone());
    s.cfg.repo = "someone/wave-profiles".into();
    s.login = "someone".into();
    s.cfg.guest = false;
    s.repos = vec![RepoInfo { full_name: "someone/wave-profiles".into(), private: true }, RepoInfo { full_name: "someone/dotfiles".into(), private: false }];
    s.profiles = ["studio-mac-7f3a", "work-imac-2c91", "ubuntu-lab-04be"].iter().map(|n| Profile { name: n.to_string(), host: n.rsplit_once('-').unwrap().0.into(), os: if n.starts_with("ubuntu") { "linux".into() } else { "macos".into() }, updated: 0 }).collect();
    s.pick = "work-imac-2c91".into();
    let now = repo::now();
    s.status = Status { last_check: now, last_push: now - 12, last_hash: 0, error: String::new(), feed: vec![
        (now - 12, "widgets.json · push 1f3a9c2".into()),
        (now - 1040, "workspaces.json · push 77b01de".into()),
        (now - 2800, "settings.json · push 0c55e18".into()),
        (now - 3900, "3 files · push a90d4f3".into()),
    ] };
}

fn init_demo() -> S {
    let mut s = init();
    demo(&mut s);
    s.cfg.setup_done = true;
    s
}

/// `demo`: the real window on made-up data. Settings go to a temporary folder, and the keychain,
/// GitHub and Wave are left alone.
pub fn run(demo_mode: bool) {
    if demo_mode {
        // SAFETY: still single threaded; nothing has read the environment yet.
        unsafe {
            std::env::set_var("WAVYSYNC_HOME", std::env::temp_dir().join("wavysync-demo"));
            std::env::set_var("WAVYSYNC_TOKEN", "");
        }
    }
    let spec = AppSpec {
        setup: Some(setup),
        poll: Some(poll),
        families: ("Inter", "ui-monospace"),
        scenes: &[
            ("sync", |s| { demo(s); s.cfg.setup_done = false; s.step = 1; }),
            ("repo", |s| { demo(s); s.cfg.setup_done = false; s.step = 2; s.repo_open = true; }),
            ("profile", |s| { demo(s); s.cfg.setup_done = true; s.step = 3; }),
            ("tour", |s| { demo(s); s.cfg.setup_done = false; s.step = 1; s.ext_open = false; }),
            ("apply", |s| { demo(s); s.cfg.setup_done = true; s.step = 3; s.open_apply(); }),
            ("live", |s| { demo(s); s.cfg.setup_done = true; s.step = 1; }),
            ("flow", |s| { demo(s); s.cfg.setup_done = true; s.step = 1; s.flowing = true; s.flow.speed = FLOW_V; s.flow.phase = 0.37; s.busy = "Syncing…".into(); }),
            ("tr", |s| { demo(s); s.tr = true; s.cfg.setup_done = true; s.step = 3; s.open_apply(); }),
        ],
        ..AppSpec::new("WavySync", (W, H), if demo_mode { init_demo } else { init }, view)
    };
    // WAVYSYNC_BACKGROUND opens the window offscreen without focus, for smoke tests.
    let background = std::env::var_os("WAVYSYNC_BACKGROUND").is_some();
    let options = WindowOptions {
        transparent: true,
        borderless: true,
        resizable: false,
        position: background.then_some((-4000, 100)),
        ..Default::default()
    };
    let app = App::new(spec).window_options(options);
    if background { app.open_inactive().run() } else { app.run() }
}
