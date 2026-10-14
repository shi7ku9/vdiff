//! The frame around the diff: top bar, status bar, file sidebar, help
//! popup, and notices.

use super::{App, DiffSource, Notice};
use crate::git::{ChangedFile, Status};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Padding, Paragraph};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

impl App {
    /// `vdiff`, what is being compared, and the change counts.
    pub(super) fn render_top_bar(&self, frame: &mut Frame<'_>, area: Rect) {
        if area.is_empty() {
            return;
        }
        let theme = &self.theme;
        let buf = frame.buffer_mut();
        let logo = Span::styled(" vdiff ", theme.logo);
        let x = buf.set_span(area.x, area.y, &logo, area.width).0;
        // Two blank cells after the badge, one at the right edge.
        let room = usize::from(area.width).saturating_sub(logo.width() + 3);
        let labels = self.source_labels();
        let summary = self.summary();
        let summary_width = spans_width(&summary);
        // Shorten the label before giving up the summary.
        let with_summary = labels
            .iter()
            .find(|label| summary_width > 0 && spans_width(label) + 3 + summary_width <= room);
        let label = match with_summary {
            Some(label) => {
                let summary_x = area.right().saturating_sub(1 + summary_width as u16);
                buf.set_line(
                    summary_x,
                    area.y,
                    &Line::from(summary),
                    summary_width as u16,
                );
                label.clone()
            }
            None => fit_label(&labels, room, theme.muted),
        };
        buf.set_line(x.saturating_add(2), area.y, &Line::from(label), room as u16);
    }

