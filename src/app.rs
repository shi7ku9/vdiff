//! TUI rendering helpers and the ratatui App.
//!
//! The diff grid is rendered as display rows: `build_rows` turns a
//! `DiffGrid` into full-width strings (marker row first), and
//! `slice_by_width` implements width-aware horizontal scrolling so
//! grapheme clusters (CJK, combining marks, emoji) are never split.

use crate::diff::{self, DiffGrid, StepKind};
use crate::git::{self, ChangedFile, GitShell, RevSpec, Status};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use std::fmt::Write as _;
use std::io;
use std::path::PathBuf;
use unicode_segmentation::UnicodeSegmentation;

/// Display rows for the TUI: rows[0] is the marker row (one marker per
/// display block, `|` between groups, padded to the full content
/// display width), rows[i+1] is file line i. All rows share the same
/// column boundaries. Adjacent steps that belong to a grapheme in any
/// row are kept together and padded after the complete grapheme.
pub fn build_rows(grid: &DiffGrid) -> Vec<String> {
    build_display(grid).rows
}

fn cell_style(kind: Option<StepKind>) -> Style {
    let neutral = Style::default().fg(Color::Reset).bg(Color::Reset);
    match kind {
        Some(StepKind::Match) => neutral,
        Some(StepKind::Delete) => neutral.fg(Color::Black).bg(Color::Red),
        Some(StepKind::Insert) => neutral.fg(Color::Black).bg(Color::Green),
        None => neutral.fg(Color::DarkGray),
    }
}

/// Cached display rows for the current grid, rebuilt once per reload.
struct Display {
    rows: Vec<String>,
    /// Shared diff kind per display cell; `None` denotes a separator.
    kinds: Vec<Option<StepKind>>,
    /// Actual end offsets, including the following separator if present.
    /// Change navigation uses the same layout as the displayed columns.
    group_end_offsets: Vec<usize>,
    /// Rendered width, common to the marker and every content row.
    width: usize,
}

fn build_display(grid: &DiffGrid) -> Display {
    let mut marker = String::new();
    let mut rows: Vec<String> = (0..grid.height).map(|_| String::new()).collect();
    let mut kinds = Vec::new();
    let mut group_end_offsets = Vec::with_capacity(grid.groups.len());
    for group in &grid.groups {
        let kind = grid.steps[group.start].kind;
        let mark = match kind {
            StepKind::Match => ' ',
            StepKind::Delete => '-',
            StepKind::Insert => '+',
        };
        let mut group_rows: Vec<String> = (0..grid.height).map(|_| String::new()).collect();
        for i in group.clone() {
            for (r, c) in grid.steps[i].content.chars().enumerate() {
                group_rows[r].push(c);
            }
        }

        // A step boundary is usable only if it is a grapheme boundary in
        // every row. Taking the union of all joined steps preserves both
        // complete glyphs and one shared layout across unlike file lines.
        let mut boundaries = vec![true; group.len() + 1];
        for text in &group_rows {
            let mut step = 0;
            // Keep raw scalar offsets here: styled_graphemes filters
            // controls, but row_chars below consumes the original text.
            for grapheme in text.graphemes(true) {
                let end = step + grapheme.chars().count();
                boundaries[step + 1..end].fill(false);
                step = end;
            }
        }
        let mut row_chars: Vec<_> = group_rows.iter().map(|text| text.chars()).collect();
        let mut start = 0;
        for (end, &boundary) in boundaries.iter().enumerate().skip(1) {
            if !boundary {
                continue;
            }
            let blocks: Vec<_> = row_chars
                .iter_mut()
                .map(|chars| {
                    let text: String = chars.by_ref().take(end - start).collect();
                    let text = visible_block(&text, kind);
                    let width = rendered_width(&text);
                    (text, width)
                })
                .collect();
            // Reserve only the widest rendered block across file lines.
            // Combining marks and ZWJ scalars do not add phantom cells.
            let width = blocks.iter().map(|(_, width)| *width).max().unwrap_or(0);
            if width > 0 {
                marker.push(mark);
                marker.extend(std::iter::repeat_n(' ', width - 1));
            }
            kinds.extend(std::iter::repeat_n(Some(kind), width));
            for (row, (text, actual_width)) in rows.iter_mut().zip(blocks) {
                row.push_str(&text);
                row.extend(std::iter::repeat_n(' ', width - actual_width));
            }
            start = end;
        }

        if group.end < grid.steps.len() {
            marker.push('|');
            kinds.push(None);
            for row in &mut rows {
                row.push('|');
            }
        }
        group_end_offsets.push(kinds.len());
    }
    let mut out = vec![marker];
    out.extend(rows);
    let width = kinds.len();
    Display {
        rows: out,
        kinds,
        group_end_offsets,
        width,
    }
}

/// Give standalone zero-width glyphs a base so they cannot attach to
/// separators or padding. Filter controls only after raw layout boundaries
/// have been recorded; control-only edits still need a visible indication.
fn visible_block(text: &str, kind: StepKind) -> String {
    let mut visible = String::with_capacity(text.len());
    for grapheme in text.graphemes(true) {
        if grapheme.chars().any(char::is_control) {
            // Keep raw boundaries until drawing. Removing a control here
            // could join glyphs on either side into a different grapheme.
            visible.push_str(grapheme);
            continue;
        }
        if Span::raw(grapheme).width() == 0 {
            visible.push('◌');
        }
        visible.push_str(grapheme);
    }
    if rendered_width(&visible) == 0 && kind != StepKind::Match {
        visible.clear();
        for c in text.chars() {
            if !visible.is_empty() {
                visible.push(' ');
            }
            write!(visible, "U+{:04X}", u32::from(c)).unwrap();
        }
    }
    visible
}

/// Use the same grapheme widths and control filtering as the renderer.
fn rendered_width(text: &str) -> usize {
    rendered_graphemes(text).map(|(_, width)| width).sum()
}

