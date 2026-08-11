use std::path::Path;
use std::process::{Command, Output};

fn run_vdiff(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_vdiff"))
        .args(args)
        .output()
        .expect("failed to run vdiff")
}

fn temp_file(name: &str, contents: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("vdiff-cli-{}-{name}", std::process::id()));
    std::fs::write(&path, contents).unwrap();
    path
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("git binary is required for integration tests");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn rev_parse(dir: &Path, rev: &str) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", rev])
        .output()
        .unwrap();
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

/// A repo with one commit containing a.txt = "foo\nbar baz\n".
fn make_repo() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q"]);
    git(dir.path(), &["config", "user.name", "Test User"]);
    git(dir.path(), &["config", "user.email", "test@example.com"]);
    std::fs::write(dir.path().join("a.txt"), "foo\nbar baz\n").unwrap();
    git(dir.path(), &["add", "."]);
    git(
        dir.path(),
        &["-c", "commit.gpgsign=false", "commit", "-q", "-m", "one"],
    );
    let hash = rev_parse(dir.path(), "HEAD");
    (dir, hash)
}

#[test]
fn files_mode_prints_expected_format() {
    let a = temp_file("a.txt", "foo\nbar baz\nquux\n");
    let b = temp_file("b.txt", "foo\nbar qaz\nquux\n");
    let out = run_vdiff(&[a.to_str().unwrap(), b.to_str().unwrap()]);
    let _ = std::fs::remove_file(&a);
    let _ = std::fs::remove_file(&b);
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        "    |-|+|\nfoo | | |  \nbar |b|q|az\nquux| | |  \n"
    );
}

#[test]
fn files_mode_cjk_aligns_separators() {
    let a = temp_file("a-cjk.txt", "中文abc\n");
    let b = temp_file("b-cjk.txt", "中日abc\n");
    let out = run_vdiff(&[a.to_str().unwrap(), b.to_str().unwrap()]);
    let _ = std::fs::remove_file(&a);
    let _ = std::fs::remove_file(&b);
    assert!(out.status.success());
    // 文→日 cells are 2 display cells wide; the `|`s line up at
    // display 2, 5, 8 on both the marker row and the content row.
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        "  |- |+ |\n中|文|日|abc\n"
    );
}

#[test]
fn files_mode_cjk_identical_is_verbatim() {
    // Identical mixed-width files must print their content unchanged:
    // no boundaries means no separators means no alignment padding.
    let a = temp_file("a-cjk-identical.txt", "中中\n中x\n");
    let b = temp_file("b-cjk-identical.txt", "中中\n中x\n");
    let out = run_vdiff(&[a.to_str().unwrap(), b.to_str().unwrap()]);
    let _ = std::fs::remove_file(&a);
    let _ = std::fs::remove_file(&b);
    assert!(out.status.success());
    assert_eq!(String::from_utf8(out.stdout).unwrap(), "中中\n中x\n");
}

#[test]
fn files_mode_identical_files() {
    let a = temp_file("a2.txt", "x\ny\n");
    let b = temp_file("b2.txt", "x\ny\n");
    let out = run_vdiff(&[a.to_str().unwrap(), b.to_str().unwrap()]);
    let _ = std::fs::remove_file(&a);
    let _ = std::fs::remove_file(&b);
    assert!(out.status.success());
    assert_eq!(String::from_utf8(out.stdout).unwrap(), "x\ny\n");
}

#[test]
fn files_mode_missing_file_fails() {
    let out = run_vdiff(&["/nonexistent/vdiff-a", "/nonexistent/vdiff-b"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).starts_with("vdiff: "));
}

#[test]
fn bare_vdiff_shows_usage_hint() {
    // Piped stdout → run_plain → files mode with empty paths: a usage
    // hint instead of the cryptic "No such file or directory".
    let out = Command::new(env!("CARGO_BIN_EXE_vdiff")).output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("usage: vdiff <FILE1> <FILE2>"));

    // One missing file gets the same hint.
    let out = Command::new(env!("CARGO_BIN_EXE_vdiff"))
        .arg("a.txt")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("usage: vdiff <FILE1> <FILE2>"));
}

#[test]
fn git_mode_clean_worktree_is_silent() {
    let (repo, _) = make_repo();
    let out = Command::new(env!("CARGO_BIN_EXE_vdiff"))
        .arg("git")
        .arg("HEAD")
        .current_dir(repo.path())
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(out.stdout.is_empty());
}

#[test]
fn git_mode_uncommitted_changes() {
    let (repo, _) = make_repo();
    std::fs::write(repo.path().join("a.txt"), "foo\nbar qaz\n").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_vdiff"))
        .arg("git")
        .current_dir(repo.path())
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.starts_with("=== a.txt ===\n"));
    assert!(stdout.contains("-|+|\n"));
}

#[test]
fn git_mode_cached_shows_staged_only() {
    let (repo, _) = make_repo();
    std::fs::write(repo.path().join("a.txt"), "foo\nbar qaz\n").unwrap();
    git(repo.path(), &["add", "a.txt"]);
    // unstaged: nothing left between worktree and index
    let unstaged = Command::new(env!("CARGO_BIN_EXE_vdiff"))
        .arg("git")
        .current_dir(repo.path())
        .output()
        .unwrap();
    assert!(unstaged.stdout.is_empty());
    // staged: index vs HEAD
    let staged = Command::new(env!("CARGO_BIN_EXE_vdiff"))
        .arg("git")
        .arg("--cached")
        .current_dir(repo.path())
        .output()
        .unwrap();
    assert!(staged.status.success());
    let stdout = String::from_utf8(staged.stdout).unwrap();
    assert!(stdout.starts_with("=== a.txt ===\n"));
    assert!(stdout.contains("-|+|\n"));
}

#[test]
fn git_mode_three_dot_range() {
    // Baseline must have the quux line in BOTH commits: a 2-line -> 3-line
    // diff aligns the whole second line (marker "-----|+++++|"), whereas
    // this test pins the single-column baz -> qaz diff ("    |-|+|\n",
    // same marker as files_mode_prints_expected_format).
    let (repo, _) = make_repo();
    std::fs::write(repo.path().join("a.txt"), "foo\nbar baz\nquux\n").unwrap();
    git(repo.path(), &["add", "a.txt"]);
    git(
        repo.path(),
        &["-c", "commit.gpgsign=false", "commit", "-q", "-m", "two"],
    );
    let first = rev_parse(repo.path(), "HEAD");
    std::fs::write(repo.path().join("a.txt"), "foo\nbar qaz\nquux\n").unwrap();
    git(repo.path(), &["add", "a.txt"]);
    git(
        repo.path(),
        &["-c", "commit.gpgsign=false", "commit", "-q", "-m", "three"],
    );
    let second = rev_parse(repo.path(), "HEAD");
    let out = Command::new(env!("CARGO_BIN_EXE_vdiff"))
        .arg("git")
        .arg(format!("{first}...{second}"))
        .current_dir(repo.path())
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.starts_with("=== a.txt ===\n"));
    assert!(stdout.contains("    |-|+|\n"));
}

#[test]
fn git_mode_outside_repo_fails() {
    let dir = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_vdiff"))
        .arg("git")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not a git repository"));
}
