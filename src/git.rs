//! Git integration: shell out to the `git` CLI behind the `GitShell`
//! trait so tests can inject a `FakeGit`.

#[cfg(test)]
use std::collections::HashMap;
use std::fmt;
use std::process::Command;

pub trait GitShell {
    /// Run git with `args`; `Err` on spawn failure, non-zero exit, or
    /// non-UTF-8 output. `Ok("")` is a successful empty output.
    fn output(&self, args: &[&str]) -> Result<String, GitRunError>;
    /// Like `output`, but decodes non-UTF-8 output lossily. Use for
    /// output that must not fail on exotic bytes (e.g. file lists).
    fn output_lossy(&self, args: &[&str]) -> Result<String, GitRunError> {
        self.output(args)
    }
}

/// Why a git invocation failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitRunError {
    /// The git executable could not be spawned (e.g. not on PATH).
    SpawnFailed,
    /// git ran but exited non-zero.
    NonZero { code: Option<i32>, stderr: String },
    /// The output was not valid UTF-8.
    NonUtf8,
}

impl fmt::Display for GitRunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GitRunError::SpawnFailed => write!(f, "could not run git (is it installed?)"),
            GitRunError::NonZero { code, stderr } => {
                let code = code
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "?".to_string());
                if stderr.is_empty() {
                    write!(f, "git exited with status {code}")
                } else {
                    write!(f, "git exited with status {code}: {stderr}")
                }
            }
            GitRunError::NonUtf8 => write!(f, "git produced non-UTF-8 output"),
        }
    }
}

impl std::error::Error for GitRunError {}

pub struct RealGit;

