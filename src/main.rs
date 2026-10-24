mod annot;
mod app;
mod editor;
mod pdf;
mod text;
mod view;
mod worker;

use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "usage: vidf [--clear] FILE.pdf

  --clear  remove all ViDF annotations, restoring the original file";

fn main() -> ExitCode {
    let mut clear = false;
    let mut path = None;
    for arg in std::env::args_os().skip(1) {
        match arg.to_str() {
            Some("--clear") => clear = true,
            Some("-h" | "--help") => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            _ if path.is_none() => path = Some(PathBuf::from(arg)),
            _ => {
                eprintln!("{USAGE}");
                return ExitCode::FAILURE;
            }
        }
    }
    let Some(path) = path else {
        eprintln!("{USAGE}");
        return ExitCode::FAILURE;
    };
    if clear {
        let result = pdf::open(&path).and_then(|o| {
            let n = o.annots.len();
            pdf::save(&path, o.base_len, &[]).map(|()| n)
        });
        return match result {
            Ok(n) => {
                println!("removed {n} annotation(s); {} is the original file again", path.display());
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("vidf: {e}");
                ExitCode::FAILURE
            }
        };
    }
    match app::run(path) {
        bevy::app::AppExit::Success => ExitCode::SUCCESS,
        bevy::app::AppExit::Error(_) => ExitCode::FAILURE,
    }
}
