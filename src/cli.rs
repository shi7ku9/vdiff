use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use std::path::PathBuf;

/// Top-level CLI. Files mode is the default (`vdiff a.txt b.txt`);
/// `vdiff git [--cached] [revs...]` switches to repository mode.
///
/// The pinned shape — a non-optional `command: Command` field — cannot
/// be derived directly: a required clap subcommand rejects bare
/// positional files (`vdiff a.txt b.txt` → "unrecognized subcommand").
/// So the derive lives on [`RawCli`] below, and [`Cli`] implements
/// `Parser` by delegating to it.
#[derive(Debug)]
pub struct Cli {
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Diff two files
    Files {
        file1: PathBuf,
        file2: PathBuf,
    },
    /// Diff revisions of a git repository (git diff semantics)
    Git {
        /// Compare the index against the given revision (default: HEAD)
        #[arg(long)]
        cached: bool,
        /// Revisions passed through to git diff, e.g. HEAD^, A..B, A...B
        #[arg(allow_hyphen_values = true)]
        revs: Vec<String>,
    },
}

/// The clap-derive surface: two positional files (files mode), plus
/// the `git` subcommand. `vdiff <file1> <file2>` fills the positionals
/// (`command` is `None`); `vdiff git ...` picks the subcommand.
#[derive(Debug, Parser)]
#[command(name = "vdiff", version, about = "A vertical diff viewer: diffs files column-by-column instead of line-by-line")]
struct RawCli {
    /// Diff two files: vdiff <file1> <file2>
    #[arg(value_name = "FILE1")]
    file1: Option<PathBuf>,
    #[arg(value_name = "FILE2")]
    file2: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<RawCommand>,
}

/// The git subcommand surface, mirroring [`Command::Git`].
#[derive(Debug, Subcommand)]
enum RawCommand {
    #[command(name = "git")]
    Git {
        /// Compare the index against the given revision (default: HEAD)
        #[arg(long)]
        cached: bool,
        /// Revisions passed through to git diff, e.g. HEAD^, A..B, A...B
        #[arg(allow_hyphen_values = true)]
        revs: Vec<String>,
    },
}

impl From<RawCli> for Cli {
    fn from(raw: RawCli) -> Cli {
        match raw.command {
            Some(RawCommand::Git { cached, revs }) => Cli {
                command: Command::Git { cached, revs },
            },
            // Bare `vdiff` (no args) falls through to files mode with
            // empty paths, which fails with the standard io error in
            // `run_plain` ("vdiff: No such file or directory").
            None => Cli {
                command: Command::Files {
                    file1: raw.file1.unwrap_or_default(),
                    file2: raw.file2.unwrap_or_default(),
                },
            },
        }
    }
}

impl FromArgMatches for Cli {
    fn from_arg_matches(matches: &clap::ArgMatches) -> Result<Self, clap::Error> {
        Ok(RawCli::from_arg_matches(matches)?.into())
    }

    fn update_from_arg_matches(&mut self, matches: &clap::ArgMatches) -> Result<(), clap::Error> {
        *self = RawCli::from_arg_matches(matches)?.into();
        Ok(())
    }
}

impl CommandFactory for Cli {
    fn command() -> clap::Command {
        RawCli::command()
    }

    fn command_for_update() -> clap::Command {
        RawCli::command_for_update()
    }
}

impl Parser for Cli {}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn parses_files_mode() {
        let cli = Cli::parse_from(["vdiff", "a.txt", "b.txt"]);
        assert!(matches!(cli.command, Command::Files { .. }));
    }

    #[test]
    fn parses_git_bare() {
        let cli = Cli::parse_from(["vdiff", "git"]);
        assert!(matches!(cli.command, Command::Git { cached: false, revs } if revs.is_empty()));
    }

    #[test]
    fn parses_git_cached() {
        let cli = Cli::parse_from(["vdiff", "git", "--cached"]);
        assert!(matches!(cli.command, Command::Git { cached: true, .. }));
    }

    #[test]
    fn parses_git_revs() {
        let cli = Cli::parse_from(["vdiff", "git", "HEAD^"]);
        assert!(matches!(cli.command, Command::Git { cached: false, revs } if revs == vec!["HEAD^"]));
        let cli = Cli::parse_from(["vdiff", "git", "xx...xx"]);
        assert!(matches!(cli.command, Command::Git { cached: false, revs } if revs == vec!["xx...xx"]));
    }
}