    /// Labels for what is being compared, most detailed first.
    fn source_labels(&self) -> Vec<Vec<Span<'static>>> {
        let dim = self.theme.muted;
        let (old, new) = &self.labels;
        match &self.source {
            DiffSource::Files { .. } => pair_labels(old, new, dim),
            DiffSource::Git { files, .. } => {
                let revs = vec![
                    Span::raw(old.clone()),
                    Span::styled(" → ", dim),
                    Span::raw(new.clone()),
                ];
                let Some(file) = files.get(self.selection) else {
                    return vec![revs];
                };
                // The file matters more than the revisions on a narrow
                // screen, so the shorter labels drop the revisions.
                let paths = entry_labels(file, dim);
                let mut labels = vec![[revs, vec![Span::raw("   ")], paths[0].clone()].concat()];
                labels.extend(paths);
                labels
            }
        }
    }

    /// Change counts for the right end of the top bar.
    fn summary(&self) -> Vec<Span<'static>> {
        let theme = &self.theme;
        let Some(display) = &self.display else {
            return Vec::new();
        };
        let stats = display.stats;
        let mut spans = Vec::new();
        if self.degraded {
            spans.push(Span::styled(" LCS skipped ", theme.warning));
            spans.push(Span::raw("  "));
        }
        if stats.changes == 0 {
            spans.push(Span::styled("identical", theme.muted));
            return spans;
        }
        let plural = if stats.changes == 1 { "" } else { "s" };
        spans.extend([
            Span::styled(format!("{} change{plural}", stats.changes), theme.muted),
            Span::raw("   "),
            Span::styled(format!("-{}", stats.deleted), theme.delete_mark),
            Span::raw(" "),
            Span::styled(format!("+{}", stats.inserted), theme.insert_mark),
            Span::styled(" cols", theme.muted),
        ]);
        spans
    }

    /// Mode, visible ranges, and key hints.
    pub(super) fn render_status(&self, frame: &mut Frame<'_>, area: Rect) {
        if area.is_empty() {
            return;
        }
        let theme = &self.theme;
        let mode = match self.source {
            DiffSource::Files { .. } => " FILES ",
            DiffSource::Git { .. } => " GIT ",
        };
        let mode = Span::styled(mode, theme.mode);
        let mut positions = self.positions();
        let mut hints = self.hints();
        // On a narrow screen drop hints first, keeping `? help` the
        // longest, then the ranges.
        loop {
            let left = mode.width() + positions.iter().map(|p| 2 + spans_width(p)).sum::<usize>();
            let right = hints.iter().map(|h| spans_width(h)).sum::<usize>()
                + 3 * hints.len().saturating_sub(1);
            let needed = left + if hints.is_empty() { 0 } else { right + 3 };
            if needed <= usize::from(area.width) {
                break;
            }
            if hints.len() > 1 {
                hints.remove(0);
            } else if positions.pop().is_none() {
                if hints.is_empty() {
                    break;
                }
                hints.clear();
            }
        }
        let mut left = vec![mode];
        for position in positions {
            left.push(Span::raw("  "));
            left.extend(position);
        }
        let buf = frame.buffer_mut();
        buf.set_line(area.x, area.y, &Line::from(left), area.width);
        let mut right = Vec::new();
        for (i, hint) in hints.into_iter().enumerate() {
            if i > 0 {
                right.push(Span::raw("   "));
            }
            right.extend(hint);
        }
        let right_width = spans_width(&right) as u16;
        let right_x = area.right().saturating_sub(right_width + 1);
        buf.set_line(right_x, area.y, &Line::from(right), right_width);
    }

    /// The visible line range, and the column range when the diff is
    /// wider than the pane: `line 1-20/80  col 1-78/240`.
    fn positions(&self) -> Vec<Vec<Span<'static>>> {
        let (Some(grid), Some(display)) = (&self.grid, &self.display) else {
            return Vec::new();
        };
        let muted = self.theme.muted;
        let range = |name: &str, scroll: usize, shown: usize, total: usize| {
            let last = (scroll + shown).min(total);
            vec![
                Span::styled(format!("{name} "), muted),
                Span::raw(format!("{}-{last}", scroll + 1)),
                Span::styled(format!("/{total}"), muted),
            ]
        };
        let mut out = Vec::new();
        if grid.height > 0 {
            // The marker row takes one row of the pane.
            let shown = self.pane_height.saturating_sub(1).max(1);
            out.push(range("line", self.scroll_y, shown, grid.height));
        }
        if self.pane_width > 0 && display.width > self.pane_width {
            out.push(range("col", self.scroll_x, self.pane_width, display.width));
        }
        out
    }

    /// The few keys worth showing right now; `?` lists the rest.
    fn hints(&self) -> Vec<Vec<Span<'static>>> {
        let theme = &self.theme;
        let hint = |key: &'static str, action: &'static str| {
            vec![
                Span::styled(key, theme.key),
                Span::styled(format!(" {action}"), theme.muted),
            ]
        };
        let mut hints = Vec::new();
        if self.entries().len() > 1 {
            hints.push(hint("tab", "file"));
        } else if self.display.is_some() {
            hints.push(hint("hjkl", "scroll"));
        }
        if self.display.is_some() {
            hints.push(hint("n/p", "change"));
        }
        hints.push(hint("?", "help"));
        hints
    }

    /// Sidebar width for a main area `total` columns wide: the longest
    /// entry plus padding, within 24..=40 columns and two fifths of the
    /// screen.
    pub(super) fn sidebar_width(&self, total: u16) -> u16 {
        let longest = self
            .entries()
            .iter()
            .map(|file| entry_text(file).width())
            .max()
            .unwrap_or(0);
        // Selection mark, status letter, two spaces, path, one space.
        let wanted = (longest + 5).clamp(24, 40) as u16;
        wanted.min(total.saturating_mul(2) / 5)
    }

    /// Keep the selected entry among the `rows` entries on screen.
    pub(super) fn scroll_sidebar(&mut self, rows: usize) {
        let len = self.entries().len();
        self.sidebar_offset = self.sidebar_offset.min(len.saturating_sub(rows));
        if self.selection < self.sidebar_offset {
            self.sidebar_offset = self.selection;
        } else if rows > 0 && self.selection >= self.sidebar_offset + rows {
            self.sidebar_offset = self.selection + 1 - rows;
        }
    }

    pub(super) fn render_sidebar(&self, frame: &mut Frame<'_>, area: Rect) {
        if area.is_empty() {
            return;
        }
        let theme = &self.theme;
        let entries = self.entries();
        let buf = frame.buffer_mut();
        let title = Span::styled("FILES", theme.muted.add_modifier(Modifier::BOLD));
        buf.set_span(area.x + 1, area.y, &title, area.width.saturating_sub(1));
        let count = format!("{}/{}", self.selection + 1, entries.len());
        let count_width = count.width() as u16;
        if count_width + 8 <= area.width {
            buf.set_string(area.right() - 1 - count_width, area.y, &count, theme.muted);
        }
        let rows = usize::from(area.height - 1);
        let room = area.width.saturating_sub(5);
        for (row, (index, file)) in entries
            .iter()
            .enumerate()
            .skip(self.sidebar_offset)
            .take(rows)
            .enumerate()
        {
            let y = area.y + 1 + row as u16;
            let selected = index == self.selection;
            if selected {
                buf.set_style(Rect::new(area.x, y, area.width, 1), theme.selection);
                buf.set_string(area.x, y, "▌", theme.selection_mark);
            }
            let (letter, style) = match file.status {
                Status::Modified => ("M", theme.modified),
                Status::Added => ("A", theme.added),
                Status::Deleted => ("D", theme.deleted),
                Status::Renamed => ("R", theme.renamed),
            };
            buf.set_string(area.x + 1, y, letter, style);
            // Directories are muted, except on the selected row, whose
            // background would swallow them.
            let dim = if selected { Style::new() } else { theme.muted };
            let label = fit_label(&entry_labels(file, dim), usize::from(room), dim);
            buf.set_line(area.x + 4, y, &Line::from(label), room);
        }
    }

    /// The divider between the sidebar and the diff.
    pub(super) fn render_rule(&self, frame: &mut Frame<'_>, area: Rect) {
        let buf = frame.buffer_mut();
        for y in area.top()..area.bottom() {
            buf.set_string(area.x, y, "│", self.theme.rule);
        }
    }

    /// The key reference, centered over everything else.
    pub(super) fn render_help(&self, frame: &mut Frame<'_>, area: Rect) {
        let theme = &self.theme;
        let mut sections = vec![
            (
                "Scroll",
                vec![
                    ("j / k", "down / up"),
                    ("h / l", "left / right"),
                    ("g / G", "top / bottom"),
                ],
            ),
            ("Changes", vec![("n / p", "next / previous change")]),
        ];
        if self.has_sidebar() {
            sections.push((
                "Files",
                vec![
                    ("Tab / S-Tab", "next / previous file"),
                    ("e", "show / hide the file list"),
                ],
            ));
        }
        sections.push((
            "Mouse",
            vec![
                ("wheel", "scroll, sideways with Shift"),
                ("click", "pick a file or an overview spot"),
            ],
        ));
        sections.push((
            "General",
            vec![("?", "show / hide this help"), ("q", "quit")],
        ));
        let key_width = sections
            .iter()
            .flat_map(|(_, keys)| keys)
            .map(|(key, _)| key.width())
            .max()
            .unwrap_or(0);
        let mut lines = Vec::new();
        for (title, keys) in sections {
            if !lines.is_empty() {
                lines.push(Line::default());
            }
            lines.push(Line::styled(
                title,
                theme.muted.add_modifier(Modifier::BOLD),
            ));
            for (key, action) in keys {
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(format!("{key:<key_width$}"), theme.key),
                    Span::raw("   "),
                    Span::raw(action),
                ]));
            }
        }
        // Borders and a cell of padding on each side.
        let width = lines.iter().map(Line::width).max().unwrap_or(0) + 4;
        let height = lines.len() + 2;
        let popup = area.centered(
            Constraint::Length(width as u16),
            Constraint::Length(height as u16),
        );
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(theme.rule)
            .title(Line::styled(" Keys ", theme.key))
            .title_bottom(Line::styled(" esc to close ", theme.muted).right_aligned())
            .padding(Padding::horizontal(1));
        frame.render_widget(Clear, popup);
        frame.render_widget(Paragraph::new(lines).block(block), popup);
    }

    /// A centered message in place of the diff.
    pub(super) fn render_notice(&self, frame: &mut Frame<'_>, area: Rect, notice: &Notice) {
        let mut lines = vec![Line::styled(
            notice.title.clone(),
            Style::new().add_modifier(Modifier::BOLD),
        )];
        if let Some(detail) = &notice.detail {
            lines.push(Line::styled(detail.clone(), self.theme.muted));
        }
        let height = (lines.len() as u16).min(area.height);
        let area = Rect {
            y: area.y + (area.height - height) / 2,
            height,
            ..area
        };
        frame.render_widget(Paragraph::new(lines).alignment(Alignment::Center), area);
    }
}

