pub mod app;
pub mod cli;
pub mod diff;
pub mod git;

use std::io::IsTerminal;

pub fn run(cli: &cli::Cli) -> Result<(), Box<dyn std::error::Error>> {
    // Bare `vdiff` or a single positional fall through to files mode
    // with empty paths — say what the user should have typed instead
    // of "No such file or directory" (plain) or "binary or unreadable"
    // (TUI).
    if let cli::Command::Files { file1, file2 } = &cli.command
        && (file1.as_os_str().is_empty() || file2.as_os_str().is_empty())
    {
        return Err("usage: vdiff <FILE1> <FILE2> | vdiff git [--cached] [REVS...]".into());
    }
    if std::io::stdout().is_terminal() {
        app::run_tui(cli)
    } else {
        run_plain(cli)
    }
}

fn run_plain(cli: &cli::Cli) -> Result<(), Box<dyn std::error::Error>> {
    match &cli.command {
        cli::Command::Files { file1, file2 } => {
            let a = std::fs::read_to_string(file1)
                .map_err(|e| format!("reading {}: {e}", file1.display()))?;
            let b = std::fs::read_to_string(file2)
                .map_err(|e| format!("reading {}: {e}", file2.display()))?;
            let grid = diff::compute(&a, &b);
            print!("{}", diff::render_text(&grid));
            Ok(())
        }
        cli::Command::Git { cached, revs } => {
            let g = git::RealGit;
            if !git::in_repo(&g)? {
                return Err(Box::new(git::GitError::NotARepo));
            }
            let spec = git::resolve(&g, *cached, revs)?;
            let files = git::changed_files(&g, &spec)?;
            for file in files {
                println!("=== {} ===", file.new_path);
                match git::load_content(&g, &spec, &file) {
                    Ok(Some((old, new))) => {
                        print!("{}", diff::render_text(&diff::compute(&old, &new)))
                    }
                    Ok(None) => println!("(binary or unreadable)"),
                    Err(e) => println!("(error: {e})"),
                }
            }
            Ok(())
        }
    }
}