impl GitShell for RealGit {
    fn output(&self, args: &[&str]) -> Result<String, GitRunError> {
        let out = match Command::new("git").args(args).output() {
            Ok(out) => out,
            Err(_) => return Err(GitRunError::SpawnFailed),
        };
        if !out.status.success() {
            return Err(GitRunError::NonZero {
                code: out.status.code(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            });
        }
        String::from_utf8(out.stdout).map_err(|_| GitRunError::NonUtf8)
    }

    fn output_lossy(&self, args: &[&str]) -> Result<String, GitRunError> {
        let out = match Command::new("git").args(args).output() {
            Ok(out) => out,
            Err(_) => return Err(GitRunError::SpawnFailed),
        };
        if !out.status.success() {
            return Err(GitRunError::NonZero {
                code: out.status.code(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            });
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
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
    /// Repo root (absolute, no trailing slash) that worktree and
    /// index paths from `git diff --name-status` are relative to.
    /// Empty when the root cannot be decoded or when paths are
    /// cwd-relative (see `prefix`).
    pub toplevel: String,
    /// Cwd relative to the root, with trailing slash ("sub/"). Under
    /// `diff.relative` git reports cwd-relative paths, so `prefix +
    /// path` gives the root-relative one every side needs.
    pub prefix: String,
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

impl From<GitRunError> for GitError {
    fn from(e: GitRunError) -> Self {
        GitError::GitFailed(e.to_string())
    }
}

/// True when the current directory is inside a git repository.
/// `Ok(false)` for a non-zero exit (outside a repo); other failures
/// (missing git binary, non-UTF-8 output) are errors.
pub fn in_repo(g: &dyn GitShell) -> Result<bool, GitRunError> {
    match g.output(&["rev-parse", "--git-dir"]) {
        Ok(s) => Ok(!s.is_empty()),
        Err(GitRunError::NonZero { .. }) => Ok(false),
        Err(e) => Err(e),
    }
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
    } else {
        let pos = r.find("..")?;
        (RangeKind::Plain, pos)
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
        Ok(hash) => Ok(hash.trim().to_string()),
        Err(e) => Err(GitError::MergeBaseFailed(format!("{a}...{b}: {e}"))),
    }
}

/// Repo layout for resolving diff paths, resolved once per spec that
/// reads the worktree: the repo root that paths are relative to, and
/// the cwd's prefix inside it. Under `diff.relative` git reports
/// cwd-relative paths, so the prefix must be prepended to make them
/// root-relative for every side (worktree reads and `cat-file`).
/// An empty root (undecodable, e.g. non-UTF-8) falls back to
/// cwd-relative reads, which is right when running from the root.
fn worktree_layout(g: &dyn GitShell) -> (String, String) {
    let relative = g
        .output(&["config", "--bool", "diff.relative"])
        .map(|s| s.trim() == "true")
        .unwrap_or(false);
    let toplevel = g
        .output_lossy(&["rev-parse", "--show-toplevel"])
        .map(|s| s.trim_end_matches(['\n', '\r']).to_string())
        .unwrap_or_default();
    if relative && !toplevel.is_empty() {
        let prefix = g
            .output_lossy(&["rev-parse", "--show-prefix"])
            .map(|s| s.trim_end_matches(['\n', '\r']).to_string())
            .unwrap_or_default();
        (toplevel, prefix)
    } else {
        (toplevel, String::new())
    }
}

/// Both sides of a range must not look like options: git reads
/// "A..-B" as a range plus a switch.
fn check_components(a: &str, b: &str) -> Result<(), GitError> {
    if a.starts_with('-') || b.starts_with('-') {
        return Err(GitError::InvalidRevSpec(
            "range sides must not start with '-'".to_string(),
        ));
    }
    Ok(())
}

/// Resolve rev arguments into diff sides, following `git diff`
/// semantics. `revs` is passed through to git unchanged for the file
/// list (in `diff_args`).
pub fn resolve(g: &dyn GitShell, cached: bool, revs: &[String]) -> Result<RevSpec, GitError> {
    // A revision that looks like an option would be read by git as
    // one (e.g. --output=<file> writes a diff file).
    if let Some(bad) = revs.iter().find(|r| r.starts_with('-')) {
        return Err(GitError::InvalidRevSpec(format!(
            "revision arguments must not start with '-': {bad}"
        )));
    }
    if cached {
        if revs.len() > 1 {
            return Err(GitError::InvalidRevSpec(
                "--cached takes at most one revision".to_string(),
            ));
        }
        let old = match revs.first() {
            Some(r) => Source::Rev(r.clone()),
            None => Source::Rev("HEAD".to_string()),
        };
        let mut diff_args = vec!["--cached".to_string()];
        diff_args.extend(revs.iter().cloned());
        return Ok(RevSpec {
            old,
            new: Source::Index,
            diff_args,
            toplevel: String::new(),
            prefix: String::new(),
        });
    }
    match revs.len() {
        0 => {
            let (toplevel, prefix) = worktree_layout(g);
            Ok(RevSpec {
                old: Source::Index,
                new: Source::Worktree,
                diff_args: vec![],
                toplevel,
                prefix,
            })
        }
        1 => {
            let r = &revs[0];
            if let Some((kind, a, b)) = split_range(r) {
                check_components(&a, &b)?;
                Ok(match kind {
                    RangeKind::MergeBase => {
                        let base = merge_base(g, &a, &b)?;
                        RevSpec {
                            old: Source::Rev(base),
                            new: Source::Rev(b),
                            diff_args: vec![r.clone()],
                            toplevel: String::new(),
                            prefix: String::new(),
                        }
                    }
                    RangeKind::Plain => RevSpec {
                        old: Source::Rev(a),
                        new: Source::Rev(b),
                        diff_args: vec![r.clone()],
                        toplevel: String::new(),
                        prefix: String::new(),
                    },
                })
            } else {
                let (toplevel, prefix) = worktree_layout(g);
                Ok(RevSpec {
                    old: Source::Rev(r.clone()),
                    new: Source::Worktree,
                    diff_args: vec![r.clone()],
                    toplevel,
                    prefix,
                })
            }
        }
        2 => {
            let first = revs[0].clone();
            let second = &revs[1];
            if let Some((kind, a, b)) = split_range(second) {
                check_components(&a, &b)?;
                Ok(match kind {
                    RangeKind::MergeBase => {
                        let base = merge_base(g, &a, &b)?;
                        RevSpec {
                            old: Source::Rev(base),
                            new: Source::Rev(b),
                            diff_args: revs.to_vec(),
                            toplevel: String::new(),
                            prefix: String::new(),
                        }
                    }
                    RangeKind::Plain => RevSpec {
                        old: Source::Rev(a),
                        new: Source::Rev(b),
                        diff_args: revs.to_vec(),
                        toplevel: String::new(),
                        prefix: String::new(),
                    },
                })
            } else {
                Ok(RevSpec {
                    old: Source::Rev(first),
                    new: Source::Rev(second.clone()),
                    diff_args: revs.to_vec(),
                    toplevel: String::new(),
                    prefix: String::new(),
                })
            }
        }
        _ => Err(GitError::InvalidRevSpec(format!(
            "expected at most two revisions, got {}",
            revs.len()
        ))),
    }
}

#[cfg(test)]
#[derive(Default)]
pub struct FakeGit {
    map: HashMap<String, Result<String, GitRunError>>,
}

#[cfg(test)]
impl FakeGit {
    /// Seed a successful answer; `None` seeds a non-zero exit.
    pub fn set(&mut self, args: &[&str], out: Option<String>) {
        let result = out.map(Ok).unwrap_or(Err(GitRunError::NonZero {
            code: Some(1),
            stderr: "fake failure".to_string(),
        }));
        self.map.insert(args.join(" "), result);
    }

    /// Seed a non-zero exit with the given stderr.
    pub fn set_failure(&mut self, args: &[&str], stderr: &str) {
        self.map.insert(
            args.join(" "),
            Err(GitRunError::NonZero {
                code: Some(128),
                stderr: stderr.to_string(),
            }),
        );
    }

    /// Seed non-UTF-8 output (what git emits for binary blobs).
    pub fn set_binary(&mut self, args: &[&str]) {
        self.map.insert(args.join(" "), Err(GitRunError::NonUtf8));
    }
}

#[cfg(test)]
impl GitShell for FakeGit {
    fn output(&self, args: &[&str]) -> Result<String, GitRunError> {
        self.map
            .get(&args.join(" "))
            .cloned()
            .unwrap_or(Err(GitRunError::NonZero {
                code: Some(1),
                stderr: "fake failure".to_string(),
            }))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Modified,
    Added,
    Deleted,
    Renamed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedFile {
    pub status: Status,
    pub old_path: String,
    pub new_path: String,
}

/// Parse `git diff --name-status -z` output: fields NUL-separated,
/// each file a status letter (possibly `R` plus a similarity score)
/// followed by one path, or by old and new paths for renames/copies.
/// Only the NUL separates fields; a tab inside a filename is data.
/// The field stream is positional — no content sniffing, so a
/// single-letter path cannot be mistaken for a status. Unknown status
/// letters are treated as Modified.
pub fn parse_name_status_z(out: &str) -> Vec<ChangedFile> {
    let fields: Vec<&str> = out.split('\0').filter(|f| !f.is_empty()).collect();
    let mut files = Vec::new();
    let mut i = 0;
    while i < fields.len() {
        let letter = fields[i].chars().next().unwrap_or('\0');
        let status = status_of(letter);
        // Renames and copies carry two paths, old first.
        let n_paths = if letter == 'R' || letter == 'C' { 2 } else { 1 };
        let old = fields.get(i + 1).copied().unwrap_or("");
        let new = if n_paths == 2 {
            fields.get(i + 2).copied().unwrap_or(old)
        } else {
            old
        };
        files.push(ChangedFile {
            status,
            old_path: old.to_string(),
            new_path: new.to_string(),
        });
        i += 1 + n_paths;
    }
    files
}

fn status_of(letter: char) -> Status {
    match letter {
        'A' => Status::Added,
        'D' => Status::Deleted,
        'R' => Status::Renamed,
        _ => Status::Modified,
    }
}

/// The list of changed files for a rev spec, via
/// `git diff <diff_args> --name-status -z`. An `Err` means the command
/// FAILED (bad rev, `--cached` without a HEAD, ...); an empty list is
/// a genuinely clean tree.
pub fn changed_files(g: &dyn GitShell, spec: &RevSpec) -> Result<Vec<ChangedFile>, GitError> {
    let mut args: Vec<String> = vec!["diff".to_string()];
    args.extend(spec.diff_args.iter().cloned());
    args.push("--name-status".to_string());
    args.push("-z".to_string());
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    // Lossy decode: one non-UTF-8 filename must not kill the whole
    // file list (paths are only displayed, never diffed).
    let out = g.output_lossy(&arg_refs).map_err(GitError::from)?;
    Ok(parse_name_status_z(&out))
}

/// Why one diff side's content could not be loaded.
#[derive(Debug, PartialEq, Eq)]
pub enum FetchError {
    /// The side is not text: binary blob, non-UTF-8 bytes, or an
    /// unreadable file. Surfaced as "(binary or unreadable)".
    Binary,
    /// git itself failed to produce the content (missing blob,
    /// submodule, ...).
    GitFailed(GitRunError),
}

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FetchError::Binary => write!(f, "binary or unreadable"),
            FetchError::GitFailed(e) => write!(f, "git failed: {e}"),
        }
    }
}

impl std::error::Error for FetchError {}

fn fetch_source(
    g: &dyn GitShell,
    spec: &RevSpec,
    src: &Source,
    path: &str,
) -> Result<String, FetchError> {
    // Paths from `git diff --name-status` are root-relative (or
    // cwd-relative under diff.relative, when `prefix` carries the
    // cwd); `cat-file` resolves paths against the root too, so every
    // side reads `toplevel + prefix + path`.
    let rooted = format!("{}{path}", spec.prefix);
    match src {
        Source::Worktree => {
            let full = std::path::Path::new(&spec.toplevel).join(&rooted);
            std::fs::read_to_string(full).map_err(|_| FetchError::Binary)
        }
        Source::Index => fetch_blob(g, &format!(":{rooted}")),
        Source::Rev(rev) => fetch_blob(g, &format!("{rev}:{rooted}")),
    }
}

fn fetch_blob(g: &dyn GitShell, object: &str) -> Result<String, FetchError> {
    g.output(&["cat-file", "-p", object]).map_err(|e| match e {
        GitRunError::NonUtf8 => FetchError::Binary,
        e => FetchError::GitFailed(e),
    })
}

/// Fetch (old, new) contents for a changed file. `Ok(None)` means one
/// side is binary or unreadable; `Err` means git itself failed.
/// Added files have an empty old side; deleted files an empty new
/// side.
pub fn load_content(
    g: &dyn GitShell,
    spec: &RevSpec,
    file: &ChangedFile,
) -> Result<Option<(String, String)>, FetchError> {
    let old = match file.status {
        Status::Added => Some(String::new()),
        _ => match fetch_source(g, spec, &spec.old, &file.old_path) {
            Ok(s) => Some(s),
            Err(FetchError::Binary) => None,
            Err(e) => return Err(e),
        },
    };
    let new = match file.status {
        Status::Deleted => Some(String::new()),
        _ => match fetch_source(g, spec, &spec.new, &file.new_path) {
            Ok(s) => Some(s),
            Err(FetchError::Binary) => None,
            Err(e) => return Err(e),
        },
    };
    Ok(match (old, new) {
        (Some(old), Some(new)) => Some((old, new)),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g() -> FakeGit {
        FakeGit::default()
    }

    #[test]
    fn in_repo_checks_git_dir() {
        let mut f = g();
        assert!(!in_repo(&f).unwrap());
        f.set(&["rev-parse", "--git-dir"], Some(".git".to_string()));
        assert!(in_repo(&f).unwrap());
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
        assert!(matches!(
            resolve(&g(), true, &["a".to_string(), "b".to_string()]),
            Err(GitError::InvalidRevSpec(_))
        ));
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
    fn resolve_more_than_two_revs_fails() {
        assert!(matches!(
            resolve(
                &g(),
                false,
                &["a".to_string(), "b".to_string(), "c".to_string()]
            ),
            Err(GitError::InvalidRevSpec(_))
        ));
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
        assert!(matches!(
            resolve(&f, false, &["a...b".to_string()]),
            Err(GitError::MergeBaseFailed(_))
        ));
    }

    #[test]
    fn labels() {
        assert_eq!(
            RevSpec {
                old: Source::Index,
                new: Source::Worktree,
                diff_args: vec![],
                toplevel: String::new(),
                prefix: String::new()
            }
            .old_label(),
            "index"
        );
        assert_eq!(
            RevSpec {
                old: Source::Index,
                new: Source::Worktree,
                diff_args: vec![],
                toplevel: String::new(),
                prefix: String::new()
            }
            .new_label(),
            "worktree"
        );
        assert_eq!(
            RevSpec {
                old: Source::Rev("HEAD".into()),
                new: Source::Index,
                diff_args: vec![],
                toplevel: String::new(),
                prefix: String::new()
            }
            .old_label(),
            "HEAD"
        );
    }

    #[test]
    fn parse_name_status_z_basic() {
        let out = "M\0src/lib.rs\0A\0src/main.rs\0D\0old.cpp\0R100\0a.cpp\0b.cpp\0";
        let files = parse_name_status_z(out);
        assert_eq!(files.len(), 4);
        assert_eq!(files[0].status, Status::Modified);
        assert_eq!(files[0].old_path, "src/lib.rs");
        assert_eq!(files[0].new_path, "src/lib.rs");
        assert_eq!(files[1].status, Status::Added);
        assert_eq!(files[2].status, Status::Deleted);
        assert_eq!(files[3].status, Status::Renamed);
        assert_eq!(files[3].old_path, "a.cpp");
        assert_eq!(files[3].new_path, "b.cpp");
    }

    #[test]
    fn parse_name_status_z_tab_in_filename() {
        // A literal tab inside a filename is data, not a separator;
        // splitting on it would cut the path and misalign every
        // following record.
        let out = "M\0tab\tname.txt\0A\0other.txt\0";
        let files = parse_name_status_z(out);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].status, Status::Modified);
        assert_eq!(files[0].new_path, "tab\tname.txt");
        assert_eq!(files[1].status, Status::Added);
        assert_eq!(files[1].new_path, "other.txt");
    }

    #[test]
    fn parse_name_status_z_empty() {
        assert!(parse_name_status_z("").is_empty());
    }

    #[test]
    fn parse_name_status_z_real_git_separators() {
        // Real `git diff --name-status -z` separates every field with NUL.
        let out = "M\0src/lib.rs\0R100\0a.cpp\0b.cpp\0";
        let files = parse_name_status_z(out);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].status, Status::Modified);
        assert_eq!(files[0].old_path, "src/lib.rs");
        assert_eq!(files[1].status, Status::Renamed);
        assert_eq!(files[1].old_path, "a.cpp");
        assert_eq!(files[1].new_path, "b.cpp");
    }

    #[test]
    fn parse_name_status_z_single_letter_paths() {
        // A single-letter path ("a") must not be misread as a status.
        let out = "M\0a\0D\0x.rs\0";
        let files = parse_name_status_z(out);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].status, Status::Modified);
        assert_eq!(files[0].old_path, "a");
        assert_eq!(files[0].new_path, "a");
        assert_eq!(files[1].status, Status::Deleted);
        assert_eq!(files[1].old_path, "x.rs");
        assert_eq!(files[1].new_path, "x.rs");
    }

