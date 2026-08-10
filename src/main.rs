use clap::Parser;
use std::process::ExitCode;

fn main() -> ExitCode {
    let cli = vdiff::cli::Cli::parse();
    match vdiff::run(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("vdiff: {err}");
            ExitCode::FAILURE
        }
    }
}