/// A changed file as plain text: its path, or `dir/{old → new}` for a
/// rename.
pub(super) fn entry_text(file: &ChangedFile) -> String {
    entry_labels(file, Style::new())[0]
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

/// Labels for a changed file, most detailed first.
fn entry_labels(file: &ChangedFile, dim: Style) -> Vec<Vec<Span<'static>>> {
    match file.status {
        Status::Renamed => pair_labels(&file.old_path, &file.new_path, dim),
        _ => pair_labels(&file.new_path, &file.new_path, dim),
    }
}

/// Labels for an old and a new path, most detailed first: in full, then
/// with leading directories folded into `…` one at a time. Equal paths
/// show once; different ones as git prints renames, `dir/{a → b}`.
/// Directories and punctuation take the `dim` style.
fn pair_labels(old: &str, new: &str, dim: Style) -> Vec<Vec<Span<'static>>> {
    if old == new {
        let (dir, name) = split_dir(old);
        return folded(dir)
            .into_iter()
            .map(|dir| vec![Span::styled(dir, dim), Span::raw(name.to_string())])
            .collect();
    }
    let (prefix, old_mid, new_mid, suffix) = rename_parts(old, new);
    let braces = !prefix.is_empty() || !suffix.is_empty();
    folded(prefix)
        .into_iter()
        .map(|prefix| {
            let mut spans = vec![Span::styled(prefix, dim)];
            if braces {
                spans.push(Span::styled("{", dim));
            }
            spans.extend([
                Span::raw(old_mid.to_string()),
                Span::styled(" → ", dim),
                Span::raw(new_mid.to_string()),
            ]);
            if braces {
                spans.push(Span::styled("}", dim));
            }
            spans.push(Span::raw(suffix.to_string()));
            spans
        })
        .collect()
}

