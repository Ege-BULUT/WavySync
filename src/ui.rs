use rune::{AppSpec, Cx, El, col, text};

/// Rune's headless tooling flags (`--snap`, `--script`, ...) go to `rune::run`.
pub fn is_tooling_arg(a: &str) -> bool {
    ["--snap", "--script", "--tree", "--list-scenes", "--scene", "--size", "--video", "--desktop", "--native"].contains(&a)
}

fn view(_: &(), _: &mut Cx<()>) -> El {
    col().w_full().h_full().center().child(text("WavySync"))
}

pub fn run() {
    rune::run(AppSpec::new("WavySync", (320.0, 200.0), || (), view));
}
