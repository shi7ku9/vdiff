//! TUI rendering helpers and the ratatui App.
//!
//! The diff grid is rendered as display rows: `build_rows` turns a
//! `DiffGrid` into full-width strings (marker row first), and
//! `slice_by_width` implements width-aware horizontal scrolling so
//! wide characters (CJK) are never split in half.

use crate::diff::{self, DiffGrid, StepKind, cell_pad, step_width};
use crate::git::{self, ChangedFile, GitShell, RevSpec, Status};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use std::io;
use std::path::PathBuf;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Display rows for the TUI: rows[0] is the marker row (one char per
/// step, `|` between groups, padded with spaces to the full content
/// display width), rows[i+1] is file line i. Cells are padded to
/// their step column's display width up to the last `|`, so the
/// separators line up vertically; past it, rows may differ at the
/// tail.
pub fn build_rows(grid: &DiffGrid) -> Vec<String> {
    let mut marker = String::new();
    let mut rows: Vec<String> = (0..grid.height).map(|_| String::new()).collect();
    // Only cells up to the last step with a `|` after it get padded.
    let pad_until = grid
        .groups
        .iter()
        .filter(|g| g.end < grid.steps.len())
        .map(|g| g.end)
        .max()
        .unwrap_or(0);
    for (i, step) in grid.steps.iter().enumerate() {
        let col_w = step_width(step);
        let pad_cell = |c: char| if i < pad_until { cell_pad(c, col_w) } else { 0 };
        for (r, c) in step.content.iter().enumerate() {
            rows[r].push(*c);
            // Pad every cell to its step column's display width so
            // wide (CJK) cells don't shift the `|` separators.
            for _ in 0..pad_cell(*c) {
                rows[r].push(' ');
            }
        }
        let mark = match step.kind {
            StepKind::Match => ' ',
            StepKind::Delete => '-',
            StepKind::Insert => '+',
        };
        marker.push(mark);
        // Marker glyphs are 1 cell wide, so the general cell rule
        // pads them to col_w - 1.
        for _ in 0..pad_cell(mark) {
            marker.push(' ');
        }
        let ends_group = grid.groups.iter().any(|g| g.end == i + 1) && i + 1 < grid.steps.len();
        if ends_group {
            marker.push('|');
            for row in &mut rows {
                row.push('|');
            }
        }
    }
    // Pad the marker to the widest row: the tail cells past the last
    // separator are unpadded, so rows (and the marker) may differ
    // there.
    let content_width = rows
        .iter()
        .map(|r| UnicodeWidthStr::width(r.as_str()))
        .max()
        .unwrap_or(0);
    while UnicodeWidthStr::width(marker.as_str()) < content_width {
        marker.push(' ');
    }
    let mut out = vec![marker];
    out.extend(rows);
    out
}

pub enum DiffSource {
    Files {
        old: PathBuf,
        new: PathBuf,
    },
    Git {
        spec: RevSpec,
        files: Vec<ChangedFile>,
        git: Box<dyn GitShell>,
    },
}

impl DiffSource {
    pub fn from_cli(cli: &crate::cli::Cli) -> Result<Self, git::GitError> {
        match &cli.command {
            crate::cli::Command::Files { file1, file2 } => Ok(DiffSource::Files {
                old: file1.clone(),
                new: file2.clone(),
            }),
            crate::cli::Command::Git { cached, revs } => {
                let g: Box<dyn GitShell> = Box::new(git::RealGit);
                if !git::in_repo(&*g) {
                    return Err(git::GitError::NotARepo);
                }
                let spec = git::resolve(&*g, *cached, revs)?;
                let files = git::changed_files(&*g, &spec)?;
                Ok(DiffSource::Git {
                    spec,
                    files,
                    git: g,
                })
            }
        }
    }
}

pub struct App {
    pub source: DiffSource,
    pub selection: usize,
    pub show_sidebar: bool,
    pub transposed: bool,
    pub scroll_y: usize,
    pub scroll_x: usize,
    pub grid: Option<DiffGrid>,
    pub message: Option<String>,
    pub degraded: bool,
    pub labels: (String, String),
    pane_width: usize,
    pane_height: usize,
}

impl App {
    pub fn new(source: DiffSource) -> App {
        let labels = match &source {
            DiffSource::Files { old, new } => {
                (old.display().to_string(), new.display().to_string())
            }
            DiffSource::Git { spec, .. } => (spec.old_label(), spec.new_label()),
        };
        let mut app = App {
            source,
            selection: 0,
            show_sidebar: true,
            transposed: false,
            scroll_y: 0,
            scroll_x: 0,
            grid: None,
            message: None,
            degraded: false,
            labels,
            // Unknown until the first render: 0 means "no clamp yet"
            // (max_scroll = content size). A positive default like
            // 80x24 would pin max_scroll_y/x to 0 for small diffs and
            // make j/l/n/p unable to move.
            pane_width: 0,
            pane_height: 0,
        };
        app.reload();
        app
    }

    pub fn has_sidebar(&self) -> bool {
        matches!(self.source, DiffSource::Git { .. })
    }

    fn entries(&self) -> Vec<ChangedFile> {
        match &self.source {
            DiffSource::Git { files, .. } => files.clone(),
            DiffSource::Files { .. } => Vec::new(),
        }
    }

    fn reload(&mut self) {
        let contents = match &self.source {
            DiffSource::Files { old, new } => {
                match (std::fs::read_to_string(old), std::fs::read_to_string(new)) {
                    (Ok(a), Ok(b)) => Some((a, b)),
                    _ => None,
                }
            }
            DiffSource::Git { files, .. } if files.is_empty() => None,
            DiffSource::Git { files, git, spec } => {
                let idx = self.selection.min(files.len().saturating_sub(1));
                git::load_content(&**git, spec, &files[idx])
            }
        };
        self.scroll_y = 0;
        self.scroll_x = 0;
        match contents {
            Some((old, new)) => {
                let grid = diff::compute(&old, &new);
                self.degraded = grid.degraded;
                self.grid = Some(grid);
                self.message = None;
            }
            None => {
                self.grid = None;
                self.message = Some(match &self.source {
                    DiffSource::Git { files, .. } if files.is_empty() => "no changes".to_string(),
                    _ => "binary or unreadable".to_string(),
                });
            }
        }
    }

    fn content_dims(&self) -> (usize, usize) {
        let Some(grid) = &self.grid else {
            return (0, 0);
        };
        if self.transposed {
            let rows = transposed_rows(grid);
            let w = rows
                .iter()
                .map(|r| UnicodeWidthStr::width(r.as_str()))
                .max()
                .unwrap_or(0);
            (grid.steps.len(), w)
        } else {
            let rows = build_rows(grid);
            // Rows share width up to the last `|`, then differ at the
            // unpadded tail; take the widest.
            let w = rows
                .iter()
                .map(|r| UnicodeWidthStr::width(r.as_str()))
                .max()
                .unwrap_or(0);
            (grid.height, w)
        }
    }

    fn max_scroll_y(&self) -> usize {
        let (h, _) = self.content_dims();
        // The bordered diff pane's inner area is pane_height - 2 rows;
        // the sticky marker row occupies one of them in normal view,
        // leaving pane_height - 3 content rows visible. Clamp against
        // that window so the bottom rows of a tall diff are reachable.
        h.saturating_sub(self.pane_height.saturating_sub(3))
    }

    fn max_scroll_x(&self) -> usize {
        let (_, w) = self.content_dims();
        // The horizontal window is the bordered inner width:
        // pane_width - 2 columns.
        w.saturating_sub(self.pane_width.saturating_sub(2))
    }

    pub fn render(&mut self, frame: &mut Frame<'_>) {
        let area = frame.area();
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0), Constraint::Length(1)])
            .split(area);
        let (sidebar_area, diff_area) = if self.has_sidebar() && self.show_sidebar {
            let c = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(28), Constraint::Min(0)])
                .split(chunks[0]);
            (Some(c[0]), c[1])
        } else {
            (None, chunks[0])
        };
        if let Some(area) = sidebar_area {
            self.render_sidebar(frame, area);
        }
        self.render_diff(frame, diff_area);
        self.render_status(frame, chunks[1]);
        self.pane_width = diff_area.width as usize;
        self.pane_height = diff_area.height as usize;
    }

    fn render_sidebar(&self, frame: &mut Frame<'_>, area: Rect) {
        let items: Vec<ListItem> = self
            .entries()
            .iter()
            .map(|f| {
                let (letter, color) = match f.status {
                    Status::Modified => ("M", Color::Yellow),
                    Status::Added => ("A", Color::Green),
                    Status::Deleted => ("D", Color::Red),
                    Status::Renamed => ("R", Color::Cyan),
                };
                let path = if f.status == Status::Renamed {
                    format!("{} → {}", f.old_path, f.new_path)
                } else {
                    f.new_path.clone()
                };
                ListItem::new(Line::from(vec![
                    Span::styled(letter, Style::default().fg(color)),
                    Span::raw(" "),
                    Span::raw(path),
                ]))
            })
            .collect();
        let mut state = ListState::default().with_selected(Some(self.selection));
        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title(" Files "))
            .highlight_style(
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("> ");
        frame.render_stateful_widget(list, area, &mut state);
    }

    fn render_diff(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let title = format!(" {} → {} ", self.labels.0, self.labels.1);
        let block = Block::default().borders(Borders::ALL).title(title);
        let inner = block.inner(area);
        frame.render_widget(block, area);

        match (&self.grid, &self.message) {
            (Some(grid), _) => {
                let rows: Vec<String> = if self.transposed {
                    transposed_rows(grid)
                } else {
                    build_rows(grid)
                };
                if self.transposed {
                    for (i, row) in rows.iter().enumerate() {
                        // saturating: rows scrolled past the top clamp to
                        // 0 and are skipped below (plain subtraction would
                        // underflow u16 in debug builds).
                        let y = inner
                            .y
                            .saturating_add(i as u16)
                            .saturating_sub(self.scroll_y as u16);
                        if y < inner.y {
                            continue;
                        }
                        if y >= inner.y + inner.height {
                            break;
                        }
                        self.draw_step_row(frame, inner, y, row);
                    }
                } else {
                    if let Some(marker) = rows.first() {
                        self.draw_marker_row(frame, inner, inner.y, marker);
                    }
                    for (i, row) in rows.iter().enumerate().skip(1) {
                        // saturating: rows scrolled past the top clamp to
                        // 0 and are skipped below (plain subtraction would
                        // underflow u16 in debug builds).
                        let y = inner
                            .y
                            .saturating_add(i as u16)
                            .saturating_sub(self.scroll_y as u16);
                        // Content rows live below the sticky marker row:
                        // y == inner.y is the marker's row and must not be
                        // overwritten once the user scrolls.
                        if y <= inner.y {
                            continue;
                        }
                        if y >= inner.y + inner.height {
                            break;
                        }
                        self.draw_content_row(frame, inner, y, row);
                    }
                }
            }
            (None, Some(message)) => {
                let area = Rect {
                    x: inner.x,
                    y: inner.y + inner.height.saturating_sub(1) / 2,
                    width: inner.width,
                    height: 1,
                };
                frame.render_widget(
                    Paragraph::new(message.as_str()).alignment(ratatui::layout::Alignment::Center),
                    area,
                );
            }
            (None, None) => {}
        }
    }

    fn draw_row(
        &self,
        frame: &mut Frame<'_>,
        inner: Rect,
        y: u16,
        row: &str,
        scroll_x: usize,
        style_for: impl Fn(char) -> Style,
    ) {
        let (visible, first_cell) = slice_by_width(row, scroll_x, inner.width as usize);
        // A wide char straddling the scroll boundary is skipped whole,
        // so the first drawn char may sit past the viewport start; the
        // offset keeps this row's grid cells under the same viewport
        // columns as the (all-width-1) marker row.
        let mut x = inner.x + (first_cell - scroll_x) as u16;
        for c in visible.chars() {
            frame
                .buffer_mut()
                .set_string(x, y, c.to_string(), style_for(c));
            x += c.width().unwrap_or(0) as u16;
        }
    }

    fn draw_marker_row(&self, frame: &mut Frame<'_>, inner: Rect, y: u16, row: &str) {
        self.draw_row(frame, inner, y, row, self.scroll_x, |c| match c {
            '-' => Style::default().fg(Color::Red),
            '+' => Style::default().fg(Color::Green),
            '|' => Style::default().fg(Color::DarkGray),
            _ => Style::default(),
        });
    }

    fn draw_content_row(&self, frame: &mut Frame<'_>, inner: Rect, y: u16, row: &str) {
        self.draw_row(frame, inner, y, row, self.scroll_x, |c| {
            if c == '|' {
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default()
            }
        });
    }

    fn draw_step_row(&self, frame: &mut Frame<'_>, inner: Rect, y: u16, row: &str) {
        // Transposed rows start with the prefix char — color only that one.
        self.draw_row(frame, inner, y, row, self.scroll_x, |c| match c {
            '-' => Style::default().fg(Color::Red),
            '+' => Style::default().fg(Color::Green),
            _ => Style::default(),
        });
    }

    fn render_status(&self, frame: &mut Frame<'_>, area: Rect) {
        let mut spans = vec![Span::raw(format!(
            " {} → {}  ",
            self.labels.0, self.labels.1
        ))];
        match (&self.grid, &self.message) {
            (Some(_), _) => {
                spans.push(Span::raw(format!(
                    "v{}/{} h{}/{}  ",
                    self.scroll_y,
                    self.max_scroll_y(),
                    self.scroll_x,
                    self.max_scroll_x()
                )));
                if self.degraded {
                    spans.push(Span::raw("LCS skipped (wide file)  "));
                }
            }
            (None, Some(m)) => spans.push(Span::raw(format!("{m}  "))),
            (None, None) => {}
        }
        spans.push(Span::raw(
            "tab/⇧tab select · hjkl scroll · n/p changes · t transpose · e sidebar · q quit",
        ));
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    /// Handle one key event. Returns true when the app should quit.
    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')) {
            return true;
        }
        match key.code {
            KeyCode::Char('q') => return true,
            KeyCode::Char('t') => {
                self.transposed = !self.transposed;
                self.scroll_y = 0;
                self.scroll_x = 0;
            }
            KeyCode::Char('e') if self.has_sidebar() => self.show_sidebar = !self.show_sidebar,
            // vim convention: g = top, G = bottom.
            KeyCode::Char('g') => self.scroll_y = 0,
            KeyCode::Char('G') => self.scroll_y = self.max_scroll_y(),
            KeyCode::Char('n') => self.jump_group(1),
            KeyCode::Char('p') => self.jump_group(-1),
            KeyCode::Char('h') => self.scroll_x = self.scroll_x.saturating_sub(1),
            KeyCode::Char('l') => self.scroll_x = (self.scroll_x + 1).min(self.max_scroll_x()),
            KeyCode::Char('j') => self.scroll_y = (self.scroll_y + 1).min(self.max_scroll_y()),
            KeyCode::Char('k') => self.scroll_y = self.scroll_y.saturating_sub(1),
            KeyCode::Tab => self.move_selection(1),
            KeyCode::BackTab => self.move_selection(-1),
            _ => {}
        }
        false
    }

    fn move_selection(&mut self, delta: i32) {
        let len = self.entries().len();
        if len == 0 {
            return;
        }
        let next = (self.selection as i32 + delta).clamp(0, len as i32 - 1) as usize;
        if next != self.selection {
            self.selection = next;
            self.reload();
        }
    }

    /// Jump to the next (dir=1) or previous (dir=-1) change group.
    fn jump_group(&mut self, dir: i32) {
        let Some(grid) = &self.grid else { return };
        // Change groups: runs of non-Match steps (kind != Match).
        let change_groups: Vec<usize> = grid
            .groups
            .iter()
            .enumerate()
            .filter(|(_, g)| grid.steps[g.start].kind != StepKind::Match)
            .map(|(i, _)| i)
            .collect();
        if change_groups.is_empty() {
            return;
        }
        // Anchor row of each change group: the first affected file row in
        // normal view, the group's first step in transposed view.
        let ranges: Vec<std::ops::Range<usize>> = change_groups
            .iter()
            .map(|&i| grid.groups[i].clone())
            .collect();
        let rows: Vec<usize> = ranges
            .iter()
            .map(|g| {
                if self.transposed {
                    g.start
                } else {
                    first_affected_row(grid, g).unwrap_or(usize::MAX)
                }
            })
            .collect();
        // All grid reads above are done; below we only mutate scrolls.
        let target = if dir > 0 {
            // Next change group after the current scroll row; at the
            // bottom (nothing below) wrap around to the first one.
            ranges
                .iter()
                .zip(&rows)
                .find(|(_, r)| **r > self.scroll_y)
                .map(|(g, _)| g.clone())
                .or_else(|| ranges.first().cloned())
        } else {
            // Previous change group before the current scroll row.
            ranges
                .iter()
                .zip(&rows)
                .rev()
                .find(|(_, r)| **r < self.scroll_y)
                .map(|(g, _)| g.clone())
                .or_else(|| {
                    if self.scroll_y == 0 && self.scroll_x == 0 {
                        // At the very top: wrap to the last group.
                        ranges.last().cloned()
                    } else {
                        // Already at the first change group: back to the top.
                        None
                    }
                })
        };
        let Some(group) = target else {
            self.scroll_y = 0;
            self.scroll_x = 0;
            return;
        };
        if self.transposed {
            self.scroll_y = group.start.min(self.max_scroll_y());
            self.scroll_x = 0;
            return;
        }
        let Some(grid) = &self.grid else { return };
        if let Some(row) = first_affected_row(grid, &group) {
            self.scroll_y = row.min(self.max_scroll_y());
            self.scroll_x = group_start_width(grid, &group).min(self.max_scroll_x());
        }
    }
}