fn is_separator(byte: u8) -> bool {
    byte == b'/' || byte == std::path::MAIN_SEPARATOR as u8
}

/// `src/app.rs` → (`src/`, `app.rs`).
fn split_dir(path: &str) -> (&str, &str) {
    match path.bytes().rposition(is_separator) {
        Some(i) => path.split_at(i + 1),
        None => ("", path),
    }
}

/// `prefix` as written, then with its leading directories folded into
/// `…` one at a time: `/tmp/x/`, `…/x/`, `…/`.
fn folded(prefix: &str) -> Vec<String> {
    let mut out = vec![prefix.to_string()];
    for (i, byte) in prefix.bytes().enumerate() {
        if i > 0 && is_separator(byte) {
            out.push(format!("…{}", &prefix[i..]));
        }
    }
    out
}

/// Split two paths around their shared leading and trailing
/// directories, the way git prints renames (`pprint_rename` in git's
/// diff.c): `a/x/f.rs` and `a/y/f.rs` give (`a/`, `x`, `y`, `/f.rs`).
/// Both ends are cut at separators, so the parts stay valid UTF-8.
fn rename_parts<'a>(old: &'a str, new: &'a str) -> (&'a str, &'a str, &'a str, &'a str) {
    let (a, b) = (old.as_bytes(), new.as_bytes());
    // The shared prefix ends just after its last separator.
    let mut prefix = 0;
    for (i, (x, y)) in a.iter().zip(b).enumerate() {
        if x != y {
            break;
        }
        if is_separator(*x) {
            prefix = i + 1;
        }
    }
    // The shared suffix starts at a separator. It may reuse the one
    // that ends the prefix, but nothing before it.
    let floor = prefix.saturating_sub(1);
    let mut suffix = 0;
    for k in 1..=a.len().min(b.len()) {
        let (i, j) = (a.len() - k, b.len() - k);
        if i < floor || j < floor || a[i] != b[j] {
            break;
        }
        if is_separator(a[i]) {
            suffix = k;
        }
    }
    let old_mid = &old[prefix..(old.len() - suffix).max(prefix)];
    let new_mid = &new[prefix..(new.len() - suffix).max(prefix)];
    (&old[..prefix], old_mid, new_mid, &old[old.len() - suffix..])
}

