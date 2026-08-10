//! TUI rendering helpers and the ratatui App.
//!
//! The diff grid is rendered as display rows: `build_rows` turns a
//! `DiffGrid` into full-width strings (marker row first), and
//! `slice_by_width` implements width-aware horizontal scrolling so
//! wide characters (CJK) are never split in half.

use crate::diff::{DiffGrid, StepKind};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Display rows for the TUI: rows[0] is the marker row (one char per
/// step, `|` between groups, padded with spaces to the full content
/// display width), rows[i+1] is file line i. All rows share the same
/// display width.
pub fn build_rows(grid: &DiffGrid) -> Vec<String> {
    let mut marker = String::new();
    let mut rows: Vec<String> = (0..grid.height).map(|_| String::new()).collect();
    for (i, step) in grid.steps.iter().enumerate() {
        for (r, c) in step.content.iter().enumerate() {
            rows[r].push(*c);
        }
        marker.push(match step.kind {
            StepKind::Match => ' ',
            StepKind::Delete => '-',
            StepKind::Insert => '+',
        });
        let ends_group = grid.groups.iter().any(|g| g.end == i + 1) && i + 1 < grid.steps.len();
        if ends_group {
            marker.push('|');
            for row in &mut rows {
                row.push('|');
            }
        }
    }
    let content_width = rows.first().map(|r| UnicodeWidthStr::width(r.as_str())).unwrap_or(0);
    while UnicodeWidthStr::width(marker.as_str()) < content_width {
        marker.push(' ');
    }
    let mut out = vec![marker];
    out.extend(rows);
    out
}

/// Slice `s` by display width: skip the first `start` cells, keep at
/// most `max_width` cells. A wide char that straddles a boundary is
/// skipped whole (its width still consumes the cells up to the
/// boundary), so output never contains a partial wide char.
pub fn slice_by_width(s: &str, start: usize, max_width: usize) -> String {
    let mut out = String::new();
    let mut pos = 0usize;
    for c in s.chars() {
        let w = c.width().unwrap_or(0);
        if pos + w <= start {
            pos += w;
            continue;
        }
        if pos < start {
            // Wide char straddling the scroll boundary: skip it whole.
            pos += w;
            continue;
        }
        if pos + w <= start + max_width {
            out.push(c);
            pos += w;
        } else {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::compute;
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn build_rows_marker_padded_to_full_width() {
        let grid = compute("foo\nbar baz\nquux\n", "foo\nbar qaz\nquux\n");
        let rows = build_rows(&grid);
        assert_eq!(rows.len(), 4); // marker + 3 lines
        assert_eq!(rows[0], "    |-|+|  ");
        assert_eq!(rows[1], "foo | | |  ");
        assert_eq!(rows[2], "bar |b|q|az");
        assert_eq!(rows[3], "quux| | |  ");
        let widths: Vec<usize> = rows.iter().map(|r| UnicodeWidthStr::width(r.as_str())).collect();
        assert!(widths.windows(2).all(|w| w[0] == w[1]), "rows must share display width: {widths:?}");
    }

    #[test]
    fn build_rows_no_steps() {
        assert_eq!(build_rows(&compute("", "")), vec![""]);
    }

    #[test]
    fn slice_ascii() {
        assert_eq!(slice_by_width("abcdef", 2, 3), "cde");
    }

    #[test]
    fn slice_skips_straddling_wide_char() {
        // 中 (width 2) starts at 0, straddles start=1 → skipped whole
        assert_eq!(slice_by_width("中文", 1, 3), "文");
        // 中 straddles the right edge: width 2 does not fit in 1 cell
        assert_eq!(slice_by_width("中a", 0, 1), "");
    }

    #[test]
    fn slice_past_end_is_empty() {
        assert_eq!(slice_by_width("abc", 10, 5), "");
        assert_eq!(slice_by_width("", 0, 5), "");
    }
}
