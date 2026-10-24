mod annot;
mod app;
mod editor;
mod geom;
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
        let (n, warning) = match pdf::clear(&path) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("vidf: {e}");
                return ExitCode::FAILURE;
            }
        };
        let removed = format!("removed {n} annotation{}", if n == 1 { "" } else { "s" });
        return match warning {
            None if n == 0 => {
                println!("{}: no ViDF annotations", path.display());
                ExitCode::SUCCESS
            }
            None => {
                println!("{removed}; {} is the original file again", path.display());
                ExitCode::SUCCESS
            }
            Some(w) => {
                if n > 0 {
                    println!("{removed}");
                }
                eprintln!("vidf: {w}; {} is not the original file", path.display());
                ExitCode::FAILURE
            }
        };
    }
    match app::run(path) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("vidf: {e}");
            ExitCode::FAILURE
        }
    }
}
