//! Column-based (vertical) diff engine.
//!
//! The diff units are *columns*: the k-th character of every line of a
//! file, padded with spaces, emitted as a structured `DiffGrid`.

use std::ops::Range;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepKind {
    Match,
    Delete,
    Insert,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub kind: StepKind,
    pub content: Vec<char>,
}

/// One diff step per file column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffGrid {
    pub steps: Vec<Step>,
    /// Max line count of the two files; every step's content is padded to this.
    pub height: usize,
    /// Runs of consecutive steps with the same kind. Used for the `|`
    /// separators, marker display, and n/p navigation.
    pub groups: Vec<Range<usize>>,
    /// True when the LCS was skipped (pathological widths); the middle
    /// columns are then all Delete+Insert.
    pub degraded: bool,
}

/// Split into lines, keeping the meaning of a trailing empty line
/// (`"a\nb\n\n"` ends with a real empty line). An empty input yields
/// no lines at all.
fn lines_with_trailing(input: &str) -> Vec<String> {
    if input.is_empty() {
        return Vec::new();
    }
    let body = input.strip_suffix('\n').unwrap_or(input);
    body.split('\n').map(str::to_string).collect()
}

/// One column of a file: the k-th char of every line, missing chars
/// padded with spaces. Every column is padded to `height`.
fn columns_padded(lines: &[String], height: usize) -> Vec<String> {
    let chars: Vec<Vec<char>> = lines.iter().map(|line| line.chars().collect()).collect();
    let width = chars.iter().map(|line| line.len()).max().unwrap_or(0);
    (0..width)
        .map(|col| {
            let mut column = String::new();
            for line in &chars {
                column.push(line.get(col).copied().unwrap_or(' '));
            }
            for _ in column.chars().count()..height {
                column.push(' ');
            }
            column
        })
        .collect()
}

/// Line-based diff of two column lists. Trims the common prefix and
/// suffix first so the LCS table stays small. When the two middle
/// widths exceed the threshold the LCS is skipped entirely (degraded
/// mode) and the middle is one big Delete+Insert.
fn diff_columns(a: &[String], b: &[String]) -> (Vec<Step>, bool) {
    let prefix = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    let suffix = a[prefix..]
        .iter()
        .rev()
        .zip(b[prefix..].iter().rev())
        .take_while(|(x, y)| x == y)
        .count();

    let mut ops: Vec<Step> = a[..prefix]
        .iter()
        .map(|c| Step {
            kind: StepKind::Match,
            content: c.chars().collect(),
        })
        .collect();
    let mid_a = &a[prefix..a.len() - suffix];
    let mid_b = &b[prefix..b.len() - suffix];
    let degraded = mid_a.len() * mid_b.len() > 10_000_000;
    if degraded {
        ops.extend(mid_a.iter().map(|c| Step {
            kind: StepKind::Delete,
            content: c.chars().collect(),
        }));
        ops.extend(mid_b.iter().map(|c| Step {
            kind: StepKind::Insert,
            content: c.chars().collect(),
        }));
    } else {
        lcs_ops(mid_a, mid_b, &mut ops);
    }
    ops.extend(a[a.len() - suffix..].iter().map(|c| Step {
        kind: StepKind::Match,
        content: c.chars().collect(),
    }));
    (ops, degraded)
}

/// Classic LCS dynamic programming over the (trimmed) column lists,
/// emitting the edit operations in order.
fn lcs_ops(a: &[String], b: &[String], out: &mut Vec<Step>) {
    let (n, m) = (a.len(), b.len());
    if n == 0 {
        out.extend(b.iter().map(|c| Step {
            kind: StepKind::Insert,
            content: c.chars().collect(),
        }));
        return;
    }
    if m == 0 {
        out.extend(a.iter().map(|c| Step {
            kind: StepKind::Delete,
            content: c.chars().collect(),
        }));
        return;
    }

    let mut table = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            table[i][j] = if a[i] == b[j] {
                table[i + 1][j + 1] + 1
            } else {
                table[i + 1][j].max(table[i][j + 1])
            };
        }
    }

    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if a[i] == b[j] {
            out.push(Step {
                kind: StepKind::Match,
                content: a[i].chars().collect(),
            });
            i += 1;
            j += 1;
        } else if table[i + 1][j] >= table[i][j + 1] {
            out.push(Step {
                kind: StepKind::Delete,
                content: a[i].chars().collect(),
            });
            i += 1;
        } else {
            out.push(Step {
                kind: StepKind::Insert,
                content: b[j].chars().collect(),
            });
            j += 1;
        }
    }
    out.extend(a[i..].iter().map(|c| Step {
        kind: StepKind::Delete,
        content: c.chars().collect(),
    }));
    out.extend(b[j..].iter().map(|c| Step {
        kind: StepKind::Insert,
        content: c.chars().collect(),
    }));
}