fn rendered_graphemes(text: &str) -> impl Iterator<Item = (&str, usize)> {
    text.graphemes(true)
        .filter(|grapheme| !grapheme.chars().any(char::is_control))
        .map(|grapheme| (grapheme, Span::raw(grapheme).width()))
        .filter(|(_, width)| *width > 0)
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
                if !git::in_repo(&*g)? {
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

/// Outcome of handling one key event.
pub struct KeyResult {
    /// True when the app should exit the event loop.
    pub quit: bool,
    /// True when the visible state changed and the screen must redraw.
    /// Unknown keys and no-op scrolls (e.g. `h` at column 0) report
    /// `changed: false` so the loop can skip the redraw.
    pub changed: bool,
}

pub struct App {
    pub source: DiffSource,
    pub selection: usize,
    pub show_sidebar: bool,
    pub scroll_y: usize,
    pub scroll_x: usize,
    pub grid: Option<DiffGrid>,
    /// Cached display rows for the current grid (see `Display`);
    /// rebuilt in `reload`.
    display: Option<Display>,
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
            scroll_y: 0,
            scroll_x: 0,
            grid: None,
            display: None,
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

    fn entries(&self) -> &[ChangedFile] {
        match &self.source {
            DiffSource::Git { files, .. } => files,
            DiffSource::Files { .. } => &[],
        }
    }

    fn reload(&mut self) {
        let loaded: Result<Option<(String, String)>, String> = match &self.source {
            DiffSource::Files { old, new } => {
                match (std::fs::read_to_string(old), std::fs::read_to_string(new)) {
                    (Ok(a), Ok(b)) => Ok(Some((a, b))),
                    _ => Ok(None),
                }
            }
            DiffSource::Git { files, .. } if files.is_empty() => Ok(None),
            DiffSource::Git { files, git, spec } => {
                let idx = self.selection.min(files.len().saturating_sub(1));
                git::load_content(&**git, spec, &files[idx]).map_err(|e| e.to_string())
            }
        };
        self.scroll_y = 0;
        self.scroll_x = 0;
        match loaded {
            Ok(Some((old, new))) => {
                let grid = diff::compute(&old, &new);
                self.degraded = grid.degraded;
                // Display rows depend only on the grid; build them
                // once here instead of on every frame.
                self.display = Some(build_display(&grid));
                self.grid = Some(grid);
                self.message = None;
            }
            Ok(None) => {
                self.grid = None;
                self.display = None;
                self.degraded = false;
                self.message = Some(match &self.source {
                    DiffSource::Git { files, .. } if files.is_empty() => "no changes".to_string(),
                    _ => "binary or unreadable".to_string(),
                });
            }
            Err(message) => {
                self.grid = None;
                self.display = None;
                self.degraded = false;
                self.message = Some(message);
            }
        }
    }

    fn content_dims(&self) -> (usize, usize) {
        let Some(grid) = &self.grid else {
            return (0, 0);
        };
        // Rows and their display width are precomputed in
        // `build_display`; the height is the grid height.
        let Some(disp) = &self.display else {
            return (0, 0);
        };
        (grid.height, disp.width)
    }

    fn max_scroll_y(&self) -> usize {
        let (h, _) = self.content_dims();
        // The bordered diff pane's inner area is pane_height - 2 rows;
        // the sticky marker row occupies one of them, so pane_height -
        // 3 content rows are visible.
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
        // Record the pane size before rendering the status bar so the
        // scroll clamps are correct on the very first frame (and not
        // only after a state-changing key triggers a redraw).
        self.pane_width = diff_area.width as usize;
        self.pane_height = diff_area.height as usize;
        // A resize or a sidebar toggle can shrink the pane below the
        // current scroll; re-clamp here so every frame heals it (a
        // stale scroll_y after `G` + enlarge would blank the pane).
        self.scroll_y = self.scroll_y.min(self.max_scroll_y());
        self.scroll_x = self.scroll_x.min(self.max_scroll_x());
        if let Some(area) = sidebar_area {
            self.render_sidebar(frame, area);
        }
        self.render_diff(frame, diff_area);
        self.render_status(frame, chunks[1]);
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
            (Some(_), _) => {
                // Rows are cached in `self.display` (see `build_display`);
                // they are built once per reload, not per frame.
                let Some(disp) = &self.display else {
                    return;
                };
                // Color the full visible column, including the empty area
                // below the file. Row drawing restores continuation-cell
                // styles after ratatui writes wide characters.
                for (offset, kind) in disp
                    .kinds
                    .iter()
                    .skip(self.scroll_x)
                    .take(usize::from(inner.width))
                    .enumerate()
                {
                    frame.buffer_mut().set_style(
                        Rect::new(inner.x + offset as u16, inner.y, 1, inner.height),
                        cell_style(*kind),
                    );
                }
                let rows = &disp.rows;
                // Row windows are computed in usize: u16 would wrap
                // around for diffs taller than 65535 rows and blank
                // out the bottom. `y` is in the frame by the time it
                // is cast.
                let top = usize::from(inner.y);
                let bottom = top + usize::from(inner.height);
                if let Some(marker) = rows.first() {
                    self.draw_row(frame, inner, inner.y, marker, &disp.kinds);
                }
                for (i, row) in rows.iter().enumerate().skip(1) {
                    let y = top.saturating_add(i).saturating_sub(self.scroll_y);
                    // Content rows live below the sticky marker row:
                    // y == top is the marker's row and must not be
                    // overwritten once the user scrolls.
                    if y <= top {
                        continue;
                    }
                    if y >= bottom || y >= usize::from(u16::MAX) {
                        break;
                    }
                    self.draw_row(frame, inner, y as u16, row, &disp.kinds);
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
        kinds: &[Option<StepKind>],
    ) {
        let scroll_x = self.scroll_x;
        let right = scroll_x + usize::from(inner.width);
        let mut pos = 0;
        // Draw complete graphemes at their cached layout positions. Do not
        // concatenate across filtered controls and re-segment the result.
        for (grapheme, width) in rendered_graphemes(row) {
            let end = pos + width;
            if end > right {
                break;
            }
            if pos >= scroll_x {
                frame.buffer_mut().set_string(
                    inner.x + (pos - scroll_x) as u16,
                    y,
                    grapheme,
                    Style::default(),
                );
            }
            pos = end;
        }
        // Restore styles in wide-glyph continuation and clipped cells.
        for (offset, kind) in kinds
            .iter()
            .skip(scroll_x)
            .take(usize::from(inner.width))
            .enumerate()
        {
            frame.buffer_mut()[(inner.x + offset as u16, y)].set_style(cell_style(*kind));
        }
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
            "tab/⇧tab select · hjkl scroll · n/p changes · e sidebar · q quit",
        ));
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    /// Handle one key event. Returns whether the app should quit and
    /// whether the visible state changed (only then must the screen be
    /// redrawn).
    pub fn handle_key(&mut self, key: KeyEvent) -> KeyResult {
        // Uppercase C too: some terminals report Ctrl+Shift+C as
        // Char('C') + CONTROL|SHIFT (kitty protocol).
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
        {
            return KeyResult {
                quit: true,
                changed: false,
            };
        }
        // Ctrl/Alt chords would otherwise fall through to the plain
        // arms (Ctrl+Q would quit, Ctrl+L would scroll right); only
        // Shift is allowed (Shift+G is how `G` arrives).
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return KeyResult {
                quit: false,
                changed: false,
            };
        }
        let before = (
            self.scroll_y,
            self.scroll_x,
            self.selection,
            self.show_sidebar,
        );
        match key.code {
            KeyCode::Char('q') => {
                return KeyResult {
                    quit: true,
                    changed: false,
                };
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
        let after = (
            self.scroll_y,
            self.scroll_x,
            self.selection,
            self.show_sidebar,
        );
        KeyResult {
            quit: false,
            changed: before != after,
        }
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
        // Anchor row of each change group: the first affected file row.
        let ranges: Vec<std::ops::Range<usize>> = change_groups
            .iter()
            .map(|&i| grid.groups[i].clone())
            .collect();
        let rows: Vec<usize> = ranges
            .iter()
            .map(|g| first_affected_row(grid, g).unwrap_or(usize::MAX))
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
        let Some(grid) = &self.grid else { return };
        if let (Some(row), Some(display)) = (first_affected_row(grid, &group), &self.display) {
            let group_index = grid.groups.partition_point(|g| g.start < group.start);
            self.scroll_y = row.min(self.max_scroll_y());
            self.scroll_x = display.group_end_offsets[group_index].min(self.max_scroll_x());
        }
    }
}

/// The first file line where any step in `group` has a non-padding
/// char. Each step's string is scanned once: `char_indices().find`
/// lands on the first non-space char, whose index is the row.
fn first_affected_row(grid: &DiffGrid, group: &std::ops::Range<usize>) -> Option<usize> {
    grid.steps[group.clone()]
        .iter()
        .filter_map(|s| {
            s.content
                .char_indices()
                .enumerate()
                .find(|(_, (_, c))| *c != ' ')
                .map(|(n, _)| n)
        })
        .min()
}

pub fn run_tui(cli: &crate::cli::Cli) -> Result<(), Box<dyn std::error::Error>> {
    let source = DiffSource::from_cli(cli)?;
    let mut app = App::new(source);
    run_tui_app(&mut app)?;
    Ok(())
}

/// Drive the terminal loop until the app signals quit. Redraws only
/// when a key actually changed the state (or the terminal resized),
/// instead of unconditionally on every event.
pub fn run_tui_app(app: &mut App) -> io::Result<()> {
    let mut terminal = ratatui::init();
    let result = (|| {
        terminal.draw(|frame| app.render(frame))?;
        loop {
            match event::read()? {
                Event::Key(key) => {
                    let r = app.handle_key(key);
                    if r.quit {
                        break;
                    }
                    if r.changed {
                        terminal.draw(|frame| app.render(frame))?;
                    }
                }
                Event::Resize(_, _) => {
                    terminal.draw(|frame| app.render(frame))?;
                }
                _ => {}
            }
        }
        Ok(())
    })();
    ratatui::restore();
    result
}

/// Slice `s` by display width: skip the first `start` cells, keep at
/// most `max_width` cells. A grapheme that straddles a boundary is
/// skipped whole, so output never contains a partial grapheme. Returns
/// the drawn text plus the display cell index of its first grapheme. That
/// index sits past `start` when a straddling wide grapheme was skipped,
/// so the caller can keep rows aligned under the same viewport
/// columns.
pub fn slice_by_width(s: &str, start: usize, max_width: usize) -> (String, usize) {
    let mut out = String::new();
    let mut first = start;
    let mut pos = 0usize;
    for (grapheme, w) in rendered_graphemes(s) {
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
            out.push_str(grapheme);
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
    fn unicode_graphemes_do_not_reserve_extra_scalar_cells() {
        for prefix in ["a\u{301}", "a\u{301}\u{308}", "👩‍❤️‍👩", "👩‍❤️‍💋‍👩"]
        {
            let grid = compute(&format!("{prefix}x\n"), &format!("{prefix}y\n"));
            let display = build_display(&grid);
            assert_eq!(display.rows[1], format!("{prefix}|x|y"));
            assert_eq!(
                display.rows[0],
                format!("{}|-|+", " ".repeat(rendered_width(prefix)))
            );
            assert_eq!(display.width, rendered_width(prefix) + 4);
        }
    }

    #[test]
    fn combining_only_edits_keep_visible_markers_and_backgrounds() {
        let grid = compute("e\u{301}\n", "e\u{308}\n");
        let display = build_display(&grid);
        assert_eq!(display.rows, [" |-|+", "e|◌\u{301}|◌\u{308}"]);
        assert_eq!(display.group_end_offsets, [2, 4, 5]);
        let (mut app, a, b) = files_app("e\u{301}\n", "e\u{308}\n");
        let buf = draw_buffer(&mut app);
        assert_column_backgrounds_align(&buf);
        assert_eq!(buf[(3, 1)].symbol(), "-");
        assert_eq!(buf[(3, 2)].symbol(), "◌\u{301}");
        assert_eq!(buf[(3, 2)].bg, Color::Red);
        assert_eq!(buf[(5, 1)].symbol(), "+");
        assert_eq!(buf[(5, 2)].symbol(), "◌\u{308}");
        assert_eq!(buf[(5, 2)].bg, Color::Green);
        for x in [2, 4] {
            assert_eq!(buf[(x, 2)].symbol(), "|");
            assert_eq!(buf[(x, 2)].bg, Color::Reset);
        }
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    fn control_prefix_keeps_raw_grapheme_boundaries() {
        let text = "\ra\u{301}x\n中bcx\n";
        let display = build_display(&compute(text, text));
        assert!(
            display.rows[1].contains("a\u{301}"),
            "{:?}",
            display.rows[1]
        );
        assert_eq!(
            slice_by_width(&display.rows[1], 0, display.width).0,
            "  a\u{301} x"
        );
        assert_eq!(display.rows[2], "中bcx");
        for row in &display.rows {
            assert_eq!(rendered_width(row), display.width);
        }
        let (mut app, a, b) = files_app(text, text);
        let buf = draw_buffer(&mut app);
        assert_eq!(buf[(3, 2)].symbol(), "a\u{301}");
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    fn pure_combining_additions_and_deletions_are_visible() {
        for (old, new, color, mark) in [
            ("", "\u{301}\n", Color::Green, "+"),
            ("\u{301}\n", "", Color::Red, "-"),
        ] {
            let (mut app, a, b) = files_app(old, new);
            let buf = draw_buffer(&mut app);
            assert_column_backgrounds_align(&buf);
            assert_eq!(buf[(1, 1)].symbol(), mark);
            assert_eq!(buf[(1, 2)].symbol(), "◌\u{301}");
            assert_eq!(buf[(1, 2)].bg, color);
            assert_eq!(buf[(1, 21)].bg, color);
            let _ = std::fs::remove_file(&a);
            let _ = std::fs::remove_file(&b);
        }
    }

    #[test]
    fn orphan_marks_are_visible_when_other_rows_have_nonzero_width() {
        let (mut app, a, b) = files_app("e\u{301}\nex\n", "e\u{308}\ney\n");
        let display = app.display.as_ref().unwrap();
        assert_eq!(display.rows, [" |-|+", "e|◌\u{301}|◌\u{308}", "e|x|y"]);
        let buf = draw_buffer(&mut app);
        assert_column_backgrounds_align(&buf);
        assert_eq!(buf[(3, 2)].symbol(), "◌\u{301}");
        assert_eq!(buf[(5, 2)].symbol(), "◌\u{308}");
        for y in [2, 3] {
            assert_eq!(buf[(3, y)].bg, Color::Red);
            assert_eq!(buf[(5, y)].bg, Color::Green);
            assert_eq!(buf[(2, y)].symbol(), "|");
            assert_eq!(buf[(4, y)].symbol(), "|");
        }
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    fn multiple_controls_preserve_combining_and_zwj_boundaries() {
        let text = "\r\u{7}a\u{301}x\n中abcx\n";
        let display = build_display(&compute(text, text));
        assert_eq!(
            slice_by_width(&display.rows[1], 0, display.width).0,
            "   a\u{301} x"
        );
        assert_eq!(display.rows[2], "中abcx");
        for row in &display.rows {
            assert_eq!(rendered_width(row), display.width);
        }

        let prefix = "\r\u{7}👩‍❤️‍👩\r";
        let grid = compute(&format!("{prefix}x\n"), &format!("{prefix}y\n"));
        let display = build_display(&grid);
        assert_eq!(display.rows[0], "  |-|+");
        assert_eq!(
            slice_by_width(&display.rows[1], 0, display.width).0,
            "👩‍❤️‍👩|x|y"
        );
        assert_eq!(display.width, 6);
        let (mut app, a, b) = files_app(&format!("{prefix}x\n"), &format!("{prefix}y\n"));
        let buf = draw_buffer(&mut app);
        assert_column_backgrounds_align(&buf);
        assert_eq!(buf[(1, 2)].symbol(), "👩‍❤️‍👩");
        assert_eq!(buf[(4, 2)].symbol(), "x");
        assert_eq!(buf[(6, 2)].symbol(), "y");
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    fn filtered_controls_do_not_join_neighboring_graphemes() {
        let prefix = "🇺\r🇸";
        let (mut app, a, b) = files_app(&format!("{prefix}x\n"), &format!("{prefix}y\n"));
        let left_width = Span::raw("🇺").width() as u16;
        let right_width = Span::raw("🇸").width() as u16;
        let delete_x = left_width + right_width + 2;
        assert!(app.display.as_ref().unwrap().rows[1].contains('\r'));
        let buf = draw_buffer(&mut app);
        assert_column_backgrounds_align(&buf);
        assert_eq!(buf[(1, 2)].symbol(), "🇺");
        assert_eq!(buf[(1 + left_width, 2)].symbol(), "🇸");
        assert_eq!(buf[(delete_x, 2)].symbol(), "x");
        assert_eq!(buf[(delete_x, 2)].bg, Color::Red);
        assert_eq!(buf[(delete_x + 2, 2)].symbol(), "y");
        assert_eq!(buf[(delete_x + 2, 2)].bg, Color::Green);
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    fn control_only_edits_show_code_points() {
        // A suffix keeps the CR inside the line, outside CRLF normalization.
        let grid = compute("\rx", "\u{7}x");
        let display = build_display(&grid);
        assert_eq!(display.rows, ["-     |+     | ", "U+000D|U+0007|x"]);
        assert_eq!(display.group_end_offsets, [7, 14, 15]);
        let (mut app, a, b) = files_app("\rx", "\u{7}x");
        let buf = draw_buffer(&mut app);
        assert_column_backgrounds_align(&buf);
        for x in 1..=6 {
            assert_eq!(buf[(x, 2)].bg, Color::Red);
        }
        for x in 8..=13 {
            assert_eq!(buf[(x, 2)].bg, Color::Green);
        }
        assert_eq!(buf[(7, 2)].symbol(), "|");
        assert_eq!(buf[(7, 2)].bg, Color::Reset);
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    fn dotted_circle_edits_survive_navigation_scrolling_and_resize() {
        let suffix = "z".repeat(40);
        let (mut app, a, b) = files_app(
            &format!("e\u{301}{suffix}\n"),
            &format!("e\u{308}{suffix}\n"),
        );
        app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert_eq!(app.scroll_x, 4);
        let mut terminal = Terminal::new(TestBackend::new(12, 6)).unwrap();
        let buf = terminal.draw(|frame| app.render(frame)).unwrap().buffer;
        assert_column_backgrounds_align(buf);
        assert_eq!(buf[(1, 1)].symbol(), "+");
        assert_eq!(buf[(1, 2)].symbol(), "◌\u{308}");
        assert_eq!(buf[(1, 2)].bg, Color::Green);
        app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE));
        assert_eq!(app.scroll_x, 0);
        let buf = draw_buffer(&mut app);
        assert_column_backgrounds_align(&buf);
        assert_eq!(buf[(3, 2)].symbol(), "◌\u{301}");
        assert_eq!(buf[(5, 2)].symbol(), "◌\u{308}");
        app.scroll_x = 2;
        let buf = terminal.draw(|frame| app.render(frame)).unwrap().buffer;
        assert_column_backgrounds_align(buf);
        assert_eq!(buf[(1, 2)].symbol(), "◌\u{301}");
        assert_eq!(buf[(3, 2)].symbol(), "◌\u{308}");
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
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

    #[test]
    fn slice_keeps_combining_and_zwj_graphemes_whole() {
        assert_eq!(slice_by_width("a\u{301}x", 0, 1), ("a\u{301}".into(), 0));
        assert_eq!(slice_by_width("a\u{301}x", 1, 1), ("x".into(), 1));
        let family = "👩‍❤️‍👩";
        let text = format!("{family}x");
        assert_eq!(slice_by_width(&text, 0, 2), (family.into(), 0));
        assert_eq!(slice_by_width(&text, 0, 1), (String::new(), 0));
        assert_eq!(slice_by_width(&text, 1, 2), ("x".into(), 2));
        assert_eq!(slice_by_width(&text, 2, 1), ("x".into(), 2));
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
            toplevel: String::new(),
            prefix: String::new(),
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

    // The backend skips wide-character continuation cells. Inspect the
    // completed frame to check their styles as well as the printed glyphs.
    fn draw_buffer(app: &mut App) -> ratatui::buffer::Buffer {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| app.render(frame))
            .unwrap()
            .buffer
            .clone()
    }

    fn assert_column_backgrounds_align(buf: &ratatui::buffer::Buffer) {
        for x in 1..buf.area.width - 1 {
            for y in 2..buf.area.height - 2 {
                assert_eq!(buf[(x, y)].bg, buf[(x, 1)].bg, "column {x}, row {y}");
            }
        }
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
        assert_eq!(buf[(6, 1)].fg, Color::Black);
        assert_eq!(buf[(6, 1)].bg, Color::Red);
        assert_eq!(buf[(8, 1)].symbol(), "+");
        assert_eq!(buf[(8, 1)].fg, Color::Black);
        assert_eq!(buf[(8, 1)].bg, Color::Green);
        assert_eq!(buf[(5, 1)].symbol(), "|");
        for y in 2..=4 {
            assert_eq!(buf[(6, y)].bg, Color::Red);
            assert_eq!(buf[(8, y)].bg, Color::Green);
            assert_eq!(buf[(6, y)].fg, Color::Black);
            assert_eq!(buf[(8, y)].fg, Color::Black);
            assert_eq!(buf[(1, y)].bg, Color::Reset);
            assert_eq!(buf[(5, y)].bg, Color::Reset);
            assert_eq!(buf[(5, y)].fg, Color::DarkGray);
        }
        for y in 5..=21 {
            assert_eq!(buf[(6, y)].bg, Color::Red);
            assert_eq!(buf[(8, y)].bg, Color::Green);
            assert_eq!(buf[(5, y)].bg, Color::Reset);
            assert_eq!(buf[(12, y)].bg, Color::Reset);
        }
        assert_eq!(buf[(6, 22)].bg, Color::Reset); // bottom border
        assert_eq!(buf[(6, 22)].symbol(), "─");
        assert_eq!(buf[(6, 23)].bg, Color::Reset); // status bar
        assert_eq!(buf[(12, 2)].bg, Color::Reset);
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    fn cjk_separators_align_in_tui() {
        let (mut app, a, b) = files_app("中文abc\n", "中日abc\n");
        let buf = draw_buffer(&mut app);
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
        for y in 1..=2 {
            for x in [4, 5] {
                assert_eq!(buf[(x, y)].bg, Color::Red);
            }
            for x in [7, 8] {
                assert_eq!(buf[(x, y)].bg, Color::Green);
            }
        }
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    fn zwj_cluster_stays_glued_in_wide_column() {
        // The ZWJ's column is 2 cells wide (the 中 line); a pad space
        // after the ZWJ would push the following emoji apart.
        let s = "👩‍❤️‍👩\n中中\n";
        let (mut app, a, b) = files_app(s, s);
        let backend = draw(&mut app);
        let buf = backend.buffer();
        // ratatui stores the whole grapheme cluster in one cell; a pad
        // space between the ZWJ and the following emoji would split it
        // into separate symbols.
        assert_eq!(buf[(1, 2)].symbol(), "👩‍❤️‍👩");
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    fn colors_padding_in_wide_changed_columns() {
        let (mut app, a, b) = files_app("中文\na文\n", "中文\nb文\n");
        let buf = draw_buffer(&mut app);
        for y in 1..=3 {
            for x in [1, 2] {
                assert_eq!(buf[(x, y)].bg, Color::Red);
            }
            for x in [4, 5] {
                assert_eq!(buf[(x, y)].bg, Color::Green);
            }
            for x in [3, 6, 7, 8] {
                assert_eq!(buf[(x, y)].bg, Color::Reset);
            }
        }
        assert_eq!(buf[(2, 3)].symbol(), " ");
        assert_eq!(buf[(5, 3)].symbol(), " ");
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    fn colors_changes_after_combining_marks_and_zwj_graphemes() {
        for (prefix, prefix_width) in [
            ("a\u{301}", 1),
            ("a\u{301}\u{308}", 1),
            ("👩‍❤️‍👩", 2),
            ("👩‍❤️‍💋‍👩", 2),
        ] {
            let (mut app, a, b) = files_app(&format!("{prefix}x\n"), &format!("{prefix}y\n"));
            let buf = draw_buffer(&mut app);
            assert_column_backgrounds_align(&buf);
            assert_eq!(buf[(1, 2)].symbol(), prefix);
            for x in 1..=prefix_width {
                assert_eq!(buf[(x, 2)].bg, Color::Reset);
            }
            let delete_x = prefix_width + 2;
            let insert_x = prefix_width + 4;
            assert_eq!(buf[(delete_x, 2)].symbol(), "x");
            assert_eq!(buf[(delete_x, 1)].symbol(), "-");
            assert_eq!(buf[(delete_x, 2)].bg, Color::Red);
            assert_eq!(buf[(insert_x, 2)].symbol(), "y");
            assert_eq!(buf[(insert_x, 1)].symbol(), "+");
            assert_eq!(buf[(insert_x, 2)].bg, Color::Green);
            for x in [prefix_width + 1, prefix_width + 3] {
                assert_eq!(buf[(x, 2)].symbol(), "|");
                assert_eq!(buf[(x, 2)].fg, Color::DarkGray);
                assert_eq!(buf[(x, 2)].bg, Color::Reset);
            }
            assert_eq!(buf[(insert_x + 1, 2)].bg, Color::Reset);
            let _ = std::fs::remove_file(&a);
            let _ = std::fs::remove_file(&b);
        }
    }

    #[test]
    fn content_rows_share_grapheme_layout_boundaries() {
        let (mut app, a, b) = files_app("a\u{301}x\nabz\n", "a\u{301}y\nabz\n");
        let buf = draw_buffer(&mut app);
        assert_column_backgrounds_align(&buf);
        for (y, delete_x, insert_x, deleted, inserted) in [(2, 4, 6, "x", "y"), (3, 4, 6, "z", "z")]
        {
            assert_eq!(buf[(delete_x, y)].symbol(), deleted);
            assert_eq!(buf[(delete_x, y)].bg, Color::Red);
            assert_eq!(buf[(insert_x, y)].symbol(), inserted);
            assert_eq!(buf[(insert_x, y)].bg, Color::Green);
            assert_eq!(buf[(delete_x - 1, y)].bg, Color::Reset);
            assert_eq!(buf[(delete_x + 1, y)].bg, Color::Reset);
            assert_eq!(buf[(insert_x + 1, y)].bg, Color::Reset);
        }
        // Blank rows retain the marker's full-height column bands.
        assert_eq!(buf[(4, 21)].bg, Color::Red);
        assert_eq!(buf[(6, 21)].bg, Color::Green);
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    fn shared_blocks_merge_overlapping_graphemes_across_rows() {
        let grid = compute("a\u{301}bcx\nab\u{301}cx\n", "a\u{301}bcy\nab\u{301}cy\n");
        let display = build_display(&grid);
        assert_eq!(
            display.rows,
            ["   |-|+", "a\u{301}bc|x|y", "ab\u{301}c|x|y"]
        );
        for row in &display.rows {
            assert_eq!(rendered_width(row), display.width);
        }
        assert_eq!(display.width, *display.group_end_offsets.last().unwrap());
    }

    #[test]
    fn mixed_width_rows_pad_after_complete_graphemes() {
        for (old, new, prefix, delete_x) in [
            ("a\u{301}x\n中bx\n", "a\u{301}y\n中by\n", "a\u{301}", 5),
            ("👩‍❤️‍👩x\n中abx\n", "👩‍❤️‍👩y\n中aby\n", "👩‍❤️‍👩", 6),
        ] {
            let (mut app, a, b) = files_app(old, new);
            let buf = draw_buffer(&mut app);
            assert_column_backgrounds_align(&buf);
            assert_eq!(buf[(1, 2)].symbol(), prefix);
            for y in [2, 3] {
                assert_eq!(buf[(delete_x, y)].symbol(), "x");
                assert_eq!(buf[(delete_x + 2, y)].symbol(), "y");
                assert_eq!(buf[(delete_x - 1, y)].symbol(), "|");
                assert_eq!(buf[(delete_x + 1, y)].symbol(), "|");
            }
            let _ = std::fs::remove_file(&a);
            let _ = std::fs::remove_file(&b);
        }
    }

    #[test]
    fn unicode_alignment_survives_vertical_scroll() {
        let (mut app, a, b) = files_app(&"a\u{301}x\n".repeat(40), &"a\u{301}y\n".repeat(40));
        let _ = draw(&mut app);
        app.scroll_y = 17;
        let buf = draw_buffer(&mut app);
        assert_eq!(app.scroll_y, 17);
        assert_column_backgrounds_align(&buf);
        assert_eq!(buf[(3, 1)].symbol(), "-");
        assert_eq!(buf[(5, 1)].symbol(), "+");
        for y in [2, 21] {
            assert_eq!(buf[(1, y)].symbol(), "a\u{301}");
            assert_eq!(buf[(3, y)].symbol(), "x");
            assert_eq!(buf[(5, y)].symbol(), "y");
        }
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    fn unicode_alignment_survives_navigation_clipping_and_resize() {
        let prefix = "👩‍❤️‍👩".repeat(8);
        let (mut app, a, b) = files_app(&format!("{prefix}x\n"), &format!("{prefix}y\n"));
        let mut small = Terminal::new(TestBackend::new(12, 6)).unwrap();
        app.scroll_x = 1; // cut through the first emoji
        let buf = small.draw(|frame| app.render(frame)).unwrap().buffer;
        assert_column_backgrounds_align(buf);
        assert_eq!(buf[(1, 2)].symbol(), " ");
        assert_eq!(buf[(2, 2)].symbol(), "👩‍❤️‍👩");
        // The sixth emoji straddles the right edge and is omitted whole.
        assert_eq!(buf[(10, 2)].symbol(), " ");

        app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        let buf = small.draw(|frame| app.render(frame)).unwrap().buffer;
        assert_column_backgrounds_align(buf);
        assert_eq!(buf[(8, 1)].symbol(), "-");
        assert_eq!(buf[(8, 2)].symbol(), "x");
        assert_eq!(buf[(10, 1)].symbol(), "+");
        assert_eq!(buf[(10, 2)].symbol(), "y");
        app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE));
        assert_eq!(app.scroll_x, 0);
        app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert!(app.scroll_x > 0);
        let buf = draw_buffer(&mut app); // larger viewport reclamps scroll
        assert_eq!(app.scroll_x, 0);
        assert_column_backgrounds_align(&buf);
        assert_eq!(buf[(18, 1)].symbol(), "-");
        assert_eq!(buf[(18, 2)].symbol(), "x");
        assert_eq!(buf[(20, 1)].symbol(), "+");
        assert_eq!(buf[(20, 2)].symbol(), "y");
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    fn navigation_uses_compact_unicode_offsets_without_clamping() {
        let prefix = "👩‍❤️‍👩".repeat(8);
        let suffix = "z".repeat(40);
        let (mut app, a, b) = files_app(
            &format!("{prefix}x{suffix}\n"),
            &format!("{prefix}y{suffix}\n"),
        );
        app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert_eq!(app.scroll_x, 19); // 16 prefix cells + |x|
        let mut terminal = Terminal::new(TestBackend::new(12, 6)).unwrap();
        let buf = terminal.draw(|frame| app.render(frame)).unwrap().buffer;
        assert_column_backgrounds_align(buf);
        assert_eq!(
            app.scroll_x, 19,
            "trailing context must prevent scroll clamping"
        );
        assert_eq!(buf[(1, 1)].symbol(), "+");
        assert_eq!(buf[(1, 2)].symbol(), "y");
        assert_eq!(buf[(1, 2)].bg, Color::Green);
        app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE));
        assert_eq!(app.scroll_x, 0);
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    fn scrolled_unicode_changes_keep_their_backgrounds() {
        for (prefix, width, deleted_x, inserted_x) in [("a\u{301}", 6, 2, 4), ("👩‍❤️‍👩", 7, 3, 5)]
        {
            let (mut app, a, b) = files_app(&format!("{prefix}x\n"), &format!("{prefix}y\n"));
            let mut terminal = Terminal::new(TestBackend::new(width, 6)).unwrap();
            app.scroll_x = 1;
            let buf = terminal.draw(|frame| app.render(frame)).unwrap().buffer;
            assert_column_backgrounds_align(buf);
            assert_eq!(app.scroll_x, 1);
            assert_eq!(buf[(deleted_x, 2)].symbol(), "x");
            assert_eq!(buf[(deleted_x, 2)].bg, Color::Red);
            assert_eq!(buf[(inserted_x, 2)].symbol(), "y");
            assert_eq!(buf[(inserted_x, 2)].bg, Color::Green);
            for x in [deleted_x - 1, deleted_x + 1] {
                assert_eq!(buf[(x, 2)].symbol(), "|");
                assert_eq!(buf[(x, 2)].fg, Color::DarkGray);
                assert_eq!(buf[(x, 2)].bg, Color::Reset);
            }
            if width == 7 {
                assert_eq!(buf[(1, 2)].symbol(), " ");
                assert_eq!(buf[(1, 2)].bg, Color::Reset);
            }
            let _ = std::fs::remove_file(&a);
            let _ = std::fs::remove_file(&b);
        }
    }

    #[test]
    fn literal_pipe_uses_its_column_style() {
        for (old, new, color) in [("|\n", "x\n", Color::Red), ("x\n", "|\n", Color::Green)] {
            let (mut app, a, b) = files_app(old, new);
            let backend = draw(&mut app);
            let buf = backend.buffer();
            let x = if color == Color::Red { 1 } else { 3 };
            assert_eq!(buf[(x, 2)].symbol(), "|");
            assert_eq!(buf[(x, 2)].bg, color);
            assert_eq!(buf[(x, 2)].fg, Color::Black);
            assert_eq!(buf[(2, 2)].bg, Color::Reset);
            assert_eq!(buf[(2, 2)].fg, Color::DarkGray);
            let _ = std::fs::remove_file(&a);
            let _ = std::fs::remove_file(&b);
        }
        let (mut app, a, b) = files_app("|\n", "|\n");
        let backend = draw(&mut app);
        assert_eq!(backend.buffer()[(1, 2)].fg, Color::Reset);
        assert_eq!(backend.buffer()[(1, 2)].bg, Color::Reset);
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    fn colors_pure_additions_and_deletions() {
        for (old, new, color) in [("", "中\na\n", Color::Green), ("中\na\n", "", Color::Red)] {
            let (mut app, a, b) = files_app(old, new);
            let buf = draw_buffer(&mut app);
            let width = app.display.as_ref().unwrap().width as u16;
            for y in 1..=21 {
                for x in 1..=width {
                    assert_eq!(buf[(x, y)].bg, color);
                    assert_eq!(buf[(x, y)].fg, Color::Black);
                }
                assert_eq!(buf[(width + 1, y)].bg, Color::Reset);
            }
            assert_eq!(buf[(1, 22)].bg, Color::Reset); // bottom border
            assert_eq!(buf[(1, 23)].bg, Color::Reset); // status bar
            assert_eq!(buf[(1, 0)].bg, Color::Reset); // top border
            let _ = std::fs::remove_file(&a);
            let _ = std::fs::remove_file(&b);
        }
    }

    #[test]
    fn colors_wide_characters_clipped_at_viewport_edges() {
        let (mut app, a, b) = files_app(
            &format!("{}\n", "中文abc".repeat(10)),
            &format!("{}\n", "中日abc".repeat(10)),
        );
        let mut terminal = Terminal::new(TestBackend::new(6, 6)).unwrap();
        for (scroll, colors) in [
            (3, [Color::Red, Color::Red, Color::Reset, Color::Green]),
            (4, [Color::Red, Color::Reset, Color::Green, Color::Green]),
            (7, [Color::Green, Color::Reset, Color::Reset, Color::Reset]),
        ] {
            app.scroll_x = scroll;
            let buf = terminal.draw(|frame| app.render(frame)).unwrap().buffer;
            for y in [1, 2, 3] {
                for (offset, color) in colors.into_iter().enumerate() {
                    assert_eq!(
                        buf[(offset as u16 + 1, y)].bg,
                        color,
                        "scroll={scroll}, y={y}"
                    );
                }
            }
            let clipped_x = if scroll == 3 { 4 } else { 1 };
            assert_eq!(buf[(clipped_x, 2)].symbol(), " ");
        }
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    fn switching_files_clears_previous_backgrounds() {
        let mut app = git_app(vec![("a\u{301}x\n", "a\u{301}y\n"), ("same\n", "same\n")]);
        app.show_sidebar = false;
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();
        assert_eq!(terminal.backend().buffer()[(3, 2)].bg, Color::Red);
        assert_eq!(terminal.backend().buffer()[(5, 2)].bg, Color::Green);
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        terminal.draw(|frame| app.render(frame)).unwrap();
        assert_eq!(app.selection, 1);
        for y in 1..=21 {
            for x in 1..=6 {
                assert_eq!(terminal.backend().buffer()[(x, y)].bg, Color::Reset);
            }
        }
    }

    #[test]
    fn cjk_separators_stay_aligned_when_scrolled_into_wide_char() {
        // Content wider than the pane, so the clamp lets scroll_x in.
        let block = "中文abc"; // 9 cells
        let (mut app, a, b) = files_app(
            &format!("{}\n", block.repeat(10)),
            &format!("{}\n", block.replace("文", "日").repeat(10)),
        );
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
        // 40 rows in a 23-row pane: max_scroll_y is 20, so a scroll of
        // 10 survives the render clamp (with a 20-row diff the clamp
        // pins it to 0 and the test would pass vacuously).
        let mut old = String::new();
        let mut new = String::new();
        for i in 1..=39 {
            old.push_str(&format!("{i}\n"));
            new.push_str(&format!("{i}\n"));
        }
        old.push_str("L40\n");
        new.push_str("X40\n");
        let (mut app, a, b) = files_app(&old, &new);
        let _ = draw(&mut app); // sets pane_height from the real rect (23)
        assert_eq!(app.max_scroll_y(), 20);
        app.scroll_y = 10;
        let backend = draw(&mut app);
        let buf = backend.buffer();
        // Marker "-|+|" starts at x=1 (block border): '-' over the
        // delete column, '+' over the insert — still at the top row
        // after vertical scroll.
        assert_eq!(buf[(1, 1)].symbol(), "-");
        assert_eq!(buf[(3, 1)].symbol(), "+");
        for y in [1, 2, 21] {
            assert_eq!(buf[(1, y)].bg, Color::Red);
            assert_eq!(buf[(3, y)].bg, Color::Green);
        }
        assert_eq!(app.scroll_y, 10, "clamp must keep a valid scroll");
        // Line 30 sits on the pane's bottom row: '3' at col 0, '0' at
        // col 5 (delete|insert cells, then the '0' of "30").
        assert_eq!(buf[(1, 21)].symbol(), "3");
        assert_eq!(buf[(5, 21)].symbol(), "0");
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

    #[test]
    fn scroll_reclamps_after_pane_grows() {
        // `G` at the bottom of a small pane leaves scroll_y beyond the
        // window of a taller pane; render must re-clamp it, or the
        // whole pane renders blank.
        let mut old = String::new();
        let mut new = String::new();
        for i in 1..=50 {
            old.push_str(&format!("L{i:02}\n"));
            new.push_str(&format!("L{i:02}\n"));
        }
        old.push_str("L51\n");
        new.push_str("X51\n");
        let (mut app, a, b) = files_app(&old, &new);
        let mut small = Terminal::new(TestBackend::new(80, 5)).unwrap();
        small.draw(|frame| app.render(frame)).unwrap();
        app.handle_key(key(KeyCode::Char('G')));
        assert!(app.scroll_y > 0);
        let backend = draw(&mut app);
        assert_eq!(app.scroll_y, app.max_scroll_y(), "scroll must re-clamp");
        let buf = backend.buffer();
        assert_eq!(buf[(3, 21)].symbol(), "X", "bottom row visible");
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    fn scroll_x_reclamps_after_sidebar_toggle() {
        // The sidebar shrinks the pane; a scroll_x set for the
        // full-width layout must be clamped by the next render.
        let line = "a".repeat(200) + "\n";
        let mut app = git_app(vec![(&line, &line)]);
        let _ = draw(&mut app); // with sidebar: narrow pane
        app.scroll_x = 150; // beyond the sidebar layout's window
        let _ = draw(&mut app);
        assert_eq!(app.scroll_x, app.max_scroll_x());
    }

    use crossterm::event::{KeyCode, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn q_and_ctrl_c_quit() {
        let (mut app, a, b) = files_app("a\n", "b\n");
        assert!(app.handle_key(key(KeyCode::Char('q'))).quit);
        assert!(
            app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
                .quit
        );
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    fn ctrl_and_alt_chords_do_nothing() {
        // Without a modifier guard, Ctrl+Q would quit and Ctrl+L
        // would scroll right through the plain char arms.
        let (mut app, a, b) = files_app("a\n", "b\n");
        let r = app.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL));
        assert!(!r.quit, "Ctrl+Q must not quit");
        let r = app.handle_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL));
        assert_eq!(app.scroll_x, 0, "Ctrl+L must not scroll");
        assert!(!r.changed);
        let r = app.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::ALT));
        assert!(!r.quit, "Alt+Q must not quit");
        // Ctrl+Shift+C still quits (kitty sends it as Char('C')).
        let r = app.handle_key(KeyEvent::new(
            KeyCode::Char('C'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ));
        assert!(r.quit, "Ctrl+Shift+C must quit");
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

    #[test]
    fn group_end_offset_has_no_phantom_trailing_separator() {
        // Only the final column differs ("aa" → "ab"): steps Match,
        // Delete, Insert; the insert group ends at the last step, where
        // build_rows draws no trailing `|`. Navigation offsets must not
        // count a phantom separator after it (total width 3 steps × 1 +
        // 2 real separators = 5; without the guard it would be 6).
        let grid = compute("aa\n", "ab\n");
        let display = build_display(&grid);
        assert_eq!(display.group_end_offsets, [2, 4, 5]);
    }
}