/// The first file line where any step in `group` has a non-padding char.
fn first_affected_row(grid: &DiffGrid, group: &std::ops::Range<usize>) -> Option<usize> {
    (0..grid.height).find(|&r| {
        grid.steps[group.clone()]
            .iter()
            .any(|s| s.content.get(r).copied().unwrap_or(' ') != ' ')
    })
}

/// Display width of the grid content before `group` ends (step widths
/// plus one cell per group separator, counting the separator after the
/// group's own last step). For a delete+insert pair this lands on the
/// column of the insert's `+` — the marker position where the change
/// is visible.
fn group_start_width(grid: &DiffGrid, group: &std::ops::Range<usize>) -> usize {
    let mut w = 0usize;
    for (i, step) in grid.steps.iter().take(group.end).enumerate() {
        w += step_width(step);
        if grid.groups.iter().any(|g| g.end == i + 1) {
            w += 1;
        }
    }
    w
}

/// Transposed display rows: one per step — prefix char (` `/`-`/`+`) +
/// `' '` + column content.
fn transposed_rows(grid: &DiffGrid) -> Vec<String> {
    grid.steps
        .iter()
        .map(|s| {
            let mut line = String::with_capacity(s.content.len() + 2);
            line.push(match s.kind {
                StepKind::Match => ' ',
                StepKind::Delete => '-',
                StepKind::Insert => '+',
            });
            line.push(' ');
            line.extend(s.content.iter());
            line
        })
        .collect()
}