/// Runs of consecutive steps with the same kind.
fn groups_of(ops: &[Step]) -> Vec<Range<usize>> {
    let mut groups = Vec::new();
    let mut start = 0;
    for i in 1..=ops.len() {
        if i == ops.len() || ops[i].kind != ops[start].kind {
            groups.push(start..i);
            start = i;
        }
    }
    groups
}

/// Compute the column diff of two file contents (CRLF stripped).
pub fn compute(a: &str, b: &str) -> DiffGrid {
    let a_lines: Vec<String> = lines_with_trailing(a)
        .into_iter()
        .map(|line| line.trim_end_matches('\r').to_string())
        .collect();
    let b_lines: Vec<String> = lines_with_trailing(b)
        .into_iter()
        .map(|line| line.trim_end_matches('\r').to_string())
        .collect();
    let height = a_lines.len().max(b_lines.len());
    let a_cols = columns_padded(&a_lines, height);
    let b_cols = columns_padded(&b_lines, height);
    let (steps, degraded) = diff_columns(&a_cols, &b_cols);
    let groups = groups_of(&steps);
    DiffGrid {
        steps,
        height,
        groups,
        degraded,
    }
}

fn op_prefix(kind: StepKind) -> char {
    match kind {
        StepKind::Match => ' ',
        StepKind::Delete => '-',
        StepKind::Insert => '+',
    }
}

fn transpose_grid(grid: &[String]) -> Vec<String> {
    let chars: Vec<Vec<char>> = grid.iter().map(|line| line.chars().collect()).collect();
    let width = chars.iter().map(|line| line.len()).max().unwrap_or(0);
    (0..width)
        .map(|row| {
            let mut line = String::new();
            for grid_line in &chars {
                line.push(grid_line.get(row).copied().unwrap_or(' '));
            }
            line
        })
        .collect()
}

/// Insert `|` after each step marked in `boundaries`. Each final row
/// holds exactly one char per diff step, so steps map 1:1 to chars.
fn separate_ops(row: &str, boundaries: &[bool]) -> String {
    let mut out = String::with_capacity(row.len() * 2);
    let len = row.chars().count();
    for (i, c) in row.chars().enumerate() {
        out.push(c);
        // No trailing separator: a trimmed marker row may end before
        // the last step.
        if i + 1 < len && boundaries.get(i).copied().unwrap_or(false) {
            out.push('|');
        }
    }
    out
}

/// The plain-text view: marker row (trailing match markers
/// trimmed, trailing `|` kept) followed by the file lines with `|`
/// separators between groups. Empty when the grid has no steps.
pub fn render_text(grid: &DiffGrid) -> String {
    let lines: Vec<String> = grid
        .steps
        .iter()
        .map(|s| {
            let mut line = String::with_capacity(grid.height + 1);
            line.push(op_prefix(s.kind));
            line.extend(s.content.iter());
            // content is a Vec<char>, so len() is already the char count.
            for _ in s.content.len()..grid.height {
                line.push(' ');
            }
            line
        })
        .collect();
    let transposed = transpose_grid(&lines);
    let boundaries: Vec<bool> = grid
        .steps
        .windows(2)
        .map(|w| w[0].kind != w[1].kind)
        .collect();

    let mut rows = Vec::with_capacity(transposed.len());
    for (i, row) in transposed.into_iter().enumerate() {
        if i == 0 {
            let trimmed = row.trim_end_matches(' ');
            let mut marker = separate_ops(trimmed, &boundaries);
            // The marker drops trailing match markers (spaces); keep
            // the `|` that separates the last change from the tail.
            if !marker.is_empty() && trimmed.len() < row.len() {
                marker.push('|');
            }
            rows.push(marker);
        } else {
            rows.push(separate_ops(&row, &boundaries));
        }
    }

    let mut out = String::new();
    if let Some((first, rest)) = rows.split_first() {
        if !first.is_empty() {
            out.push_str(first);
            out.push('\n');
        }
        out.push_str(&rest.join("\n"));
    }
    if !out.is_empty() {
        out.push('\n');
    }
    out
}