/// The first label that fits in `width` cells, or else the last one cut
/// from the left.
fn fit_label(labels: &[Vec<Span<'static>>], width: usize, mark: Style) -> Vec<Span<'static>> {
    match labels.iter().find(|label| spans_width(label) <= width) {
        Some(label) => label.clone(),
        None => truncate_left(labels.last().map_or(&[], Vec::as_slice), width, mark),
    }
}

/// Keep the end of `spans` within `width` cells, marking the cut with
/// `…`.
fn truncate_left(spans: &[Span<'static>], width: usize, mark: Style) -> Vec<Span<'static>> {
    let total = spans_width(spans);
    if total <= width {
        return spans.to_vec();
    }
    if width == 0 {
        return Vec::new();
    }
    // Cells to cut, one of them for the mark.
    let mut cut = total - width + 1;
    let mut at_cut = true;
    let mut out = vec![Span::styled("…", mark)];
    for span in spans {
        let mut kept = String::new();
        for grapheme in span.content.graphemes(true) {
            if cut > 0 {
                cut = cut.saturating_sub(grapheme.width());
            } else if at_cut && grapheme.chars().all(char::is_whitespace) {
                // Spaces left at the cut go too, so the mark sits
                // against the text.
            } else {
                at_cut = false;
                kept.push_str(grapheme);
            }
        }
        if !kept.is_empty() {
            out.push(Span::styled(kept, span.style));
        }
    }
    out
}

fn spans_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(Span::width).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(spans: &[Span<'_>]) -> String {
        spans.iter().map(|span| span.content.as_ref()).collect()
    }

    fn labels(old: &str, new: &str) -> Vec<String> {
        pair_labels(old, new, Style::new())
            .iter()
            .map(|label| text(label))
            .collect()
    }

    #[test]
    fn rename_parts_cut_at_shared_directories() {
        assert_eq!(
            rename_parts("src/io.cpp", "src/fs.cpp"),
            ("src/", "io.cpp", "fs.cpp", "")
        );
        assert_eq!(
            rename_parts("a/x/f.rs", "a/y/f.rs"),
            ("a/", "x", "y", "/f.rs")
        );
        assert_eq!(
            rename_parts("old.cpp", "new.cpp"),
            ("", "old.cpp", "new.cpp", "")
        );
        // The suffix may reuse the prefix's trailing separator, as in
        // git's `a/{ => c}/b`.
        assert_eq!(rename_parts("a/b", "a/c/b"), ("a/", "", "c", "/b"));
        assert_eq!(
            rename_parts("文件/舊.txt", "文件/新.txt"),
            ("文件/", "舊.txt", "新.txt", "")
        );
    }

    #[test]
    fn pair_labels_fold_leading_directories() {
        assert_eq!(
            labels("/tmp/x/demo/old.cpp", "/tmp/x/demo/new.cpp"),
            [
                "/tmp/x/demo/{old.cpp → new.cpp}",
                "…/x/demo/{old.cpp → new.cpp}",
                "…/demo/{old.cpp → new.cpp}",
                "…/{old.cpp → new.cpp}",
            ]
        );
        assert_eq!(labels("old.cpp", "new.cpp"), ["old.cpp → new.cpp"]);
        assert_eq!(
            labels("src/app.rs", "src/app.rs"),
            ["src/app.rs", "…/app.rs"]
        );
        assert_eq!(
            labels("a/x/f.rs", "a/y/f.rs"),
            ["a/{x → y}/f.rs", "…/{x → y}/f.rs"]
        );
    }

    #[test]
    fn pair_labels_mute_directories() {
        let dim = Style::new().add_modifier(Modifier::DIM);
        let spans = pair_labels("src/app.rs", "src/app.rs", dim).remove(0);
        assert_eq!(spans[0].content, "src/");
        assert_eq!(spans[0].style, dim);
        assert_eq!(spans[1].content, "app.rs");
        assert_eq!(spans[1].style, Style::new());
    }

    #[test]
    fn entry_text_shows_renames_like_git() {
        let file = |status, old: &str, new: &str| ChangedFile {
            status,
            old_path: old.to_string(),
            new_path: new.to_string(),
        };
        assert_eq!(
            entry_text(&file(Status::Modified, "src/app.rs", "src/app.rs")),
            "src/app.rs"
        );
        assert_eq!(
            entry_text(&file(Status::Renamed, "src/io.cpp", "src/fs.cpp")),
            "src/{io.cpp → fs.cpp}"
        );
    }

    #[test]
    fn truncate_left_keeps_the_end() {
        let spans = vec![Span::raw("src/"), Span::raw("main.rs")];
        let mark = Style::new();
        assert_eq!(text(&truncate_left(&spans, 11, mark)), "src/main.rs");
        assert_eq!(text(&truncate_left(&spans, 8, mark)), "…main.rs");
        assert_eq!(text(&truncate_left(&spans, 5, mark)), "…n.rs");
        assert_eq!(text(&truncate_left(&spans, 0, mark)), "");
        // A wide grapheme is cut whole, never split.
        let wide = vec![Span::raw("中文.rs")];
        assert_eq!(text(&truncate_left(&wide, 5, mark)), "….rs");
    }

    #[test]
    fn fit_label_takes_the_first_that_fits() {
        let all = pair_labels("/tmp/x/demo/old.cpp", "/tmp/x/demo/new.cpp", Style::new());
        assert_eq!(
            text(&fit_label(&all, 40, Style::new())),
            "/tmp/x/demo/{old.cpp → new.cpp}"
        );
        assert_eq!(
            text(&fit_label(&all, 26, Style::new())),
            "…/demo/{old.cpp → new.cpp}"
        );
        assert_eq!(text(&fit_label(&all, 10, Style::new())), "…new.cpp}");
    }
}
