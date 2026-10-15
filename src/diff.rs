//! Column-based (vertical) diff engine.
//!
//! The diff units are *columns*: the k-th grapheme of every line of a
//! file, padded with spaces, emitted as a structured `DiffGrid`.

use std::collections::HashSet;
use std::ops::Range;
use std::sync::Arc;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepKind {
    Match,
    Delete,
    Insert,
}

/// A grapheme column with implicit padding spaces and shared packed storage.
/// Cloning a column shares its backing data rather than copying its cells.
#[derive(Debug, Clone)]
pub struct Column {
    height: usize,
    data: Arc<ColumnData>,
}

#[derive(Debug, Default, PartialEq, Eq, Hash)]
struct ColumnData {
    text: String,
    // Primitive slices support bulk equality instead of per-record scans.
    rows: Vec<usize>,
    ends: Vec<usize>,
}

impl ColumnData {
    fn push(&mut self, row: usize, cell: &str) {
        if cell == " " {
            return;
        }
        debug_assert!(self.rows.last().is_none_or(|last| *last < row));
        self.text.push_str(cell);
        self.rows.push(row);
        self.ends.push(self.text.len());
    }

    fn cell(&self, index: usize) -> &str {
        let start = if index == 0 { 0 } else { self.ends[index - 1] };
        &self.text[start..self.ends[index]]
    }
}

impl PartialEq for Column {
    fn eq(&self, other: &Self) -> bool {
        self.height == other.height
            && (Arc::ptr_eq(&self.data, &other.data) || self.data == other.data)
    }
}

impl Eq for Column {}

/// Equal packed columns share one backing object before LCS. HashSet still
/// checks exact text and row boundaries, so hash collisions cannot match edits.
fn intern_columns(columns: &mut [Column], pool: &mut HashSet<Arc<ColumnData>>) {
    for column in columns {
        if let Some(existing) = pool.get(&column.data) {
            column.data = Arc::clone(existing);
        } else {
            pool.insert(Arc::clone(&column.data));
        }
    }
}

impl Column {
    /// Number of file rows, including implicit padding cells.
    pub fn len(&self) -> usize {
        self.height
    }

    pub fn is_empty(&self) -> bool {
        self.height == 0
    }

    /// Read a complete grapheme, or a shared space for an omitted padding cell.
    pub fn get(&self, row: usize) -> Option<&str> {
        if row >= self.height {
            return None;
        }
        Some(match self.data.rows.binary_search(&row) {
            Ok(index) => self.data.cell(index),
            Err(_) => " ",
        })
    }

    /// Iterate all logical cells without allocating padding strings.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = &str> {
        let mut index = 0;
        (0..self.height).map(move |row| {
            while index < self.data.rows.len() && self.data.rows[index] < row {
                index += 1;
            }
            if index < self.data.rows.len() && self.data.rows[index] == row {
                let cell = self.data.cell(index);
                index += 1;
                cell
            } else {
                " "
            }
        })
    }
}