    #[test]
    fn changed_files_builds_diff_args() {
        let mut f = FakeGit::default();
        f.set(
            &["diff", "--cached", "--name-status", "-z"],
            Some("M\0a.txt\0".to_string()),
        );
        let spec = RevSpec {
            old: Source::Rev("HEAD".into()),
            new: Source::Index,
            diff_args: vec!["--cached".into()],
            toplevel: String::new(),
            prefix: String::new(),
        };
        assert_eq!(changed_files(&f, &spec).unwrap().len(), 1);
    }

    #[test]
    fn changed_files_git_failure_is_error() {
        // An unseeded FakeGit returns None, which now means git FAILED
        // (non-zero exit, bad rev, ...), not "no changes".
        let f = FakeGit::default();
        let spec = RevSpec {
            old: Source::Index,
            new: Source::Worktree,
            diff_args: vec![],
            toplevel: String::new(),
            prefix: String::new(),
        };
        assert!(matches!(
            changed_files(&f, &spec),
            Err(GitError::GitFailed(_))
        ));
    }

    #[test]
    fn changed_files_empty_output_is_clean_tree() {
        let mut f = FakeGit::default();
        f.set(&["diff", "--name-status", "-z"], Some(String::new()));
        let spec = RevSpec {
            old: Source::Index,
            new: Source::Worktree,
            diff_args: vec![],
            toplevel: String::new(),
            prefix: String::new(),
        };
        assert_eq!(changed_files(&f, &spec).unwrap(), vec![]);
    }

