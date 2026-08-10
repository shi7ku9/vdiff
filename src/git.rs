//! Git integration: shell out to the `git` CLI behind the `GitShell`
//! trait so tests can inject a `FakeGit`.

#[cfg(test)]
use std::collections::HashMap;
use std::fmt;
use std::process::Command;

pub trait GitShell {
    /// Run git with `args`; `None` on spawn failure, non-zero exit, or
    /// non-UTF-8 output. `Some("")` is a successful empty output.
    fn output(&self, args: &[&str]) -> Option<String>;
}

pub struct RealGit;

impl GitShell for RealGit {
    fn output(&self, args: &[&str]) -> Option<String> {
        let out = Command::new("git").args(args).output().ok()?;
        if !out.status.success() {
            return None;
        }
        String::from_utf8(out.stdout).ok()
    }
}

/// Where a diff side's content comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// The index (`git cat-file -p :path`).
    Index,
    /// The file on disk.
    Worktree,
    /// A revision (`git cat-file -p <rev>:path`).
    Rev(String),
}

/// Resolved diff sides plus the args passed through to `git diff` for
/// the file list.
#[derive(Debug, Clone)]
pub struct RevSpec {
    pub old: Source,
    pub new: Source,
    pub diff_args: Vec<String>,
}

impl RevSpec {
    pub fn old_label(&self) -> String {
        match &self.old {
            Source::Index => "index".to_string(),
            Source::Worktree => "worktree".to_string(),
            Source::Rev(r) => r.clone(),
        }
    }

    pub fn new_label(&self) -> String {
        match &self.new {
            Source::Index => "index".to_string(),
            Source::Worktree => "worktree".to_string(),
            Source::Rev(r) => r.clone(),
        }
    }
}

#[derive(Debug)]
pub enum GitError {
    NotARepo,
    InvalidRevSpec(String),
    MergeBaseFailed(String),
    GitFailed(String),
}

impl fmt::Display for GitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GitError::NotARepo => write!(f, "not a git repository"),
            GitError::InvalidRevSpec(msg) => write!(f, "invalid revision arguments: {msg}"),
            GitError::MergeBaseFailed(range) => write!(f, "merge-base failed for {range}"),
            GitError::GitFailed(msg) => write!(f, "git failed: {msg}"),
        }
    }
}

impl std::error::Error for GitError {}

/// True when the current directory is inside a git repository.
pub fn in_repo(g: &dyn GitShell) -> bool {
    g.output(&["rev-parse", "--git-dir"])
        .map(|s| !s.is_empty())
        .unwrap_or(false)
}

/// How a rev arg's range separators split it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RangeKind {
    /// `A...B` — compare merge-base(A, B) with B.
    MergeBase,
    /// `A..B` — compare A with B directly.
    Plain,
}

/// Split a rev arg on range separators (`...` first, then `..`).
/// Empty sides default to "HEAD". None when neither separator is
/// present.
fn split_range(r: &str) -> Option<(RangeKind, String, String)> {
    let (kind, pos) = if let Some(pos) = r.find("...") {
        (RangeKind::MergeBase, pos)
    } else if let Some(pos) = r.find("..") {
        (RangeKind::Plain, pos)
    } else {
        return None;
    };
    let (left, rest) = r.split_at(pos);
    let right = &rest[kind_len(kind)..];
    let left = if left.is_empty() { "HEAD" } else { left };
    let right = if right.is_empty() { "HEAD" } else { right };
    Some((kind, left.to_string(), right.to_string()))
}

fn kind_len(kind: RangeKind) -> usize {
    match kind {
        RangeKind::MergeBase => 3,
        RangeKind::Plain => 2,
    }
}

fn merge_base(g: &dyn GitShell, a: &str, b: &str) -> Result<String, GitError> {
    match g.output(&["merge-base", a, b]) {
        Some(hash) => Ok(hash.trim().to_string()),
        None => Err(GitError::MergeBaseFailed(format!("{a}...{b}"))),
    }
}

/// Resolve rev arguments into diff sides, following `git diff`
/// semantics. `revs` is passed through to git unchanged for the file
/// list (in `diff_args`).
pub fn resolve(g: &dyn GitShell, cached: bool, revs: &[String]) -> Result<RevSpec, GitError> {
    if cached {
        if revs.len() > 1 {
            return Err(GitError::InvalidRevSpec("--cached takes at most one revision".to_string()));
        }
        let old = match revs.first() {
            Some(r) => Source::Rev(r.clone()),
            None => Source::Rev("HEAD".to_string()),
        };
        let mut diff_args = vec!["--cached".to_string()];
        diff_args.extend(revs.iter().cloned());
        return Ok(RevSpec { old, new: Source::Index, diff_args });
    }
    match revs.len() {
        0 => Ok(RevSpec { old: Source::Index, new: Source::Worktree, diff_args: vec![] }),
        1 => {
            let r = &revs[0];
            if let Some((kind, a, b)) = split_range(r) {
                Ok(match kind {
                    RangeKind::MergeBase => {
                        let base = merge_base(g, &a, &b)?;
                        RevSpec { old: Source::Rev(base), new: Source::Rev(b), diff_args: vec![r.clone()] }
                    }
                    RangeKind::Plain => {
                        RevSpec { old: Source::Rev(a), new: Source::Rev(b), diff_args: vec![r.clone()] }
                    }
                })
            } else {
                Ok(RevSpec { old: Source::Rev(r.clone()), new: Source::Worktree, diff_args: vec![r.clone()] })
            }
        }
        _ => {
            let first = revs[0].clone();
            let second = &revs[1];
            if let Some((kind, a, b)) = split_range(second) {
                Ok(match kind {
                    RangeKind::MergeBase => {
                        let base = merge_base(g, &a, &b)?;
                        RevSpec { old: Source::Rev(base), new: Source::Rev(b), diff_args: revs.to_vec() }
                    }
                    RangeKind::Plain => {
                        RevSpec { old: Source::Rev(a), new: Source::Rev(b), diff_args: revs.to_vec() }
                    }
                })
            } else {
                Ok(RevSpec {
                    old: Source::Rev(first),
                    new: Source::Rev(second.clone()),
                    diff_args: revs.to_vec(),
                })
            }
        }
    }
}

