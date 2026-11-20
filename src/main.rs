#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod ext;
mod repo;
mod ui;
mod wave;

const HELP: &str = "WavySync: sync Wave Terminal to a GitHub repo, one folder per computer.

  wavysync                 open the window
  wavysync --open          open the window detached (what the Wave widget runs)
  wavysync --daemon        the background loop that pushes changes
  wavysync --sync          push now
  wavysync --profiles      list the profiles in the repo
  wavysync --apply NAME [--save-to FOLDER]
                           set this Wave to profile NAME
  wavysync --detect        print the widgets found and what each needs
  wavysync --setup REPO [--folder NAME] [--guest]
                           set up without the window";

fn arg_after(args: &[String], flag: &str) -> Option<String> {
    args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned()
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let first = args.first().map(String::as_str).unwrap_or("");
    let ok_or_exit = |r: Result<(), String>| {
        if let Err(e) = r {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };
    match first {
        "--help" | "-h" => println!("{HELP}"),
        "--open" => app::spawn_detached(&[]),
        "--daemon" => app::daemon(),
        "--sync" => ok_or_exit(app::sync_once(&app::Config::load(), true).map(|c| match c {
            Some(c) => println!("pushed {c}"),
            None => println!("nothing changed"),
        })),
        "--profiles" => {
            let r = app::Config::load().repo();
            ok_or_exit(r.refresh().map(|_| {
                for p in r.profiles() {
                    println!("{}\t{}\t{}\t{}", p.name, p.host, p.os, p.updated);
                }
            }))
        }
        "--apply" => {
            let name = args.get(1).cloned().unwrap_or_default();
            let save_to = arg_after(&args, "--save-to").unwrap_or_else(|| app::Config::load().folder);
            ok_or_exit(app::apply_profile(&name, &save_to).map(|r| r.iter().for_each(|l| println!("{l}"))))
        }
        "--detect" => {
            let (exts, files) = ext::detect(&wave::read_widgets(&wave::paths().config));
            println!("{}", serde_json::to_string_pretty(&exts).unwrap());
            files.keys().for_each(|k| println!("carried: {k}"));
        }
        "--setup" => {
            let mut cfg = app::Config::load();
            cfg.repo = args.get(1).cloned().unwrap_or_default();
            cfg.guest = args.iter().any(|a| a == "--guest");
            if let Some(f) = arg_after(&args, "--folder") {
                cfg.folder = f.clone();
                cfg.target = f;
            }
            cfg.setup_done = true;
            cfg.save();
            ok_or_exit(app::install_widget().and_then(|_| app::install_autostart()));
            println!("saving to {} in {}", cfg.target, cfg.repo);
        }
        _ if first.starts_with("--") && !ui::is_tooling_arg(first) => {
            eprintln!("unknown option {first}\n\n{HELP}");
            std::process::exit(2);
        }
        _ => ui::run(),
    }
}