    #[test]
    fn load_content_rev_sides() {
        let mut f = FakeGit::default();
        f.set(&["cat-file", "-p", "HEAD:a.txt"], Some("foo\n".to_string()));
        f.set(
            &["cat-file", "-p", "HEAD~1:a.txt"],
            Some("bar\n".to_string()),
        );
        let spec = RevSpec {
            old: Source::Rev("HEAD~1".into()),
            new: Source::Rev("HEAD".into()),
            diff_args: vec![],
            toplevel: String::new(),
            prefix: String::new(),
        };
        let file = ChangedFile {
            status: Status::Modified,
            old_path: "a.txt".into(),
            new_path: "a.txt".into(),
        };
        assert_eq!(
            load_content(&f, &spec, &file),
            Ok(Some(("bar\n".to_string(), "foo\n".to_string())))
        );
    }

    #[test]
    fn load_content_index_side() {
        let mut f = FakeGit::default();
        f.set(&["cat-file", "-p", ":a.txt"], Some("staged\n".to_string()));
        f.set(
            &["cat-file", "-p", "HEAD:a.txt"],
            Some("head\n".to_string()),
        );
        let spec = RevSpec {
            old: Source::Index,
            new: Source::Rev("HEAD".into()),
            diff_args: vec![],
            toplevel: String::new(),
            prefix: String::new(),
        };
        let file = ChangedFile {
            status: Status::Modified,
            old_path: "a.txt".into(),
            new_path: "a.txt".into(),
        };
        assert_eq!(
            load_content(&f, &spec, &file).unwrap().unwrap().0,
            "staged\n"
        );
    }