#[cfg(test)]
#[derive(Default)]
pub struct FakeGit {
    map: HashMap<String, Option<String>>,
}

#[cfg(test)]
impl FakeGit {
    pub fn set(&mut self, args: &[&str], out: Option<String>) {
        self.map.insert(args.join(" "), out);
    }
}

#[cfg(test)]
impl GitShell for FakeGit {
    fn output(&self, args: &[&str]) -> Option<String> {
        self.map.get(&args.join(" ")).cloned().unwrap_or(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g() -> FakeGit { FakeGit::default() }

    #[test]
    fn in_repo_checks_git_dir() {
        let mut f = g();
        assert!(!in_repo(&f));
        f.set(&["rev-parse", "--git-dir"], Some(".git".to_string()));
        assert!(in_repo(&f));
    }

    #[test]
    fn resolve_bare() {
        let spec = resolve(&g(), false, &[]).unwrap();
        assert_eq!(spec.old, Source::Index);
        assert_eq!(spec.new, Source::Worktree);
        assert!(spec.diff_args.is_empty());
    }

    #[test]
    fn resolve_cached() {
        let spec = resolve(&g(), true, &[]).unwrap();
        assert_eq!(spec.old, Source::Rev("HEAD".to_string()));
        assert_eq!(spec.new, Source::Index);
        assert_eq!(spec.diff_args, vec!["--cached"]);
    }

    #[test]
    fn resolve_cached_with_rev() {
        let spec = resolve(&g(), true, &["R".to_string()]).unwrap();
        assert_eq!(spec.old, Source::Rev("R".to_string()));
        assert_eq!(spec.new, Source::Index);
        assert_eq!(spec.diff_args, vec!["--cached", "R"]);
    }

    #[test]
    fn resolve_cached_with_two_revs_fails() {
        assert!(matches!(resolve(&g(), true, &["a".to_string(), "b".to_string()]), Err(GitError::InvalidRevSpec(_))));
    }

    #[test]
    fn resolve_single_rev_worktree() {
        let spec = resolve(&g(), false, &["HEAD^".to_string()]).unwrap();
        assert_eq!(spec.old, Source::Rev("HEAD^".to_string()));
        assert_eq!(spec.new, Source::Worktree);
    }

    #[test]
    fn resolve_two_dot_range_single_arg() {
        let spec = resolve(&g(), false, &["HEAD^..HEAD".to_string()]).unwrap();
        assert_eq!(spec.old, Source::Rev("HEAD^".to_string()));
        assert_eq!(spec.new, Source::Rev("HEAD".to_string()));
    }

    #[test]
    fn resolve_three_dot_uses_merge_base() {
        let mut f = g();
        f.set(&["merge-base", "a", "b"], Some("base123".to_string()));
        let spec = resolve(&f, false, &["a...b".to_string()]).unwrap();
        assert_eq!(spec.old, Source::Rev("base123".to_string()));
        assert_eq!(spec.new, Source::Rev("b".to_string()));
    }

    #[test]
    fn resolve_leading_three_dot_defaults_to_head() {
        let mut f = g();
        f.set(&["merge-base", "HEAD", "HEAD^"], Some("base9".to_string()));
        let spec = resolve(&f, false, &["...HEAD^".to_string()]).unwrap();
        assert_eq!(spec.old, Source::Rev("base9".to_string()));
        assert_eq!(spec.new, Source::Rev("HEAD^".to_string()));
    }

    #[test]
    fn resolve_two_plain_revs() {
        let spec = resolve(&g(), false, &["a".to_string(), "b".to_string()]).unwrap();
        assert_eq!(spec.old, Source::Rev("a".to_string()));
        assert_eq!(spec.new, Source::Rev("b".to_string()));
    }

    #[test]
    fn resolve_range_in_second_arg() {
        let mut f = g();
        f.set(&["merge-base", "b", "c"], Some("base5".to_string()));
        let spec = resolve(&f, false, &["a".to_string(), "b...c".to_string()]).unwrap();
        assert_eq!(spec.old, Source::Rev("base5".to_string()));
        assert_eq!(spec.new, Source::Rev("c".to_string()));
        assert_eq!(spec.diff_args, vec!["a", "b...c"]);
    }

    #[test]
    fn resolve_merge_base_failure() {
        let f = g(); // no merge-base seeded → None
        assert!(matches!(resolve(&f, false, &["a...b".to_string()]), Err(GitError::MergeBaseFailed(_))));
    }

    #[test]
    fn labels() {
        assert_eq!(RevSpec { old: Source::Index, new: Source::Worktree, diff_args: vec![] }.old_label(), "index");
        assert_eq!(RevSpec { old: Source::Index, new: Source::Worktree, diff_args: vec![] }.new_label(), "worktree");
        assert_eq!(RevSpec { old: Source::Rev("HEAD".into()), new: Source::Index, diff_args: vec![] }.old_label(), "HEAD");
    }
}
