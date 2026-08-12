//! Column-based (vertical) diff engine.
//!
//! The diff units are *columns*: the k-th character of every line of a
//! file, padded with spaces, emitted as a structured `DiffGrid`.

use std::ops::Range;
use unicode_width::UnicodeWidthChar;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepKind {
    Match,
    Delete,
    Insert,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub kind: StepKind,
    /// One char per file line, padded with spaces to `grid.height`.
    /// A String (not Vec<char>) halves the allocations for wide files.
    pub content: String,
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
    /// Display width of each step's column (see `step_width`),
    /// precomputed once. Every rendered row pads its cells to this.
    pub widths: Vec<usize>,
    /// `is_group_end[i]` is true when step `i` is the last step of a
    /// group. Precomputed from `groups` so renderers test a group
    /// boundary in O(1) instead of scanning every group per step.
    pub is_group_end: Vec<bool>,
}

impl DiffGrid {
    /// Build a grid from steps, deriving the group runs, per-step
    /// column widths, and group-end flags (all pure functions of the
    /// steps) in one pass.
    pub fn from_parts(steps: Vec<Step>, height: usize, degraded: bool) -> DiffGrid {
        let groups = groups_of(&steps);
        let widths: Vec<usize> = steps.iter().map(step_width).collect();
        let mut is_group_end = vec![false; steps.len()];
        for g in &groups {
            is_group_end[g.end - 1] = true;
        }
        DiffGrid {
            steps,
            height,
            groups,
            degraded,
            widths,
            is_group_end,
        }
    }
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

/// Tab stops every `TAB_STOP` display columns; a tab expands to the
/// spaces up to the next stop. Terminals advance a tab to its own tab
/// stop, so a fixed one-cell width would misalign every following
/// column.
const TAB_STOP: usize = 8;

/// Replace tabs with spaces up to the next `TAB_STOP` column, using
/// display widths so a wide char before the tab lands on the same
/// stop as in a terminal. The column count resets at newlines.
fn expand_tabs(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut col = 0usize;
    for c in input.chars() {
        if c == '\t' {
            let next = (col / TAB_STOP + 1) * TAB_STOP;
            for _ in col..next {
                out.push(' ');
            }
            col = next;
        } else if c == '\n' {
            out.push(c);
            col = 0;
        } else {
            out.push(c);
            col += c.width().unwrap_or(1);
        }
    }
    out
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
            content: c.clone(),
        })
        .collect();
    let mid_a = &a[prefix..a.len() - suffix];
    let mid_b = &b[prefix..b.len() - suffix];
    // saturating: on 32-bit builds a huge product would wrap and skip
    // the guard. 1M cells is a ~8 MB flat table, a fine cliff.
    let degraded = mid_a.len().saturating_mul(mid_b.len()) > 1_000_000;
    if degraded {
        ops.extend(mid_a.iter().map(|c| Step {
            kind: StepKind::Delete,
            content: c.clone(),
        }));
        ops.extend(mid_b.iter().map(|c| Step {
            kind: StepKind::Insert,
            content: c.clone(),
        }));
    } else {
        lcs_ops(mid_a, mid_b, &mut ops);
    }
    ops.extend(a[a.len() - suffix..].iter().map(|c| Step {
        kind: StepKind::Match,
        content: c.clone(),
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
            content: c.clone(),
        }));
        return;
    }
    if m == 0 {
        out.extend(a.iter().map(|c| Step {
            kind: StepKind::Delete,
            content: c.clone(),
        }));
        return;
    }

    // One flat allocation instead of n+1 vectors of m+1 entries, so a
    // table near the threshold costs a single ~8 MB block.
    let stride = m + 1;
    let mut table = vec![0usize; (n + 1) * stride];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            table[i * stride + j] = if a[i] == b[j] {
                table[(i + 1) * stride + j + 1] + 1
            } else {
                table[(i + 1) * stride + j].max(table[i * stride + j + 1])
            };
        }
    }

    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if a[i] == b[j] {
            out.push(Step {
                kind: StepKind::Match,
                content: a[i].clone(),
            });
            i += 1;
            j += 1;
        } else if table[(i + 1) * stride + j] >= table[i * stride + j + 1] {
            out.push(Step {
                kind: StepKind::Delete,
                content: a[i].clone(),
            });
            i += 1;
        } else {
            out.push(Step {
                kind: StepKind::Insert,
                content: b[j].clone(),
            });
            j += 1;
        }
    }
    out.extend(a[i..].iter().map(|c| Step {
        kind: StepKind::Delete,
        content: c.clone(),
    }));
    out.extend(b[j..].iter().map(|c| Step {
        kind: StepKind::Insert,
        content: c.clone(),
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

/// Compute the column diff of two file contents (CRLF stripped, tabs
/// expanded).
pub fn compute(a: &str, b: &str) -> DiffGrid {
    let a_lines: Vec<String> = lines_with_trailing(&expand_tabs(a))
        .into_iter()
        .map(|line| line.trim_end_matches('\r').to_string())
        .collect();
    let b_lines: Vec<String> = lines_with_trailing(&expand_tabs(b))
        .into_iter()
        .map(|line| line.trim_end_matches('\r').to_string())
        .collect();
    let height = a_lines.len().max(b_lines.len());
    let a_cols = columns_padded(&a_lines, height);
    // Identical inputs are the common case; build the all-Match grid
    // straight from the columns instead of diffing (and keep the
    // memory down for very wide files).
    if a == b {
        let steps = a_cols
            .iter()
            .map(|c| Step {
                kind: StepKind::Match,
                content: c.clone(),
            })
            .collect();
        return DiffGrid::from_parts(steps, height, false);
    }
    let b_cols = columns_padded(&b_lines, height);
    let (steps, degraded) = diff_columns(&a_cols, &b_cols);
    DiffGrid::from_parts(steps, height, degraded)
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

/// Display width of one step's column: the widest char in the column.
/// Every rendered row (marker and content) pads its cells to this so
/// the `|` separators line up. The marker row renders one char per
/// step, so a column counts as cells, not chars; `max(1)` keeps
/// all-zero-width columns (bare combining marks) at one cell instead
/// of zero.
pub(crate) fn step_width(step: &Step) -> usize {
    step.content
        .chars()
        .map(|c| c.width().unwrap_or(0))
        .max()
        .unwrap_or(0)
        .max(1)
}

/// Pad count for one cell: `col_w` cells minus the char's own width.
/// Zero-width chars (ZWJ, combining marks) get no pad: a space after
/// them would break the grapheme cluster they belong to (the pad
/// lands after the char, keeping the cell left-aligned).
pub(crate) fn cell_pad(c: char, col_w: usize) -> usize {
    match c.width() {
        Some(0) => 0,
        _ => col_w.saturating_sub(c.width().unwrap_or(1)),
    }
}

/// Insert `|` after each step marked in `boundaries`, padding every
/// cell to its step column's display width (`widths`), so each column
/// occupies the same cells in every row and separators line up
/// across rows. Each final row holds exactly one char per diff step
/// (plus pad spaces), so steps map 1:1 to chars.
fn separate_ops(row: &str, boundaries: &[bool], widths: &[usize]) -> String {
    let mut out = String::with_capacity(row.len() * 2 + widths.len());
    let len = row.chars().count();
    for (i, c) in row.chars().enumerate() {
        out.push(c);
        let cell = widths.get(i).copied().unwrap_or(1);
        for _ in 0..cell_pad(c, cell) {
            out.push(' ');
        }
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
            line.extend(s.content.chars());
            // content is a String; chars() gives the char count.
            for _ in s.content.chars().count()..grid.height {
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
    let widths = &grid.widths;

    let mut rows = Vec::with_capacity(transposed.len());
    for (i, row) in transposed.into_iter().enumerate() {
        if i == 0 {
            let trimmed = row.trim_end_matches(' ');
            let mut marker = separate_ops(trimmed, &boundaries, widths);
            // The marker drops trailing match markers (spaces); keep
            // the `|` that separates the last change from the tail.
            if !marker.is_empty() && trimmed.len() < row.len() {
                marker.push('|');
            }
            rows.push(marker);
        } else {
            rows.push(separate_ops(&row, &boundaries, widths));
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
        out.extend(step.content.chars());
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
                (p, s.content.chars().collect())
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
    fn expand_tabs_reaches_next_tab_stop() {
        assert_eq!(expand_tabs("a\tb"), "a       b");
        assert_eq!(expand_tabs("\t"), "        ");
        // The column count resets at newlines.
        assert_eq!(expand_tabs("a\tb\nab\tc"), "a       b\nab      c");
        // Wide chars count display columns: 中 (2) then the tab lands
        // at the same stop as in a terminal.
        assert_eq!(expand_tabs("中\tx"), "中      x");
    }

    #[test]
    fn compute_expands_tabs() {
        // "a\tb" -> "a       b": only column 8 differs from "a\tc".
        let grid = compute("a\tb\n", "a\tc\n");
        let ops = ops_of(&grid);
        assert_eq!(ops.len(), 10);
        assert!(ops[..8].iter().all(|(p, _)| *p == "="));
        assert_eq!(ops[8], ("-", "b".to_string()));
        assert_eq!(ops[9], ("+", "c".to_string()));
    }

    #[test]
    fn render_text_tabs_align_separators() {
        let rendered = render_text(&compute("a\tb\n", "a\tc\n"));
        let mut lines = rendered.lines();
        let marker = lines.next().unwrap();
        let content = lines.next().unwrap();
        assert_eq!(
            marker.chars().position(|c| c == '|'),
            content.chars().position(|c| c == '|'),
        );
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
        assert_eq!(
            ops_of(&DiffGrid::from_parts(steps, 2, false)),
            exp(&[("=", "ab"), ("=", "cd")])
        );
    }

    #[test]
    fn diff_columns_replaces_one() {
        let a = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let b = vec!["a".to_string(), "x".to_string(), "c".to_string()];
        let (steps, _) = diff_columns(&a, &b);
        assert_eq!(
            ops_of(&DiffGrid::from_parts(steps, 1, false)),
            exp(&[("=", "a"), ("-", "b"), ("+", "x"), ("=", "c")])
        );
    }

    #[test]
    fn diff_columns_inserts_and_deletes() {
        let a = vec!["a".to_string(), "c".to_string()];
        let b = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let (steps, _) = diff_columns(&a, &b);
        assert_eq!(
            ops_of(&DiffGrid::from_parts(steps, 1, false)),
            exp(&[("=", "a"), ("+", "b"), ("=", "c")])
        );
        let a = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let b = vec!["a".to_string(), "c".to_string()];
        let (steps, _) = diff_columns(&a, &b);
        assert_eq!(
            ops_of(&DiffGrid::from_parts(steps, 1, false)),
            exp(&[("=", "a"), ("-", "b"), ("=", "c")])
        );
    }

    #[test]
    fn diff_columns_empty_sides() {
        assert!(diff_columns(&[], &[]).0.is_empty());
        let (steps, _) = diff_columns(&[], &["x".to_string()]);
        assert_eq!(
            ops_of(&DiffGrid::from_parts(steps, 1, false)),
            exp(&[("+", "x")])
        );
        let (steps, _) = diff_columns(&["x".to_string()], &[]);
        assert_eq!(
            ops_of(&DiffGrid::from_parts(steps, 1, false)),
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
    fn compute_identical_inputs_are_all_match() {
        let grid = compute("foo\nbar\n", "foo\nbar\n");
        assert!(grid.steps.iter().all(|s| s.kind == StepKind::Match));
        assert!(!grid.degraded);
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
    fn step_width_measures_wide_chars() {
        assert_eq!(
            step_width(&Step {
                kind: StepKind::Match,
                content: "a中".to_string()
            }),
            2
        );
        // All-zero-width column (bare combining mark) floors at 1.
        assert_eq!(
            step_width(&Step {
                kind: StepKind::Match,
                content: "\u{0301}".to_string()
            }),
            1
        );
    }

    #[test]
    fn render_text_cjk_aligns_separators() {
        // 文→日 is width 2, so the marker's `-`/`+` and the content
        // cells are 2 cells wide; the `|`s line up at display 2, 5, 8.
        assert_eq!(
            render_text(&compute("中文abc\n", "中日abc\n")),
            "  |- |+ |\n中|文|日|abc\n",
        );
    }

    #[test]
    fn render_text_cjk_pads_narrow_cells() {
        // Column 0 changes whole (中a → 中b): D+I steps carry width-2
        // cells, so the narrow `a`/`b` cells are padded to 2 cells and
        // the marker's `-`/`+` align with the content.
        assert_eq!(
            render_text(&compute("中文\na文\n", "中文\nb文\n")),
            "- |+ |\n中|中|文\na |b |文\n",
        );
    }

    #[test]
    fn render_text_identical_wide_files_align_columns() {
        // Identical mixed-width files still display the column grid:
        // the narrow `x` is padded to its column's 2-cell width so it
        // sits under `中`, and the row spans the full grid width.
        assert_eq!(
            render_text(&compute("中中\n中x\n", "中中\n中x\n")),
            "中中\n中x \n"
        );
    }

    #[test]
    fn render_text_wide_columns_align_halfwidth_chars() {
        // a.txt case: four full-width chars vs four half-width chars
        // on the same columns; each half-width char is padded to 2
        // cells so it sits at its column's start (a under 你, b under
        // 好, c under 啊, d under ！), trailing pad included.
        assert_eq!(
            render_text(&compute("你好啊！\nabcd\n", "你好啊！\nabcd\n")),
            "你好啊！\na b c d \n"
        );
    }

    #[test]
    fn render_text_keeps_zero_width_clusters_intact() {
        // A ZWJ cell is width 0; padding it would insert a space into
        // the 👨‍👩 emoji cluster and break it into separate glyphs.
        let s = "👨\u{200D}👩\n";
        assert_eq!(render_text(&compute(s, s)), s);
    }

    #[test]
    fn render_text_keeps_zwj_cluster_intact_in_wide_column() {
        // A wide char in the ZWJ's column widens it to 2 cells; the
        // ZWJ must still get no pad, or the family splits apart.
        let s = "👨\u{200D}👩\n中中\n";
        let rendered = render_text(&compute(s, s));
        assert!(
            rendered.contains("👨\u{200D}👩"),
            "cluster must stay glued: {rendered:?}"
        );
    }

    #[test]
    fn render_text_cjk_pads_trailing_cells() {
        // The last change's cell is padded too, so the marker ends at
        // the grid width. `|`s at display 2 and 5; the padded `+` sits
        // over 日's cells 6-7, matching the content row's width.
        assert_eq!(
            render_text(&compute("中文\n", "中日\n")),
            "  |- |+ \n中|文|日\n"
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