    #[test]
    fn load_content_worktree_side_reads_disk() {
        let path = std::env::temp_dir().join(format!("vdiff-wt-{}", std::process::id()));
        std::fs::write(&path, "disk\n").unwrap();
        let spec = RevSpec {
            old: Source::Index,
            new: Source::Worktree,
            diff_args: vec![],
            toplevel: std::env::temp_dir().to_str().unwrap().to_string(),
            prefix: String::new(),
        };
        let file = ChangedFile {
            status: Status::Modified,
            old_path: path.file_name().unwrap().to_str().unwrap().to_string(),
            new_path: path.file_name().unwrap().to_str().unwrap().to_string(),
        };
        let mut f = FakeGit::default();
        f.set(
            &["cat-file", "-p", &format!(":{}", file.old_path)],
            Some("staged\n".to_string()),
        );
        assert_eq!(load_content(&f, &spec, &file).unwrap().unwrap().1, "disk\n");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_content_added_and_deleted_sides_are_empty() {
        let mut f = FakeGit::default();
        f.set(
            &["cat-file", "-p", "HEAD:new.txt"],
            Some("text\n".to_string()),
        );
        f.set(
            &["cat-file", "-p", "HEAD:gone.txt"],
            Some("gone\n".to_string()),
        );
        let spec = RevSpec {
            old: Source::Rev("HEAD".into()),
            new: Source::Rev("HEAD".into()),
            diff_args: vec![],
            toplevel: String::new(),
            prefix: String::new(),
        };
        let added = ChangedFile {
            status: Status::Added,
            old_path: "new.txt".into(),
            new_path: "new.txt".into(),
        };
        assert_eq!(
            load_content(&f, &spec, &added),
            Ok(Some(("".to_string(), "text\n".to_string())))
        );
        let deleted = ChangedFile {
            status: Status::Deleted,
            old_path: "gone.txt".into(),
            new_path: "gone.txt".into(),
        };
        assert_eq!(
            load_content(&f, &spec, &deleted),
            Ok(Some(("gone\n".to_string(), "".to_string())))
        );
    }

    #[test]
    fn load_content_binary_added_is_none_not_blank() {
        // A binary file added to the repo must read as "binary or
        // unreadable", not as an empty pair that renders a blank pane.
        let mut f = FakeGit::default();
        f.set_binary(&["cat-file", "-p", "HEAD:bin.dat"]);
        let spec = RevSpec {
            old: Source::Rev("HEAD".into()),
            new: Source::Rev("HEAD".into()),
            diff_args: vec![],
            toplevel: String::new(),
            prefix: String::new(),
        };
        let added = ChangedFile {
            status: Status::Added,
            old_path: "bin.dat".into(),
            new_path: "bin.dat".into(),
        };
        assert_eq!(load_content(&f, &spec, &added), Ok(None));
    }

    #[test]
    fn load_content_git_failure_is_error() {
        // A failed fetch (submodule, missing blob) is not "binary";
        // the caller must be able to show git's message.
        let mut f = FakeGit::default();
        f.set_failure(
            &["cat-file", "-p", "HEAD:sm"],
            "fatal: Not a valid object name :sm",
        );
        let spec = RevSpec {
            old: Source::Rev("HEAD".into()),
            new: Source::Rev("HEAD".into()),
            diff_args: vec![],
            toplevel: String::new(),
            prefix: String::new(),
        };
        let file = ChangedFile {
            status: Status::Modified,
            old_path: "sm".into(),
            new_path: "sm".into(),
        };
        let err = load_content(&f, &spec, &file).unwrap_err();
        assert!(err.to_string().contains("Not a valid object name"));
    }

    #[test]
    fn load_content_worktree_joins_repo_root() {
        // `git diff --name-status` reports paths relative to the repo
        // root; the worktree read must join them against it.
        let dir = std::env::temp_dir().join(format!("vdiff-root-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/a.txt"), "disk\n").unwrap();
        let spec = RevSpec {
            old: Source::Rev("HEAD".into()),
            new: Source::Worktree,
            diff_args: vec![],
            toplevel: dir.to_str().unwrap().to_string(),
            prefix: String::new(),
        };
        let file = ChangedFile {
            status: Status::Modified,
            old_path: "sub/a.txt".into(),
            new_path: "sub/a.txt".into(),
        };
        let mut f = FakeGit::default();
        f.set(
            &["cat-file", "-p", ":sub/a.txt"],
            Some("staged\n".to_string()),
        );
        f.set(
            &["cat-file", "-p", "HEAD:sub/a.txt"],
            Some("old\n".to_string()),
        );
        assert_eq!(load_content(&f, &spec, &file).unwrap().unwrap().1, "disk\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_content_binary_is_none() {
        let mut f = FakeGit::default();
        f.set_binary(&["cat-file", "-p", "HEAD:bin.dat"]);
        let spec = RevSpec {
            old: Source::Rev("HEAD".into()),
            new: Source::Rev("HEAD".into()),
            diff_args: vec![],
            toplevel: String::new(),
            prefix: String::new(),
        };
        let file = ChangedFile {
            status: Status::Modified,
            old_path: "bin.dat".into(),
            new_path: "bin.dat".into(),
        };
        assert_eq!(load_content(&f, &spec, &file), Ok(None));
    }

    #[test]
    fn worktree_layout_reads_root_and_prefix() {
        let mut f = FakeGit::default();
        f.set(
            &["config", "--bool", "diff.relative"],
            Some("false\n".to_string()),
        );
        f.set(
            &["rev-parse", "--show-toplevel"],
            Some("/repo\n".to_string()),
        );
        assert_eq!(worktree_layout(&f), ("/repo".to_string(), String::new()));
    }

    #[test]
    fn worktree_layout_prefixes_under_diff_relative() {
        let mut f = FakeGit::default();
        f.set(
            &["config", "--bool", "diff.relative"],
            Some("true\n".to_string()),
        );
        f.set(
            &["rev-parse", "--show-toplevel"],
            Some("/repo\n".to_string()),
        );
        f.set(&["rev-parse", "--show-prefix"], Some("sub/\n".to_string()));
        assert_eq!(
            worktree_layout(&f),
            ("/repo".to_string(), "sub/".to_string())
        );
    }

    #[test]
    fn resolve_rejects_dash_range_components() {
        // "A..-B" passes the whole-arg check but would make git read
        // -B as a switch.
        assert!(matches!(
            resolve(&g(), false, &["A..-B".to_string()]),
            Err(GitError::InvalidRevSpec(_))
        ));
        assert!(matches!(
            resolve(&g(), false, &["A...-B".to_string()]),
            Err(GitError::InvalidRevSpec(_))
        ));
    }

    #[test]
    fn resolve_rejects_dash_prefixed_revisions() {
        assert!(matches!(
            resolve(&g(), false, &["--output=/tmp/x".to_string()]),
            Err(GitError::InvalidRevSpec(_))
        ));
    }
}
