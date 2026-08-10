pub mod app;
pub mod cli;
pub mod diff;
pub mod git;

use std::io::IsTerminal;

pub fn run(cli: &cli::Cli) -> Result<(), Box<dyn std::error::Error>> {
    if std::io::stdout().is_terminal() {
        app::run_tui(cli)
    } else {
        run_plain(cli)
    }
}

fn run_plain(cli: &cli::Cli) -> Result<(), Box<dyn std::error::Error>> {
    match &cli.command {
        cli::Command::Files { file1, file2 } => {
            let a = std::fs::read_to_string(file1)?;
            let b = std::fs::read_to_string(file2)?;
            let grid = diff::compute(&a, &b);
            print!("{}", diff::render_text(&grid));
            Ok(())
        }
        cli::Command::Git { cached, revs } => {
            let g = git::RealGit;
            if !git::in_repo(&g) {
                return Err(Box::new(git::GitError::NotARepo));
            }
            let spec = git::resolve(&g, *cached, revs)?;
            for file in git::changed_files(&g, &spec) {
                println!("=== {} ===", file.new_path);
                match git::load_content(&g, &spec, &file) {
                    Some((old, new)) => {
                        print!("{}", diff::render_text(&diff::compute(&old, &new)))
                    }
                    None => println!("(binary or unreadable)"),
                }
            }
            Ok(())
        }
    }
}