pub fn run_tui(cli: &crate::cli::Cli) -> Result<(), Box<dyn std::error::Error>> {
    let source = DiffSource::from_cli(cli)?;
    let mut app = App::new(source);
    run_tui_app(&mut app)?;
    Ok(())
}

/// Drive the terminal loop until the app signals quit.
pub fn run_tui_app(app: &mut App) -> io::Result<()> {
    let mut terminal = ratatui::init();
    let result = (|| {
        loop {
            terminal.draw(|frame| app.render(frame))?;
            if let Event::Key(key) = event::read()?
                && app.handle_key(key)
            {
                break;
            }
        }
        Ok(())
    })();
    ratatui::restore();
    result
}

/// Slice `s` by display width: skip the first `start` cells, keep at
/// most `max_width` cells. A wide char that straddles a boundary is
/// skipped whole (its width still consumes the cells up to the
/// boundary), so output never contains a partial wide char. Returns
/// the drawn text plus the grid cell index of its first char. That
/// index sits past `start` when a straddling wide char was skipped,
/// so the caller can keep rows aligned under the same viewport
/// columns.
pub fn slice_by_width(s: &str, start: usize, max_width: usize) -> (String, usize) {
    let mut out = String::new();
    let mut first = start;
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
            if out.is_empty() {
                first = pos;
            }
            out.push(c);
            pos += w;
        } else {
            break;
        }
    }
    (out, first)
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
        let widths: Vec<usize> = rows
            .iter()
            .map(|r| UnicodeWidthStr::width(r.as_str()))
            .collect();
        assert!(
            widths.windows(2).all(|w| w[0] == w[1]),
            "rows must share display width: {widths:?}"
        );
    }

    #[test]
    fn build_rows_no_steps() {
        assert_eq!(build_rows(&compute("", "")), vec![""]);
    }

    #[test]
    fn build_rows_cjk_pads_cells_to_column_width() {
        // Column 0 changes whole (中a → 中b): its cells are width 2, so
        // the narrow `a`/`b` cells get a trailing pad and the marker's
        // `-`/`+` sit over the `中`/`中` cells' first display cell.
        let grid = compute("中文\na文\n", "中文\nb文\n");
        let rows = build_rows(&grid);
        assert_eq!(rows[0], "- |+ |  ");
        assert_eq!(rows[1], "中|中|文");
        assert_eq!(rows[2], "a |b |文");
        let widths: Vec<usize> = rows
            .iter()
            .map(|r| UnicodeWidthStr::width(r.as_str()))
            .collect();
        assert!(
            widths.windows(2).all(|w| w[0] == w[1]),
            "rows must share display width: {widths:?}"
        );
    }

    #[test]
    fn slice_ascii() {
        assert_eq!(slice_by_width("abcdef", 2, 3), ("cde".to_string(), 2));
    }

    #[test]
    fn slice_skips_straddling_wide_char() {
        // 中 (width 2) starts at 0, straddles start=1 → skipped whole;
        // the first drawn char sits at cell 2, one past the viewport.
        assert_eq!(slice_by_width("中文", 1, 3), ("文".to_string(), 2));
        // 中 straddles the right edge: width 2 does not fit in 1 cell
        assert_eq!(slice_by_width("中a", 0, 1), (String::new(), 0));
    }

    #[test]
    fn slice_past_end_is_empty() {
        assert_eq!(slice_by_width("abc", 10, 5), (String::new(), 10));
        assert_eq!(slice_by_width("", 0, 5), (String::new(), 0));
    }

    use crate::git::{ChangedFile, FakeGit, RevSpec, Source, Status};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn temp_file(name: &str, contents: &str) -> std::path::PathBuf {
        // Unique per call: tests run in parallel and would otherwise
        // clobber each other's files and remove them mid-test.
        static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("vdiff-app-{}-{n}-{name}", std::process::id()));
        std::fs::write(&path, contents).unwrap();
        path
    }

    fn files_app(old: &str, new: &str) -> (App, std::path::PathBuf, std::path::PathBuf) {
        let a = temp_file("a.txt", old);
        let b = temp_file("b.txt", new);
        let app = App::new(DiffSource::Files {
            old: a.clone(),
            new: b.clone(),
        });
        (app, a, b)
    }

    /// A git-mode app whose diff content is faked through a seeded
    /// FakeGit (the same instance must be inside the App, since
    /// App::new → reload → load_content fetches through it). The spec
    /// diffs HEAD~1 (old side) against HEAD (new side), so the seeds
    /// are truthful: HEAD~1 holds the "old" content, HEAD the "new".
    fn git_app(contents: Vec<(&str, &str)>) -> App {
        let mut f = FakeGit::default();
        let mut files = Vec::new();
        for (i, (old, new)) in contents.into_iter().enumerate() {
            let path = format!("f{i}.txt");
            f.set(
                &["cat-file", "-p", &format!("HEAD~1:{path}")],
                Some(old.to_string()),
            );
            f.set(
                &["cat-file", "-p", &format!("HEAD:{path}")],
                Some(new.to_string()),
            );
            files.push(ChangedFile {
                status: Status::Modified,
                old_path: path.clone(),
                new_path: path,
            });
        }
        let spec = RevSpec {
            old: Source::Rev("HEAD~1".into()),
            new: Source::Rev("HEAD".into()),
            diff_args: vec![],
        };
        App::new(DiffSource::Git {
            spec,
            files,
            git: Box::new(f),
        })
    }

    fn draw(app: &mut App) -> TestBackend {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();
        terminal.backend().clone()
    }

    #[test]
    fn renders_marker_row_with_colors() {
        let (mut app, a, b) = files_app("foo\nbar baz\nquux\n", "foo\nbar qaz\nquux\n");
        let backend = draw(&mut app);
        let buf = backend.buffer();
        // The diff pane block is bordered, so inner starts at (1,1):
        // border at y=0, marker row at y=1 from x=1.
        // Marker: "    |-|+...": '|' at col 5, '-' at col 6, '+' at col 8.
        assert_eq!(buf[(6, 1)].symbol(), "-");
        assert_eq!(buf[(6, 1)].fg, ratatui::style::Color::Red);
        assert_eq!(buf[(8, 1)].symbol(), "+");
        assert_eq!(buf[(8, 1)].fg, ratatui::style::Color::Green);
        assert_eq!(buf[(5, 1)].symbol(), "|");
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    fn cjk_separators_align_in_tui() {
        let (mut app, a, b) = files_app("中文abc\n", "中日abc\n");
        let backend = draw(&mut app);
        let buf = backend.buffer();
        // Marker "  |- |+ |   " and content "中|文|日|abc" both place
        // the `|`s at display cells 2, 5, 8. In buffer coords (inner
        // starts at (1,1)) those are x = 3, 6, 9 on the marker row
        // (y=1) and the content row (y=2).
        for x in [3, 6, 9] {
            assert_eq!(buf[(x, 1)].symbol(), "|", "marker | at x={x}");
            assert_eq!(buf[(x, 2)].symbol(), "|", "content | at x={x}");
        }
        // The `-`/`+` markers sit over their column's first display
        // cell: '-' at buffer (4,1) over 文 at (4,2), '+' at (7,1) over
        // 日 at (7,2). Continuation cells (x=5, x=8) are skipped;
        // TestBackend leaves them empty.
        assert_eq!(buf[(4, 1)].symbol(), "-");
        assert_eq!(buf[(7, 1)].symbol(), "+");
        assert_eq!(buf[(4, 2)].symbol(), "文");
        assert_eq!(buf[(7, 2)].symbol(), "日");
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    fn cjk_separators_stay_aligned_when_scrolled_into_wide_char() {
        let (mut app, a, b) = files_app("中文abc\n", "中日abc\n");
        for scroll_x in [1, 4] {
            // scroll_x cuts inside a wide char (中 at cells 0-1, 文 at
            // 3-4): it is skipped whole, so the first drawn char sits
            // one cell past the viewport; the marker row must still
            // show the cell-2 `|` (viewport 1, buffer x = inner.x + 1
            // = 2) over the content row's cell-2 `|`.
            app.scroll_x = scroll_x;
            let backend = draw(&mut app);
            let buf = backend.buffer();
            assert_eq!(
                buf[(2, 1)].symbol(),
                "|",
                "marker | at viewport 1 (scroll_x={scroll_x})"
            );
            assert_eq!(
                buf[(2, 2)].symbol(),
                "|",
                "content | at viewport 1 (scroll_x={scroll_x})"
            );
        }
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    fn marker_row_is_sticky_when_scrolling() {
        let (mut app, a, b) = files_app(
            "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12\n13\n14\n15\n16\n17\n18\n19\n20\n",
            "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12\n13\n14\n15\n16\n17\n18\n19\n21\n",
        );
        app.scroll_y = 10;
        let backend = draw(&mut app);
        let buf = backend.buffer();
        // Marker " |-|+" starts at x=1 (block border), '-' at x=3 —
        // still at the top row after vertical scroll.
        assert_eq!(buf[(3, 1)].symbol(), "-");
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    fn sidebar_shows_status_letters() {
        let mut app = git_app(vec![("foo\n", "bar\n"), ("a\n", "b\n")]);
        let backend = draw(&mut app);
        let buf = backend.buffer();
        // Sidebar block is bordered (inner x=1) and the highlight symbol
        // "> " occupies columns 1-2, so the status letter starts at x=3.
        assert_eq!(buf[(3, 1)].symbol(), "M"); // first entry: 'M' + space + path
        // highlight symbol "> " on the selected entry
        assert_eq!(buf[(1, 1)].symbol(), ">");
    }

    #[test]
    fn transposed_view_renders_step_rows() {
        let (mut app, a, b) = files_app("foo\nbar baz\nquux\n", "foo\nbar qaz\nquux\n");
        app.transposed = true;
        let backend = draw(&mut app);
        let buf = backend.buffer();
        // The diff pane block is bordered: rows start at x=1. Transposed
        // row 0 = "  fbq" — prefix space + separator space at (1,1) and
        // (2,1), 'f' at (3,1).
        assert_eq!(buf[(3, 1)].symbol(), "f");
        // delete step row "- b " is row 4 of the stack: y = 1 + 4, prefix at x=1
        assert_eq!(buf[(1, 5)].symbol(), "-");
        assert_eq!(buf[(1, 5)].fg, ratatui::style::Color::Red);
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    fn sidebar_hidden_diff_takes_full_width() {
        let mut app = git_app(vec![("foo\nbar baz\nquux\n", "foo\nbar qaz\nquux\n")]);
        let buf_before = draw(&mut app);
        assert_ne!(buf_before.buffer()[(4, 1)].symbol(), "-"); // (4,1) is inside the sidebar
        app.show_sidebar = false;
        let buf_after = draw(&mut app);
        // Full-width diff pane is still bordered: inner starts at x=1,
        // marker "    |-|+|  " → '|' at (5,1), '-' at (6,1).
        assert_eq!(buf_after.buffer()[(5, 1)].symbol(), "|"); // marker separator at (5,1)
        assert_eq!(buf_after.buffer()[(6, 1)].symbol(), "-");
    }

    #[test]
    fn placeholder_for_no_changes() {
        let mut app = git_app(vec![]);
        let backend = draw(&mut app);
        let buf = backend.buffer();
        let row: String = (0..80).map(|x| buf[(x, 11)].symbol().to_string()).collect();
        assert!(
            row.contains("no changes"),
            "expected placeholder text, got {row:?}"
        );
    }

    #[test]
    fn max_scroll_y_reaches_bottom_content_rows() {
        // 30 content rows in the 80x24 pane: the bordered diff pane has
        // inner height 21 and the sticky marker row occupies one of
        // those, so 20 content rows are visible and the last line only
        // becomes reachable at scroll_y = 10 (h - inner.height + 1).
        // The old formula (h - pane_height = 7) left rows 28-30
        // permanently unviewable.
        let mut old = String::new();
        let mut new = String::new();
        for i in 1..=29 {
            old.push_str(&format!("L{i:02}\n"));
            new.push_str(&format!("L{i:02}\n"));
        }
        old.push_str("L30\n");
        new.push_str("X30\n"); // only line 30 differs (col 0: 'L' vs 'X')
        let (mut app, a, b) = files_app(&old, &new);
        let _ = draw(&mut app); // sets pane_height from the real rect (23)
        assert_eq!(app.max_scroll_y(), 10);
        app.scroll_y = 10;
        let backend = draw(&mut app);
        let buf = backend.buffer();
        // Last content row "L|X|3|0" (line 30, old side 'L' / new side 'X')
        // sits at the bottom row of the pane: y = 21.
        assert_eq!(buf[(1, 21)].symbol(), "L");
        assert_eq!(buf[(3, 21)].symbol(), "X");
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    fn max_scroll_x_reaches_last_columns() {
        // One 100-char row: the bordered pane's inner width is 78
        // (pane_width - 2), so the row scrolls to 22. The old formula
        // (w - pane_width = 20) left the last 2 columns unviewable.
        let old = "a".repeat(100);
        let new = "a".repeat(100); // identical single-line files: width 100
        let (mut app, a, b) = files_app(&old, &new);
        let _ = draw(&mut app); // sets pane_width from the real rect (80)
        assert_eq!(app.max_scroll_x(), 22);
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    use crossterm::event::{KeyCode, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn q_and_ctrl_c_quit() {
        let (mut app, a, b) = files_app("a\n", "b\n");
        assert!(app.handle_key(key(KeyCode::Char('q'))));
        assert!(app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)));
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    fn hjkl_scroll_diff() {
        let (mut app, a, b) = files_app(
            "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12\n13\n14\n15\n16\n17\n18\n19\n20\n",
            "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12\n13\n14\n15\n16\n17\n18\n19\n21\n",
        );
        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(app.scroll_y, 1);
        app.handle_key(key(KeyCode::Char('k')));
        assert_eq!(app.scroll_y, 0);
        app.handle_key(key(KeyCode::Char('l')));
        assert_eq!(app.scroll_x, 1);
        app.handle_key(key(KeyCode::Char('h')));
        assert_eq!(app.scroll_x, 0);
        // vim convention: g = top, G = bottom.
        app.scroll_y = 3;
        app.handle_key(key(KeyCode::Char('g')));
        assert_eq!(app.scroll_y, 0);
        app.handle_key(key(KeyCode::Char('G')));
        assert_eq!(app.scroll_y, app.max_scroll_y());
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    fn tab_and_backtab_move_selection() {
        let mut app = git_app(vec![("foo\n", "bar\n"), ("x\n", "y\n")]);
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.selection, 1);
        app.handle_key(key(KeyCode::BackTab));
        assert_eq!(app.selection, 0);
        // j/k scroll the diff — they never touch the selection.
        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(app.scroll_y, 1);
        assert_eq!(app.selection, 0);
        app.handle_key(key(KeyCode::Char('k')));
        assert_eq!(app.scroll_y, 0);
        assert_eq!(app.selection, 0);
    }

    #[test]
    fn jk_scrolls_diff_in_git_mode_without_moving_selection() {
        let mut app = git_app(vec![
            (
                "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12\n13\n14\n15\n16\n17\n18\n19\n20\n",
                "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12\n13\n14\n15\n16\n17\n18\n19\n21\n",
            ),
            ("x\n", "y\n"),
        ]);
        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(app.scroll_y, 1, "j must scroll the diff in git mode");
        assert_eq!(app.selection, 0, "j must not move the selection");
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.selection, 1, "Tab moves the selection");
        assert_eq!(
            app.scroll_y, 0,
            "selection change reloads and resets scroll"
        );
    }

    #[test]
    fn selection_change_reloads_diff() {
        let mut app = git_app(vec![
            ("foo\nbar baz\nquux\n", "foo\nbar qaz\nquux\n"),
            ("x\n", "y\n"),
        ]);
        let before = app.grid.clone();
        app.handle_key(key(KeyCode::Tab));
        let after = app.grid.clone();
        assert_ne!(before, after, "diff must change when the selection moves");
    }

    #[test]
    fn t_toggles_transposed_and_resets_scrolls() {
        let (mut app, a, b) = files_app("foo\nbar baz\nquux\n", "foo\nbar qaz\nquux\n");
        app.scroll_y = 2;
        app.scroll_x = 3;
        app.handle_key(key(KeyCode::Char('t')));
        assert!(app.transposed);
        assert_eq!(app.scroll_y, 0);
        assert_eq!(app.scroll_x, 0);
        app.handle_key(key(KeyCode::Char('t')));
        assert!(!app.transposed);
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    fn e_toggles_sidebar_only_in_git_mode() {
        let mut app = git_app(vec![("a\n", "b\n")]);
        assert!(app.show_sidebar);
        app.handle_key(key(KeyCode::Char('e')));
        assert!(!app.show_sidebar);
        app.handle_key(key(KeyCode::Char('e')));
        assert!(app.show_sidebar);
        let (mut files_app, a, b) = files_app("a\n", "b\n");
        files_app.handle_key(key(KeyCode::Char('e'))); // no sidebar in files mode
        assert!(files_app.show_sidebar);
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    fn n_p_jump_to_change_groups() {
        let (mut app, a, b) = files_app("foo\nbar baz\nquux\n", "foo\nbar qaz\nquux\n");
        app.handle_key(key(KeyCode::Char('n')));
        // change group (del+ins) starts at display width 7 (4 steps × 1 cell + 3 separators)
        assert_eq!(app.scroll_x, 7);
        assert_eq!(app.scroll_y, 1); // first affected row is line 1 ("bar ...")
        app.handle_key(key(KeyCode::Char('p')));
        assert_eq!(app.scroll_x, 0);
        assert_eq!(app.scroll_y, 0);
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }
}