impl From<Vec<String>> for Column {
    fn from(cells: Vec<String>) -> Self {
        let height = cells.len();
        let mut data = ColumnData::default();
        for (row, cell) in cells.into_iter().enumerate() {
            data.push(row, &cell);
        }
        Self {
            height,
            data: Arc::new(data),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub kind: StepKind,
    /// One complete grapheme per file line, padded with spaces to `grid.height`.
    pub content: Column,
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
    for grapheme in input.graphemes(true) {
        if grapheme == "\t" {
            let next = (col / TAB_STOP + 1) * TAB_STOP;
            for _ in col..next {
                out.push(' ');
            }
            col = next;
        } else if grapheme.ends_with('\n') {
            out.push_str(grapheme);
            col = 0;
        } else {
            out.push_str(grapheme);
            col += UnicodeWidthStr::width(grapheme);
        }
    }
    out
}

/// One column of a file: the k-th grapheme of every line, missing cells
/// padded with spaces. Every column is padded to `height`.
fn columns_padded(lines: &[String], height: usize) -> Vec<Column> {
    let mut columns = Vec::<ColumnData>::new();
    for (row, line) in lines.iter().enumerate() {
        for (col, grapheme) in line.graphemes(true).enumerate() {
            if col == columns.len() {
                columns.push(ColumnData::default());
            }
            columns[col].push(row, grapheme);
        }
    }
    columns
        .into_iter()
        .map(|data| Column {
            height,
            data: Arc::new(data),
        })
        .collect()
}

/// Line-based diff of two column lists. Trims the common prefix and
/// suffix first so the LCS table stays small. When the two middle
/// widths exceed the threshold the LCS is skipped entirely (degraded
/// mode) and the middle is one big Delete+Insert.
fn diff_columns(a: &[Column], b: &[Column]) -> (Vec<Step>, bool) {
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
fn lcs_ops(a: &[Column], b: &[Column], out: &mut Vec<Step>) {
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
    let a_expanded = expand_tabs(a);
    let b_expanded = expand_tabs(b);
    let a_has_trailing = a_expanded.ends_with('\n');
    let b_has_trailing = b_expanded.ends_with('\n');
    let trailing_differs = a_has_trailing != b_has_trailing;
    let a_lines: Vec<String> = lines_with_trailing(&a_expanded)
        .into_iter()
        .map(|line| line.trim_end_matches('\r').to_string())
        .collect();
    let b_lines: Vec<String> = lines_with_trailing(&b_expanded)
        .into_iter()
        .map(|line| line.trim_end_matches('\r').to_string())
        .collect();
    // A missing trailing newline is a real difference (git reports it
    // too); give it its own row and step so it stays visible.
    let height = a_lines.len().max(b_lines.len()) + usize::from(trailing_differs);
    let mut a_cols = columns_padded(&a_lines, height);
    let mut pool = HashSet::new();
    intern_columns(&mut a_cols, &mut pool);
    // Identical inputs are the common case; build the all-Match grid
    // straight from the columns instead of diffing (and keep the
    // memory down for very wide files).
    if a == b {
        let steps = a_cols
            .into_iter()
            .map(|c| Step {
                kind: StepKind::Match,
                content: c,
            })
            .collect();
        return DiffGrid::from_parts(steps, height, false);
    }
    let mut b_cols = columns_padded(&b_lines, height);
    intern_columns(&mut b_cols, &mut pool);
    let (mut steps, degraded) = diff_columns(&a_cols, &b_cols);
    if trailing_differs {
        // A column of spaces with a ↵ in the last row: the side that
        // ends in a newline owns the mark, the kind says which one.
        let mut data = ColumnData::default();
        data.push(height - 1, "↵");
        let content = Column {
            height,
            data: Arc::new(data),
        };
        let kind = if a_has_trailing {
            StepKind::Delete
        } else {
            StepKind::Insert
        };
        steps.push(Step { kind, content });
    }
    DiffGrid::from_parts(steps, height, degraded)
}

fn op_prefix(kind: StepKind) -> char {
    match kind {
        StepKind::Match => ' ',
        StepKind::Delete => '-',
        StepKind::Insert => '+',
    }
}

/// Display width of a column: the widest complete grapheme across rows.
/// Zero-width standalone cells still reserve a marker position.
pub(crate) fn step_width(step: &Step) -> usize {
    step.content
        .data
        .rows
        .iter()
        .enumerate()
        .map(|(index, _)| UnicodeWidthStr::width(step.content.data.cell(index)))
        .max()
        .unwrap_or(0)
        .max(1)
}

/// Pad only after a complete grapheme, never between its scalars.
pub(crate) fn cell_pad(cell: &str, width: usize) -> usize {
    width.saturating_sub(UnicodeWidthStr::width(cell))
}

/// Plain-text rows built directly from complete grapheme cells.
/// Trailing match markers are omitted; content keeps the full column grid.
pub fn render_text(grid: &DiffGrid) -> String {
    if grid.steps.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    let marker_end = grid
        .steps
        .iter()
        .rposition(|step| step.kind != StepKind::Match)
        .map_or(0, |i| i + 1);
    for (i, step) in grid.steps.iter().take(marker_end).enumerate() {
        let marker = op_prefix(step.kind).to_string();
        out.push_str(&marker);
        out.extend(std::iter::repeat_n(' ', cell_pad(&marker, grid.widths[i])));
        if grid.is_group_end[i] && i + 1 < grid.steps.len() {
            out.push('|');
        }
    }
    if !out.is_empty() {
        out.push('\n');
    }
    let mut columns: Vec<_> = grid.steps.iter().map(|step| step.content.iter()).collect();
    for _ in 0..grid.height {
        for (i, column) in columns.iter_mut().enumerate() {
            let cell = column.next().unwrap_or(" ");
            out.push_str(cell);
            out.extend(std::iter::repeat_n(' ', cell_pad(cell, grid.widths[i])));
            if grid.is_group_end[i] && i + 1 < grid.steps.len() {
                out.push('|');
            }
        }
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

    fn test_columns(contents: &[&str]) -> Vec<Column> {
        contents
            .iter()
            .map(|column| {
                column
                    .graphemes(true)
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
                    .into()
            })
            .collect()
    }

    #[test]
    fn packed_columns_preserve_unicode_cells_and_implicit_padding() {
        let column = Column::from(vec![
            "a\u{301}".to_owned(),
            " ".to_owned(),
            "👩‍❤️‍👩".to_owned(),
            " ".to_owned(),
            "x".to_owned(),
        ]);
        assert_eq!(column.len(), 5);
        assert!(!column.is_empty());
        assert_eq!(column.get(0), Some("a\u{301}"));
        assert_eq!(column.get(1), Some(" "));
        assert_eq!(column.get(2), Some("👩‍❤️‍👩"));
        assert_eq!(column.get(3), Some(" "));
        assert_eq!(column.get(4), Some("x"));
        assert_eq!(column.get(5), None);
        assert_eq!(
            column.iter().collect::<Vec<_>>(),
            ["a\u{301}", " ", "👩‍❤️‍👩", " ", "x"]
        );
        assert_eq!(column.iter().nth(2), Some("👩‍❤️‍👩"));
        assert_eq!(column.iter().skip(3).collect::<Vec<_>>(), [" ", "x"]);
        assert_eq!(column.data.rows.len(), 3);
        let clone = column.clone();
        assert!(Arc::ptr_eq(&column.data, &clone.data));
        drop(column);
        assert_eq!(clone.get(2), Some("👩‍❤️‍👩"));
        let empty = Column::from(Vec::new());
        assert!(empty.is_empty());
        assert_eq!(empty.get(0), None);
    }

    #[test]
    fn packed_column_equality_keeps_row_boundaries() {
        let separate = Column::from(vec!["a".to_owned(), "\u{301}".to_owned()]);
        let combined = Column::from(vec!["a\u{301}".to_owned(), " ".to_owned()]);
        assert_eq!(separate.data.text, combined.data.text);
        assert_ne!(separate, combined);
        let padded = columns_padded(&["a\u{301}".to_owned()], 2);
        assert_eq!(padded[0], combined);
        let blank = Column::from(vec![" ".to_owned(); 10]);
        assert_eq!(blank.len(), 10);
        assert!(blank.data.rows.is_empty());
        assert!(blank.data.text.is_empty());
        assert!(blank.iter().all(|cell| cell == " "));
    }

    #[test]
    fn canonical_columns_share_backing_across_both_inputs() {
        let a = format!("x{}y\n", "a".repeat(254)).repeat(64);
        let b = format!("p{}q\n", "a".repeat(254)).repeat(64);
        let mut a_cols = columns_padded(&lines_with_trailing(&a), 64);
        let mut b_cols = columns_padded(&lines_with_trailing(&b), 64);
        let mut pool = HashSet::new();
        intern_columns(&mut a_cols, &mut pool);
        intern_columns(&mut b_cols, &mut pool);
        assert_eq!(pool.len(), 5); // x, a, y, p, q
        for col in 1..255 {
            assert!(Arc::ptr_eq(&a_cols[1].data, &a_cols[col].data));
            assert!(Arc::ptr_eq(&a_cols[1].data, &b_cols[col].data));
        }
        let grid = compute(&a, &b);
        assert!(!grid.degraded);
        let matches: Vec<_> = grid
            .steps
            .iter()
            .filter(|step| step.kind == StepKind::Match)
            .collect();
        assert_eq!(matches.len(), 254);
        for matched in &matches {
            assert!(Arc::ptr_eq(&matches[0].content.data, &matched.content.data));
        }
        assert_eq!(grid.steps.len(), 258);
    }

    #[test]
    fn canonicalization_keeps_row_boundaries_and_logical_height_distinct() {
        let mut columns = [
            Column::from(vec!["a".to_owned(), "\u{301}".to_owned()]),
            Column::from(vec!["a\u{301}".to_owned(), " ".to_owned()]),
            Column::from(vec![" ".to_owned(); 2]),
            Column::from(vec![" ".to_owned(); 1]),
        ];
        let mut pool = HashSet::new();
        intern_columns(&mut columns, &mut pool);
        assert_eq!(columns[0].data.text, columns[1].data.text);
        assert_ne!(columns[0], columns[1]);
        assert!(!Arc::ptr_eq(&columns[0].data, &columns[1].data));
        assert!(Arc::ptr_eq(&columns[2].data, &columns[3].data));
        assert_ne!(columns[2], columns[3]);
        assert_eq!(columns[2].iter().collect::<Vec<_>>(), [" ", " "]);
        assert_eq!(columns[3].iter().collect::<Vec<_>>(), [" "]);
    }

    #[test]
    fn tall_sparse_files_do_not_store_the_padding_rectangle() {
        let text = format!("{}\n{}", "a".repeat(2000), "b\n".repeat(1999));
        assert_eq!(text.len(), 5999);
        let grid = compute(&text, &text);
        assert_eq!(grid.height, 2000);
        assert_eq!(grid.steps.len(), 2000);
        assert!(grid.steps.iter().all(|step| step.kind == StepKind::Match));
        assert_eq!(
            grid.steps
                .iter()
                .map(|step| step.content.len())
                .sum::<usize>(),
            4_000_000
        );
        assert_eq!(
            grid.steps
                .iter()
                .map(|step| step.content.data.rows.len())
                .sum::<usize>(),
            3999
        );
        assert_eq!(
            grid.steps
                .iter()
                .map(|step| step.content.data.text.len())
                .sum::<usize>(),
            3999
        );
        assert_eq!(grid.steps[0].content.get(1999), Some("b"));
        assert_eq!(grid.steps[1].content.get(1999), Some(" "));
        let clone = grid.clone();
        for (original, copied) in grid.steps.iter().zip(&clone.steps) {
            assert!(Arc::ptr_eq(&original.content.data, &copied.content.data));
        }
    }

    #[test]
    fn tall_sparse_edits_keep_sparse_cells_and_newline_markers() {
        let text = format!("{}\n{}", "a".repeat(2000), "b\n".repeat(1999));
        let mut changed = text.clone();
        changed.replace_range(changed.len() - 2..changed.len() - 1, "c");
        let grid = compute(&text, &changed);
        assert_eq!(grid.steps.len(), 2001);
        assert!(!grid.degraded);
        assert_eq!(grid.steps[0].kind, StepKind::Delete);
        assert_eq!(grid.steps[0].content.get(1999), Some("b"));
        assert_eq!(grid.steps[1].kind, StepKind::Insert);
        assert_eq!(grid.steps[1].content.get(1999), Some("c"));
        assert!(
            grid.steps[2..]
                .iter()
                .all(|step| step.kind == StepKind::Match)
        );
        assert_eq!(
            grid.steps
                .iter()
                .map(|step| step.content.data.rows.len())
                .sum::<usize>(),
            5999
        );
        let without_newline = text.strip_suffix('\n').unwrap();
        let grid = compute(without_newline, &text);
        let newline = grid.steps.last().unwrap();
        assert_eq!(newline.kind, StepKind::Insert);
        assert_eq!(newline.content.len(), 2001);
        assert_eq!(newline.content.data.rows.len(), 1);
        assert_eq!(newline.content.get(1999), Some(" "));
        assert_eq!(newline.content.get(2000), Some("↵"));
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
        assert_eq!(expand_tabs("a\u{301}\tx"), "a\u{301}       x");
        assert_eq!(expand_tabs("👩‍❤️‍👩\tx"), "👩‍❤️‍👩      x");
        assert_eq!(expand_tabs("👩‍❤️‍👩\r\na\tx"), "👩‍❤️‍👩\r\na       x");
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
        let cols = test_columns(&["ab", "cd"]);
        let (steps, _) = diff_columns(&cols, &cols);
        assert_eq!(
            ops_of(&DiffGrid::from_parts(steps, 2, false)),
            exp(&[("=", "ab"), ("=", "cd")])
        );
    }

    #[test]
    fn diff_columns_replaces_one() {
        let a = test_columns(&["a", "b", "c"]);
        let b = test_columns(&["a", "x", "c"]);
        let (steps, _) = diff_columns(&a, &b);
        assert_eq!(
            ops_of(&DiffGrid::from_parts(steps, 1, false)),
            exp(&[("=", "a"), ("-", "b"), ("+", "x"), ("=", "c")])
        );
    }

    #[test]
    fn diff_columns_inserts_and_deletes() {
        let a = test_columns(&["a", "c"]);
        let b = test_columns(&["a", "b", "c"]);
        let (steps, _) = diff_columns(&a, &b);
        assert_eq!(
            ops_of(&DiffGrid::from_parts(steps, 1, false)),
            exp(&[("=", "a"), ("+", "b"), ("=", "c")])
        );
        let a = test_columns(&["a", "b", "c"]);
        let b = test_columns(&["a", "c"]);
        let (steps, _) = diff_columns(&a, &b);
        assert_eq!(
            ops_of(&DiffGrid::from_parts(steps, 1, false)),
            exp(&[("=", "a"), ("-", "b"), ("=", "c")])
        );
    }

    #[test]
    fn diff_columns_empty_sides() {
        assert!(diff_columns(&[], &[]).0.is_empty());
        let (steps, _) = diff_columns(&[], &test_columns(&["x"]));
        assert_eq!(
            ops_of(&DiffGrid::from_parts(steps, 1, false)),
            exp(&[("+", "x")])
        );
        let (steps, _) = diff_columns(&test_columns(&["x"]), &[]);
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
        assert_eq!(
            ops_of(&grid),
            exp(&[("+", "fb "), ("+", "oa "), ("+", "or "), ("+", "  ↵")])
        );
    }

    #[test]
    fn compute_empty_vs_empty() {
        let grid = compute("", "");
        assert!(grid.steps.is_empty());
        assert_eq!(grid.height, 0);
    }

    #[test]
    fn compute_reports_missing_trailing_newline() {
        // "a\nb" vs "a\nb\n" used to diff as identical; the missing
        // trailing newline is a synthetic step with a ↵ in its row.
        let grid = compute("a\nb", "a\nb\n");
        assert_eq!(ops_of(&grid), exp(&[("=", "ab "), ("+", "  ↵")]));
        let grid = compute("a\nb\n", "a\nb");
        assert_eq!(ops_of(&grid).last().unwrap().0, "-");
    }

    #[test]
    fn compute_empty_vs_single_newline() {
        // "" vs "\n": the added empty line's newline shows as a step.
        let grid = compute("", "\n");
        assert_eq!(ops_of(&grid), exp(&[("+", " ↵")]));
    }

    #[test]
    fn render_text_shows_missing_trailing_newline() {
        assert_eq!(
            render_text(&compute("a\nb", "a\nb\n")),
            " |+\na| \nb| \n |↵\n"
        );
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
    fn combining_graphemes_are_one_column_across_file_lines() {
        let grid = compute("a\u{301}x\n中bcx\n", "a\u{301}y\n");
        assert_eq!(
            ops_of(&grid),
            exp(&[
                ("-", "a\u{301}中"),
                ("-", "xb"),
                ("-", " c"),
                ("-", " x"),
                ("+", "a\u{301} "),
                ("+", "y "),
            ])
        );
        let expected = [
            ["a\u{301}", "中"],
            ["x", "b"],
            [" ", "c"],
            [" ", "x"],
            ["a\u{301}", " "],
            ["y", " "],
        ];
        for (step, cells) in grid.steps.iter().zip(expected) {
            assert_eq!(step.content.iter().collect::<Vec<_>>(), cells);
        }
        assert_eq!(grid.height, 2);
        assert_eq!(grid.groups, [0..4, 4..6]);
        assert_eq!(grid.widths, [2, 1, 1, 1, 1, 1]);
    }

    #[test]
    fn emoji_edits_replace_whole_grapheme_columns() {
        let grid = compute("👩‍❤️‍👩x\n", "👩‍❤️‍💋‍👩x\n");
        assert_eq!(grid.steps.len(), 3);
        assert_eq!(grid.steps[0].kind, StepKind::Delete);
        assert_eq!(grid.steps[0].content.iter().collect::<Vec<_>>(), ["👩‍❤️‍👩"]);
        assert_eq!(grid.steps[1].kind, StepKind::Insert);
        assert_eq!(grid.steps[1].content.iter().collect::<Vec<_>>(), ["👩‍❤️‍💋‍👩"]);
        assert_eq!(grid.steps[2].kind, StepKind::Match);
        assert_eq!(grid.steps[2].content.iter().collect::<Vec<_>>(), ["x"]);
        assert_eq!(grid.widths, [2, 2, 1]);
        assert_eq!(render_text(&grid), "- |+ |\n👩‍❤️‍👩|👩‍❤️‍💋‍👩|x\n");
    }

    #[test]
    fn grapheme_comparison_does_not_normalize_source_text() {
        let grid = compute("é\n", "e\u{301}\n");
        assert_eq!(grid.steps.len(), 2);
        assert_eq!(grid.steps[0].kind, StepKind::Delete);
        assert_eq!(grid.steps[0].content.iter().collect::<Vec<_>>(), ["é"]);
        assert_eq!(grid.steps[1].kind, StepKind::Insert);
        assert_eq!(
            grid.steps[1].content.iter().collect::<Vec<_>>(),
            ["e\u{301}"]
        );
        assert_eq!(render_text(&grid), "-|+\né|e\u{301}\n");
    }

    #[test]
    fn unicode_tabs_and_newline_markers_preserve_cell_boundaries() {
        let grid = compute("👩‍❤️‍👩\tx", "👩‍❤️‍👩\ty\n");
        assert_eq!(grid.steps.len(), 10);
        assert!(
            grid.steps[..7]
                .iter()
                .all(|step| step.kind == StepKind::Match)
        );
        assert_eq!(
            grid.steps[0].content.iter().collect::<Vec<_>>(),
            ["👩‍❤️‍👩", " "]
        );
        assert_eq!(grid.steps[7].content.iter().collect::<Vec<_>>(), ["x", " "]);
        assert_eq!(grid.steps[8].content.iter().collect::<Vec<_>>(), ["y", " "]);
        assert_eq!(grid.steps[9].content.iter().collect::<Vec<_>>(), [" ", "↵"]);
        assert_eq!(grid.steps[9].kind, StepKind::Insert);
        assert_eq!(
            render_text(&grid),
            "        |-|++\n👩‍❤️‍👩      |x|y \n        | | ↵\n"
        );
    }

    #[test]
    fn compute_pads_multibyte_columns_by_cells() {
        // "é" is 2 bytes but 1 cell — padding must count file rows,
        // or "é" vs "é " compares unequal and yields a spurious
        // Delete+Insert (the ASCII analog "a\n" vs "a\n \n" is a Match).
        let grid = compute("é\n", "é\n \n");
        assert_eq!(ops_of(&grid), exp(&[("=", "é ")]));
    }

    #[test]
    fn degraded_skips_lcs_for_pathological_widths() {
        // diff_columns works on columns, not lines: a 5000-char single
        // line becomes 5000 one-char columns (see columns_padded).
        let a = vec![Column::from(vec!["a".to_string()]); 5_000];
        let b = vec![Column::from(vec!["b".to_string()]); 5_000];
        // 5000*5000 > 1_000_000 → degraded path: LCS skipped, so each
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
        assert_eq!(
            render_text(&compute("", "foo\nbar\n")),
            "++++\nfoo \nbar \n   ↵\n"
        );
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
                content: vec!["a".to_owned(), "中".to_owned()].into()
            }),
            2
        );
        // All-zero-width column (bare combining mark) floors at 1.
        assert_eq!(
            step_width(&Step {
                kind: StepKind::Match,
                content: vec!["\u{0301}".to_owned()].into()
            }),
            1
        );
        assert_eq!(
            step_width(&Step {
                kind: StepKind::Match,
                content: vec!["👩‍❤️‍👩".to_owned(), "a\u{301}".to_owned()].into(),
            }),
            2
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
        // The whole ZWJ emoji is one cell and must never be split.
        let s = "👩‍❤️‍👩\n";
        assert_eq!(render_text(&compute(s, s)), s);
    }

    #[test]
    fn render_text_keeps_zwj_cluster_intact_in_wide_column() {
        // The emoji and 中 share column 0; the other 中 has its own column.
        let s = "👩‍❤️‍👩\n中中\n";
        let rendered = render_text(&compute(s, s));
        assert!(
            rendered.contains("👩‍❤️‍👩"),
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
        // 4 insert columns ("++++"), plus the synthetic step for the
        // trailing newline the empty side does not have.
        assert_eq!(render_text(&grid), "+++++\nfoo  \nbar  \nquux \n    ↵\n");
    }
}