/// The raw step view: one row per diff step — prefix char (` `/`-`/`+`)
/// + `' '` + the column content (one char per file line).
pub fn render_transposed(grid: &DiffGrid) -> String {
    let mut out = String::new();
    for step in &grid.steps {
        out.push(op_prefix(step.kind));
        out.push(' ');
        out.extend(step.content.iter());
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Flatten steps into (prefix, content) pairs for readable assertions.
    fn ops_of(grid: &DiffGrid) -> Vec<(&'static str, String)> {
        grid.steps
            .iter()
            .map(|s| {
                let p = match s.kind {
                    StepKind::Match => "=",
                    StepKind::Delete => "-",
                    StepKind::Insert => "+",
                };
                (p, s.content.iter().collect())
            })
            .collect()
    }

    /// Convert expected (prefix, content) literals to ops_of's output
    /// type. Needed because std no longer implements `PartialEq`
    /// between tuples of different element types (removed in 1.90), so
    /// `Vec<(&str, String)>` can't be compared against `Vec<(&str, &str)>`.
    fn exp<'a>(pairs: &[(&'a str, &'a str)]) -> Vec<(&'a str, String)> {
        pairs.iter().map(|(p, c)| (*p, c.to_string())).collect()
    }

    #[test]
    fn lines_keep_trailing_empty_line() {
        assert_eq!(lines_with_trailing("a\nb\n"), vec!["a", "b"]);
        assert_eq!(lines_with_trailing("a\nb\n\n"), vec!["a", "b", ""]);
        assert_eq!(lines_with_trailing(""), Vec::<String>::new());
    }

    #[test]
    fn diff_columns_all_equal() {
        let cols = vec!["ab".to_string(), "cd".to_string()];
        let (steps, _) = diff_columns(&cols, &cols);
        let groups = groups_of(&steps);
        assert_eq!(
            ops_of(&DiffGrid {
                steps,
                height: 2,
                groups,
                degraded: false
            }),
            exp(&[("=", "ab"), ("=", "cd")])
        );
    }

    #[test]
    fn diff_columns_replaces_one() {
        let a = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let b = vec!["a".to_string(), "x".to_string(), "c".to_string()];
        let (steps, _) = diff_columns(&a, &b);
        let groups = groups_of(&steps);
        assert_eq!(
            ops_of(&DiffGrid {
                steps,
                height: 1,
                groups,
                degraded: false
            }),
            exp(&[("=", "a"), ("-", "b"), ("+", "x"), ("=", "c")])
        );
    }

    #[test]
    fn diff_columns_inserts_and_deletes() {
        let a = vec!["a".to_string(), "c".to_string()];
        let b = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let (steps, _) = diff_columns(&a, &b);
        let groups = groups_of(&steps);
        assert_eq!(
            ops_of(&DiffGrid {
                steps,
                height: 1,
                groups,
                degraded: false
            }),
            exp(&[("=", "a"), ("+", "b"), ("=", "c")])
        );
        let a = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let b = vec!["a".to_string(), "c".to_string()];
        let (steps, _) = diff_columns(&a, &b);
        let groups = groups_of(&steps);
        assert_eq!(
            ops_of(&DiffGrid {
                steps,
                height: 1,
                groups,
                degraded: false
            }),
            exp(&[("=", "a"), ("-", "b"), ("=", "c")])
        );
    }

    #[test]
    fn diff_columns_empty_sides() {
        assert!(diff_columns(&[], &[]).0.is_empty());
        let (steps, _) = diff_columns(&[], &["x".to_string()]);
        assert_eq!(
            ops_of(&DiffGrid {
                steps,
                height: 1,
                groups: vec![Range { start: 0, end: 1 }],
                degraded: false
            }),
            exp(&[("+", "x")])
        );
        let (steps, _) = diff_columns(&["x".to_string()], &[]);
        assert_eq!(
            ops_of(&DiffGrid {
                steps,
                height: 1,
                groups: vec![Range { start: 0, end: 1 }],
                degraded: false
            }),
            exp(&[("-", "x")])
        );
    }

    #[test]
    fn groups_merge_consecutive_same_kind_steps() {
        let grid = compute("foo\nbar baz\nquux\n", "foo\nbar qaz\nquux\n");
        assert_eq!(grid.groups, vec![0..4, 4..5, 5..6, 6..8]);
        assert!(!grid.degraded);
    }

    #[test]
    fn compute_marks_column_change() {
        let grid = compute("foo\nbar baz\nquux\n", "foo\nbar qaz\nquux\n");
        // 7 columns (max line width); column 4 differs: old " b " vs new " q "
        assert_eq!(
            ops_of(&grid),
            exp(&[
                ("=", "fbq"),
                ("=", "oau"),
                ("=", "oru"),
                ("=", "  x"),
                ("-", " b "),
                ("+", " q "),
                ("=", " a "),
                ("=", " z "),
            ])
        );
        assert_eq!(grid.height, 3);
    }

    #[test]
    fn compute_empty_vs_content() {
        let grid = compute("", "foo\nbar\n");
        assert_eq!(ops_of(&grid), exp(&[("+", "fb"), ("+", "oa"), ("+", "or")]));
    }

    #[test]
    fn compute_empty_vs_empty() {
        let grid = compute("", "");
        assert!(grid.steps.is_empty());
        assert_eq!(grid.height, 0);
    }

    #[test]
    fn compute_strips_crlf() {
        let grid = compute("a\r\nb\r\n", "a\r\nc\r\n");
        assert_eq!(ops_of(&grid), exp(&[("-", "ab"), ("+", "ac")]));
    }

    #[test]
    fn compute_utf8_multibyte() {
        let grid = compute("héllo\n", "héxlo\n");
        // one column per char position; column 2 differs (l vs x)
        assert_eq!(
            ops_of(&grid),
            exp(&[
                ("=", "h"),
                ("=", "é"),
                ("-", "l"),
                ("+", "x"),
                ("=", "l"),
                ("=", "o")
            ])
        );
    }

    #[test]
    fn compute_pads_multibyte_columns_by_chars() {
        // "é" is 2 bytes but 1 char — the pad loop must count chars,
        // or "é" vs "é " compares unequal and yields a spurious
        // Delete+Insert (the ASCII analog "a\n" vs "a\n \n" is a Match).
        let grid = compute("é\n", "é\n \n");
        assert_eq!(ops_of(&grid), exp(&[("=", "é ")]));
    }

    #[test]
    fn degraded_skips_lcs_for_pathological_widths() {
        // diff_columns works on columns, not lines: a 5000-char single
        // line becomes 5000 one-char columns (see columns_padded).
        let a = vec!["a".to_string(); 5_000];
        let b = vec!["b".to_string(); 5_000];
        // 5000*5000 > 10_000_000 → degraded path: LCS skipped, so each
        // middle column is one Delete (from a) followed by one Insert (from b).
        let (steps, degraded) = diff_columns(&a, &b);
        assert!(degraded);
        assert_eq!(steps.len(), 10_000);
        assert_eq!(steps[0].kind, StepKind::Delete);
        assert_eq!(steps[4_999].kind, StepKind::Delete);
        assert_eq!(steps[5_000].kind, StepKind::Insert);
        assert_eq!(steps[9_999].kind, StepKind::Insert);
    }

    #[test]
    fn render_text_marks_column_change() {
        assert_eq!(
            render_text(&compute("foo\nbar baz\nquux\n", "foo\nbar qaz\nquux\n")),
            "    |-|+|\nfoo | | |  \nbar |b|q|az\nquux| | |  \n",
        );
    }

    #[test]
    fn render_text_identical_files_have_no_marker() {
        assert_eq!(
            render_text(&compute("foo\nbar baz\n", "foo\nbar baz\n")),
            "foo    \nbar baz\n",
        );
    }

    #[test]
    fn render_text_empty_vs_content() {
        assert_eq!(render_text(&compute("", "foo\nbar\n")), "+++\nfoo\nbar\n");
    }

    #[test]
    fn render_text_empty_vs_empty() {
        assert_eq!(render_text(&compute("", "")), "");
    }

    #[test]
    fn render_text_strips_crlf() {
        assert_eq!(
            render_text(&compute("a\r\nb\r\n", "a\r\nc\r\n")),
            "-|+\na|a\nb|c\n"
        );
    }

    #[test]
    fn render_text_utf8() {
        assert_eq!(
            render_text(&compute("héllo\n", "héxlo\n")),
            "  |-|+|\nhé|l|x|lo\n"
        );
    }

    #[test]
    fn render_text_keeps_trailing_empty_line() {
        assert_eq!(
            render_text(&compute("ab\ncd\n", "ab\ncd\n\n")),
            "ab\ncd\n  \n"
        );
    }

    #[test]
    fn render_text_all_insert() {
        let grid = compute("", "foo\nbar\nquux\n");
        // "quux" is 4 chars wide, so there are 4 insert columns ("++++").
        assert_eq!(render_text(&grid), "++++\nfoo \nbar \nquux\n");
    }

    #[test]
    fn render_transposed_stacks_steps() {
        let grid = compute("foo\nbar baz\nquux\n", "foo\nbar qaz\nquux\n");
        assert_eq!(
            render_transposed(&grid),
            "  fbq\n  oau\n  oru\n    x\n-  b \n+  q \n   a \n   z \n",
        );
    }

    #[test]
    fn render_transposed_empty() {
        assert_eq!(render_transposed(&compute("", "")), "");
    }
}
