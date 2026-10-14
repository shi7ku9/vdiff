//! TUI rendering helpers and the ratatui App.
//!
//! The diff grid is rendered as display rows: `build_rows` turns a
//! `DiffGrid` into full-width strings (marker row first), and
//! `slice_by_width` implements width-aware horizontal scrolling so
//! grapheme clusters (CJK, combining marks, emoji) are never split.
//! The bars, sidebar, and help popup around the diff live in `chrome`.

mod chrome;

use crate::diff::{self, DiffGrid, StepKind};
use crate::git::{self, ChangedFile, GitShell, RevSpec};
use crate::theme::Theme;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::Span;
use std::borrow::Cow;
use std::fmt::Write as _;
use std::io;
use std::ops::Range;
use std::path::PathBuf;
use unicode_segmentation::UnicodeSegmentation;

/// Below this terminal width the file sidebar starts hidden.
const SIDEBAR_MIN_FRAME_WIDTH: u16 = 80;

/// Diff areas narrower than this leave out the line-number gutter.
const GUTTER_MIN_AREA_WIDTH: u16 = 20;

/// Panes shorter than this leave out the overview bar.
const OVERVIEW_MIN_PANE_HEIGHT: u16 = 8;

/// Rows and cells one mouse-wheel step scrolls.
const WHEEL_ROWS: usize = 3;
const WHEEL_CELLS: usize = 6;

/// Unchanged cells `n`/`p` leave to the left of the change they jump to.
const CHANGE_CONTEXT: usize = 3;

/// Display rows for the TUI: rows[0] is the marker row (one marker per
/// grapheme column, `|` between groups, padded to the full content
/// display width), rows[i+1] is file line i. All rows share the same
/// column boundaries. Each diff step is a complete grapheme column;
/// narrower cells are padded after their complete grapheme.
/// The TUI retains explicit cells rather than re-segmenting these joined strings.
pub fn build_rows(grid: &DiffGrid) -> Vec<String> {
    build_display(grid).rows
}

/// Where a cell sits in its column.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RowRole {
    /// The sticky marker row.
    Marker,
    /// A file line.
    File,
    /// Below the last file line.
    Below,
}

/// The style of a diff cell; `kind` is `None` for a group separator.
/// A changed column is a tab on the marker row, a tinted cell on file
/// rows, and a fainter fill below the file. Separators are blank gaps
/// between the tabs and thin rules between the file cells.
fn diff_style(theme: &Theme, role: RowRole, kind: Option<StepKind>) -> Style {
    match (kind, role) {
        (Some(StepKind::Match), _) | (None, RowRole::Marker) => theme.plain,
        (None, _) => theme.separator,
        (Some(StepKind::Delete), RowRole::Marker) => theme.delete_tab,
        (Some(StepKind::Delete), RowRole::File) => theme.delete,
        (Some(StepKind::Delete), RowRole::Below) => theme.delete_fill,
        (Some(StepKind::Insert), RowRole::Marker) => theme.insert_tab,
        (Some(StepKind::Insert), RowRole::File) => theme.insert,
        (Some(StepKind::Insert), RowRole::Below) => theme.insert_fill,
    }
}

/// Cached display rows for the current grid, rebuilt once per reload.
struct Display {
    rows: Vec<String>,
    columns: Vec<DisplayColumn>,
    /// Shared diff kind per display cell; `None` denotes a separator.
    kinds: Vec<Option<StepKind>>,
    /// Layout width shared by rows when cells are drawn independently.
    width: usize,
    /// Changes in display order.
    changes: Vec<Change>,
    /// Per grid step: its display cells and its column in each file.
    places: Vec<Place>,
    stats: DiffStats,
}

/// Where a grid step is drawn, and which column it is in the old and
/// the new file (`None` on the side it was deleted from or inserted
/// into).
#[derive(Clone, Debug, PartialEq, Eq)]
struct Place {
    cells: Range<usize>,
    old: Option<usize>,
    new: Option<usize>,
}

/// A run of deleted or inserted columns.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Run {
    steps: Range<usize>,
    /// The run's first column in its own file: the old file for a
    /// delete run, the new file for an insert run.
    column: usize,
    /// Display cells the run covers.
    cells: Range<usize>,
}

/// A delete run, an insert run, or a delete run directly followed by
/// an insert run.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Change {
    delete: Option<Run>,
    insert: Option<Run>,
}

impl Change {
    fn runs(&self) -> impl Iterator<Item = (StepKind, &Run)> {
        let delete = self.delete.iter().map(|run| (StepKind::Delete, run));
        delete.chain(self.insert.iter().map(|run| (StepKind::Insert, run)))
    }

    /// Display cells from the first run's start to the last run's end,
    /// including the separator between the runs.
    fn cells(&self) -> Range<usize> {
        let mut runs = self.runs().map(|(_, run)| &run.cells);
        let first = runs.next().expect("a change has at least one run");
        first.start..runs.last().unwrap_or(first).end
    }
}

/// Change counts for the top bar.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct DiffStats {
    changes: usize,
    /// Deleted and inserted grapheme columns.
    deleted: usize,
    inserted: usize,
}

impl DiffStats {
    fn of(grid: &DiffGrid) -> DiffStats {
        let changes = change_groups(grid);
        let columns = |group: Option<usize>| group.map_or(0, |group| grid.groups[group].len());
        DiffStats {
            changes: changes.len(),
            deleted: changes.iter().map(|(delete, _)| columns(*delete)).sum(),
            inserted: changes.iter().map(|(_, insert)| columns(*insert)).sum(),
        }
    }
}

/// The groups of each change, in order: a delete run, an insert run,
/// or a delete run directly followed by an insert run.
fn change_groups(grid: &DiffGrid) -> Vec<(Option<usize>, Option<usize>)> {
    let mut changes = Vec::new();
    // A delete run waits here for an insert run right after it.
    let mut pending = None;
    for (index, group) in grid.groups.iter().enumerate() {
        match grid.steps[group.start].kind {
            StepKind::Match => changes.extend(pending.take().map(|delete| (Some(delete), None))),
            StepKind::Delete => pending = Some(index),
            StepKind::Insert => changes.push((pending.take(), Some(index))),
        }
    }
    changes.extend(pending.map(|delete| (Some(delete), None)));
    changes
}

/// How a changed file's diff came out, for the sidebar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FileStats {
    Counted(DiffStats),
    Binary,
    /// Git failed to load one side.
    Failed,
}

/// Count the changes in one file of a git diff.
fn count_file(git: &dyn GitShell, spec: &RevSpec, file: &ChangedFile) -> FileStats {
    match git::load_content(git, spec, file) {
        Ok(Some((old, new))) => FileStats::Counted(DiffStats::of(&diff::compute(&old, &new))),
        Ok(None) => FileStats::Binary,
        Err(_) => FileStats::Failed,
    }
}

/// Count every file's changes on a background thread, in sidebar
/// order, sending each result as it is ready.
fn count_files_in_background(
    spec: RevSpec,
    files: Vec<ChangedFile>,
) -> std::sync::mpsc::Receiver<(usize, FileStats)> {
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for (index, file) in files.iter().enumerate() {
            let stats = count_file(&git::RealGit, &spec, file);
            if sender.send((index, stats)).is_err() {
                break;
            }
        }
    });
    receiver
}

struct DisplayColumn {
    start: usize,
    width: usize,
    /// A grid step, or None for a group separator.
    step: Option<usize>,
}

fn build_display(grid: &DiffGrid) -> Display {
    let mut marker = String::new();
    let mut rows: Vec<String> = (0..grid.height).map(|_| String::new()).collect();
    let mut kinds = Vec::new();
    let mut columns = Vec::new();
    // Each group's run, or `None` for a match group.
    let mut runs: Vec<Option<Run>> = Vec::with_capacity(grid.groups.len());
    let mut places = Vec::with_capacity(grid.steps.len());
    let (mut old_column, mut new_column) = (0, 0);
    for group in &grid.groups {
        let kind = grid.steps[group.start].kind;
        let mark = match kind {
            StepKind::Match => ' ',
            StepKind::Delete => '-',
            StepKind::Insert => '+',
        };
        let run_column = if kind == StepKind::Insert {
            new_column
        } else {
            old_column
        };
        let run_start = kinds.len();
        for step_index in group.clone() {
            let step = &grid.steps[step_index];
            let cells: Vec<_> = step
                .content
                .iter()
                .chain(std::iter::repeat(" "))
                .take(grid.height)
                .map(|raw| {
                    let text = visible_block(raw, kind);
                    let width = rendered_width(&text);
                    (text, width)
                })
                .collect();
            // Each step already owns one complete grapheme per row.
            let width = cells.iter().map(|(_, width)| *width).max().unwrap_or(0);
            columns.push(DisplayColumn {
                start: kinds.len(),
                width,
                step: Some(step_index),
            });
            places.push(Place {
                cells: kinds.len()..kinds.len() + width,
                old: (kind != StepKind::Insert).then_some(old_column),
                new: (kind != StepKind::Delete).then_some(new_column),
            });
            if width > 0 {
                marker.push(mark);
                marker.extend(std::iter::repeat_n(' ', width - 1));
            }
            kinds.extend(std::iter::repeat_n(Some(kind), width));
            for (row, (text, actual_width)) in rows.iter_mut().zip(cells) {
                row.push_str(&text);
                row.extend(std::iter::repeat_n(' ', width - actual_width));
            }
            if kind != StepKind::Insert {
                old_column += 1;
            }
            if kind != StepKind::Delete {
                new_column += 1;
            }
        }
        runs.push((kind != StepKind::Match).then(|| Run {
            steps: group.clone(),
            column: run_column,
            cells: run_start..kinds.len(),
        }));
        if group.end < grid.steps.len() {
            columns.push(DisplayColumn {
                start: kinds.len(),
                width: 1,
                step: None,
            });
            marker.push('|');
            kinds.push(None);
            for row in &mut rows {
                row.push('|');
            }
        }
    }
    let changes = change_groups(grid)
        .into_iter()
        .map(|(delete, insert)| Change {
            delete: delete.and_then(|group| runs[group].take()),
            insert: insert.and_then(|group| runs[group].take()),
        })
        .collect();
    let mut out = vec![marker];
    out.extend(rows);
    let width = kinds.len();
    let stats = DiffStats::of(grid);
    Display {
        rows: out,
        columns,
        kinds,
        width,
        changes,
        places,
        stats,
    }
}

/// Changes with display cells in `left..right`.
fn visible_changes(display: &Display, left: usize, right: usize) -> impl Iterator<Item = &Change> {
    let first = display
        .changes
        .partition_point(|change| change.cells().end <= left);
    display.changes[first..]
        .iter()
        .take_while(move |change| change.cells().start < right)
}

/// A tab label in git's hunk-header style: `-4,15` for a delete run
/// starting at old column 4 and spanning 15 columns, `+4` for a
/// one-column insert run. It shortens to `-4`, then `-`, to fit `width`.
fn tab_label(kind: StepKind, run: &Run, width: usize) -> String {
    let sign = if kind == StepKind::Delete { '-' } else { '+' };
    let column = run.column + 1;
    let count = run.steps.len();
    let full = if count == 1 {
        format!("{sign}{column}")
    } else {
        format!("{sign}{column},{count}")
    };
    [full, format!("{sign}{column}"), sign.to_string()]
        .into_iter()
        .find(|label| label.len() <= width)
        .unwrap_or_default()
}

/// Give standalone zero-width glyphs a base so they cannot attach to
/// separators or padding. Filter controls only after raw layout boundaries
/// have been recorded; control-only edits still need a visible indication.
fn visible_block(text: &str, kind: StepKind) -> Cow<'_, str> {
    let standalone = text
        .graphemes(true)
        .any(|grapheme| !grapheme.chars().any(char::is_control) && needs_display_base(grapheme));
    if !standalone && (kind == StepKind::Match || rendered_width(text) > 0) {
        return Cow::Borrowed(text);
    }
    let mut visible = String::with_capacity(text.len());
    for grapheme in text.graphemes(true) {
        if grapheme.chars().any(char::is_control) {
            // Keep raw boundaries until drawing. Removing a control here
            // could join glyphs on either side into a different grapheme.
            visible.push_str(grapheme);
            continue;
        }
        if needs_display_base(grapheme) {
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
    Cow::Owned(visible)
}

/// Leading extenders need a base even when the entire grapheme has width.
/// Probe the first scalar against a dotted circle on the stack, so normal
/// cells do not allocate and adjacent terminal writes cannot absorb the mark.
fn needs_display_base(grapheme: &str) -> bool {
    if Span::raw(grapheme).width() == 0 {
        return true;
    }
    let Some(first) = grapheme.chars().next() else {
        return false;
    };
    let mut bytes = [0_u8; 7]; // dotted circle (3 bytes) + one scalar (up to 4)
    bytes[..3].copy_from_slice("◌".as_bytes());
    let len = 3 + first.encode_utf8(&mut bytes[3..]).len();
    std::str::from_utf8(&bytes[..len])
        .unwrap()
        .graphemes(true)
        .count()
        == 1
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

/// Shown in place of the diff when there is nothing to draw.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notice {
    pub title: String,
    pub detail: Option<String>,
}

pub struct App {
    pub source: DiffSource,
    pub selection: usize,
    /// Sidebar visibility chosen with `e`. `None` follows the terminal
    /// width: shown from `SIDEBAR_MIN_FRAME_WIDTH` columns up.
    pub sidebar: Option<bool>,
    /// Whether the key help popup is open.
    pub help: bool,
    /// The inspect cursor, while inspecting cells.
    pub inspect: Option<Cursor>,
    pub scroll_y: usize,
    pub scroll_x: usize,
    pub grid: Option<DiffGrid>,
    /// Cached display rows for the current grid (see `Display`);
    /// rebuilt in `reload`.
    display: Option<Display>,
    pub notice: Option<Notice>,
    pub degraded: bool,
    pub labels: (String, String),
    theme: Theme,
    /// Inner size of the diff pane, marker row included.
    pane_width: usize,
    pane_height: usize,
    /// Terminal width at the last render; 0 before the first one.
    frame_width: u16,
    /// First sidebar entry on screen.
    sidebar_offset: usize,
    /// Where the last frame drew the parts the mouse can act on.
    areas: Areas,
    /// Each changed file's counts, once known (git mode).
    file_stats: Vec<Option<FileStats>>,
}

/// Where the last frame put the parts the mouse can act on.
#[derive(Clone, Copy, Debug, Default)]
struct Areas {
    /// The sidebar's file list, title row included.
    sidebar: Option<Rect>,
    /// The diff, marker row included.
    pane: Rect,
    overview: Option<Rect>,
}

/// Scroll offsets, selection, sidebar, help, and inspect cursor.
type ViewState = (usize, usize, usize, bool, bool, Option<Cursor>);

/// A cell of the diff: a file row and a grid step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cursor {
    pub row: usize,
    pub step: usize,
}

impl App {
    pub fn new(source: DiffSource, theme: Theme) -> App {
        let labels = match &source {
            DiffSource::Files { old, new } => {
                (old.display().to_string(), new.display().to_string())
            }
            DiffSource::Git { spec, .. } => (spec.old_label(), spec.new_label()),
        };
        let mut app = App {
            source,
            selection: 0,
            sidebar: None,
            help: false,
            inspect: None,
            scroll_y: 0,
            scroll_x: 0,
            grid: None,
            display: None,
            notice: None,
            degraded: false,
            labels,
            theme,
            // Unknown until the first render: 0 means "no clamp yet"
            // (max_scroll = content size). A positive default like
            // 80x24 would pin max_scroll_y/x to 0 for small diffs and
            // make j/l/n/p unable to move.
            pane_width: 0,
            pane_height: 0,
            frame_width: 0,
            sidebar_offset: 0,
            areas: Areas::default(),
            file_stats: Vec::new(),
        };
        app.file_stats = vec![None; app.entries().len()];
        app.reload();
        app
    }

    pub fn has_sidebar(&self) -> bool {
        matches!(self.source, DiffSource::Git { .. })
    }

    /// Whether the file sidebar is on screen. Before the first render
    /// the terminal width is unknown and counts as wide.
    pub fn sidebar_visible(&self) -> bool {
        let wide = self.frame_width == 0 || self.frame_width >= SIDEBAR_MIN_FRAME_WIDTH;
        self.has_sidebar() && !self.entries().is_empty() && self.sidebar.unwrap_or(wide)
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
        self.inspect = None;
        match loaded {
            Ok(Some((old, new))) => {
                let grid = diff::compute(&old, &new);
                self.degraded = grid.degraded;
                // Display rows depend only on the grid; build them
                // once here instead of on every frame.
                let display = build_display(&grid);
                // The loaded file's counts come free with its diff.
                self.record_stats(self.selection, FileStats::Counted(display.stats));
                self.display = Some(display);
                self.grid = Some(grid);
                self.notice = None;
            }
            Ok(None) => {
                self.grid = None;
                self.display = None;
                self.degraded = false;
                self.record_stats(self.selection, FileStats::Binary);
                let (old, new) = &self.labels;
                self.notice = Some(match &self.source {
                    DiffSource::Git { files, .. } if files.is_empty() => Notice {
                        title: "No changes".to_string(),
                        detail: Some(format!("{old} → {new}")),
                    },
                    DiffSource::Git { files, .. } => Notice {
                        title: "Binary or unreadable file".to_string(),
                        detail: Some(chrome::entry_text(
                            &files[self.selection.min(files.len() - 1)],
                        )),
                    },
                    DiffSource::Files { .. } => Notice {
                        title: "Binary or unreadable file".to_string(),
                        detail: Some(format!("{old} → {new}")),
                    },
                });
            }
            Err(message) => {
                self.grid = None;
                self.display = None;
                self.degraded = false;
                self.record_stats(self.selection, FileStats::Failed);
                self.notice = Some(Notice {
                    title: "Could not load this file".to_string(),
                    detail: Some(message),
                });
            }
        }
    }

    /// Remember a file's counts for the sidebar, from the diff on
    /// screen.
    fn record_stats(&mut self, index: usize, stats: FileStats) {
        if let Some(slot) = self.file_stats.get_mut(index) {
            *slot = Some(stats);
        }
    }

    /// Take a background count only for a file without one: a file
    /// loaded since may have changed after the worker read it, and its
    /// counts must match the diff that was shown.
    fn fill_stats(&mut self, index: usize, stats: FileStats) -> bool {
        match self.file_stats.get_mut(index) {
            Some(slot @ None) => {
                *slot = Some(stats);
                true
            }
            _ => false,
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
        // The sticky marker row takes the pane's first row, so
        // pane_height - 1 content rows are visible.
        h.saturating_sub(self.pane_height.saturating_sub(1))
    }

    fn max_scroll_x(&self) -> usize {
        let (_, w) = self.content_dims();
        w.saturating_sub(self.pane_width)
    }

    pub fn render(&mut self, frame: &mut Frame<'_>) {
        let area = frame.area();
        self.frame_width = area.width;
        let [top_bar, main, status_bar] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .areas(area);
        let mut diff_area = main;
        let mut sidebar = None;
        if self.sidebar_visible() {
            let width = self.sidebar_width(main.width);
            let [list, rule, rest] = Layout::horizontal([
                Constraint::Length(width),
                Constraint::Length(1),
                Constraint::Min(0),
            ])
            .areas(main);
            sidebar = Some((list, rule));
            diff_area = rest;
        }
        // A blank cell on each side keeps the diff off the divider and
        // the terminal edge; line numbers take the left one's place.
        let gutter = Rect {
            width: self.gutter_width(diff_area.width),
            ..diff_area
        };
        let left = gutter.width.max(1);
        let mut pane = Rect {
            x: diff_area.x + left,
            width: diff_area.width.saturating_sub(left + 1),
            ..diff_area
        };
        // A diff wider than the pane gets an overview bar on the pane's
        // last row.
        let mut overview = None;
        if let Some(display) = &self.display
            && pane.height >= OVERVIEW_MIN_PANE_HEIGHT
            && display.width > usize::from(pane.width)
        {
            pane.height -= 1;
            overview = Some(Rect {
                y: pane.bottom(),
                height: 1,
                ..pane
            });
        }
        let gutter = Rect {
            height: pane.height,
            ..gutter
        };
        self.areas = Areas {
            sidebar: sidebar.map(|(list, _)| list),
            pane,
            overview,
        };
        // Record the pane size before rendering the status bar so the
        // scroll clamps are correct on the very first frame (and not
        // only after a state-changing key triggers a redraw).
        self.pane_width = usize::from(pane.width);
        self.pane_height = usize::from(pane.height);
        // A resize or a sidebar toggle can shrink the pane below the
        // current scroll; re-clamp here so every frame heals it (a
        // stale scroll_y after `G` + enlarge would blank the pane).
        self.scroll_y = self.scroll_y.min(self.max_scroll_y());
        self.scroll_x = self.scroll_x.min(self.max_scroll_x());
        if let Some((list, rule)) = sidebar {
            self.scroll_sidebar(usize::from(list.height.saturating_sub(1)));
            self.render_sidebar(frame, list);
            self.render_rule(frame, rule);
        }
        self.render_gutter(frame, gutter);
        self.render_diff(frame, pane);
        if let Some(area) = overview {
            self.render_overview(frame, area);
        }
        self.render_top_bar(frame, top_bar);
        self.render_status(frame, status_bar);
        if self.help {
            self.render_help(frame, area);
        }
    }

    /// Width of the line-number gutter: a blank cell, the numbers (at
    /// least two digits), and a blank cell. Panes narrower than
    /// `GUTTER_MIN_AREA_WIDTH` keep their columns for the diff.
    fn gutter_width(&self, area_width: u16) -> u16 {
        match &self.grid {
            Some(grid) if area_width >= GUTTER_MIN_AREA_WIDTH => {
                let digits = grid.height.max(1).ilog10() + 1;
                digits.max(2) as u16 + 2
            }
            _ => 0,
        }
    }

    /// A one-row map of the whole diff width: a tick where columns were
    /// deleted or inserted (half red, half green where both meet). The
    /// part the pane shows has bright ticks between two `┃`, drawn over
    /// any ticks so a crowded bar still shows it; ticks outside it are
    /// dim.
    fn render_overview(&self, frame: &mut Frame<'_>, area: Rect) {
        let Some(display) = &self.display else {
            return;
        };
        let (total, cells) = (display.width, usize::from(area.width));
        let span = |cell: usize| {
            let start = cell * total / cells;
            start..((cell + 1) * total / cells).clamp(start + 1, total)
        };
        let view = self.scroll_x..self.scroll_x + self.pane_width;
        let shown = |cell: usize| {
            let span = span(cell);
            span.start < view.end && span.end > view.start
        };
        let first = (0..cells).find(|&cell| shown(cell));
        let last = (0..cells).rev().find(|&cell| shown(cell));
        let theme = &self.theme;
        for cell in 0..cells {
            let kinds = &display.kinds[span(cell)];
            let deleted = kinds.contains(&Some(StepKind::Delete));
            let inserted = kinds.contains(&Some(StepKind::Insert));
            let (red, green) = if shown(cell) {
                (theme.delete_mark.fg, theme.insert_mark.fg)
            } else {
                (theme.delete.bg, theme.insert.bg)
            };
            let tick = |fg, bg| Style {
                fg,
                bg,
                ..Style::new()
            };
            let (symbol, style) = match (deleted, inserted) {
                _ if Some(cell) == first || Some(cell) == last => ("┃", theme.viewport),
                (true, true) => ("▌", tick(red, green)),
                (true, false) => ("▌", tick(red, None)),
                (false, true) => ("▌", tick(green, None)),
                _ if shown(cell) => ("━", theme.viewport),
                _ => ("─", theme.rule),
            };
            frame
                .buffer_mut()
                .set_string(area.x + cell as u16, area.y, symbol, style);
        }
    }

    /// Line numbers beside the visible file rows, `~` below the file.
    fn render_gutter(&self, frame: &mut Frame<'_>, area: Rect) {
        let Some(grid) = &self.grid else {
            return;
        };
        if area.width < 3 || area.height < 2 {
            return;
        }
        let digits = usize::from(area.width - 2);
        // The marker row has no number.
        for (offset, y) in (area.y + 1..area.bottom()).enumerate() {
            let row = self.scroll_y + offset;
            let (text, style) = if row < grid.height {
                (format!("{:>digits$}", row + 1), self.theme.muted)
            } else {
                (format!("{:>digits$}", "~"), self.theme.rule)
            };
            frame.buffer_mut().set_string(area.x + 1, y, text, style);
        }
    }

    fn render_diff(&self, frame: &mut Frame<'_>, inner: Rect) {
        if inner.is_empty() {
            return;
        }
        match (&self.grid, &self.notice) {
            (Some(_), _) => {
                // Rows are cached in `self.display` (see `build_display`);
                // they are built once per reload, not per frame.
                let Some(disp) = &self.display else {
                    return;
                };
                // Color the full visible column as a lane, down to the
                // bottom of the pane; the marker and file rows restyle
                // their own cells. Row drawing restores continuation-cell
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
                        diff_style(&self.theme, RowRole::Below, *kind),
                    );
                }
                let rows = &disp.rows;
                // Row windows are computed in usize: u16 would wrap
                // around for diffs taller than 65535 rows and blank
                // out the bottom. `y` is in the frame by the time it
                // is cast.
                let top = usize::from(inner.y);
                let bottom = top + usize::from(inner.height);
                self.draw_tabs(frame, inner, disp);
                for (i, _) in rows.iter().enumerate().skip(1) {
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
                    self.draw_content_row(frame, inner, y as u16, i - 1, disp);
                }
                self.draw_cursor(frame, inner, disp);
            }
            (None, Some(notice)) => self.render_notice(frame, inner, notice),
            (None, None) => {}
        }
    }

    fn draw_content_row(
        &self,
        frame: &mut Frame<'_>,
        inner: Rect,
        y: u16,
        row: usize,
        display: &Display,
    ) {
        let Some(grid) = &self.grid else { return };
        let first = display
            .columns
            .partition_point(|column| column.start + column.width <= self.scroll_x);
        let right = self.scroll_x + usize::from(inner.width);
        for column in &display.columns[first..] {
            if column.start >= right {
                break;
            }
            let text = match column.step {
                Some(index) => {
                    let step = &grid.steps[index];
                    visible_block(step.content.get(row).unwrap_or(" "), step.kind)
                }
                None => Cow::Borrowed("│"),
            };
            self.draw_cell(frame, inner, y, &text, column.start);
        }
        self.style_row(frame, inner, y, &display.kinds, RowRole::File);
    }

    /// Highlight the cell under the inspect cursor, if it is in view.
    fn draw_cursor(&self, frame: &mut Frame<'_>, inner: Rect, display: &Display) {
        let Some(cursor) = self.inspect else {
            return;
        };
        let rows = usize::from(inner.height).saturating_sub(1);
        if cursor.row < self.scroll_y || cursor.row >= self.scroll_y + rows {
            return;
        }
        let y = inner.y + 1 + (cursor.row - self.scroll_y) as u16;
        let right = self.scroll_x + usize::from(inner.width);
        let cells = &display.places[cursor.step].cells;
        for cell in cells.start.max(self.scroll_x)..cells.end.min(right) {
            let x = inner.x + (cell - self.scroll_x) as u16;
            frame.buffer_mut()[(x, y)].set_style(self.theme.cursor);
        }
    }

    /// The marker row: a tab over each changed run, labelled with where
    /// the run starts in its file and how many columns it spans. A run
    /// scrolled partly out of view keeps its label at the pane's edge.
    fn draw_tabs(&self, frame: &mut Frame<'_>, inner: Rect, display: &Display) {
        self.style_row(frame, inner, inner.y, &display.kinds, RowRole::Marker);
        let left = self.scroll_x;
        let right = left + usize::from(inner.width);
        for change in visible_changes(display, left, right) {
            for (kind, run) in change.runs() {
                let start = run.cells.start.max(left);
                let end = run.cells.end.min(right);
                if start < end {
                    let x = inner.x + (start - left) as u16;
                    let label = tab_label(kind, run, end - start);
                    frame
                        .buffer_mut()
                        .set_string(x, inner.y, label, Style::default());
                }
            }
        }
    }

    fn draw_cell(&self, frame: &mut Frame<'_>, inner: Rect, y: u16, text: &str, start: usize) {
        let scroll_x = self.scroll_x;
        let right = scroll_x + usize::from(inner.width);
        let mut pos = start;
        // Segment each cell independently, so display substitutions cannot
        // combine with extenders in the next cell and change its width.
        for (grapheme, width) in rendered_graphemes(text) {
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
    }

    fn style_row(
        &self,
        frame: &mut Frame<'_>,
        inner: Rect,
        y: u16,
        kinds: &[Option<StepKind>],
        role: RowRole,
    ) {
        // Restore styles in wide-glyph continuation and clipped cells.
        for (offset, kind) in kinds
            .iter()
            .skip(self.scroll_x)
            .take(usize::from(inner.width))
            .enumerate()
        {
            frame.buffer_mut()[(inner.x + offset as u16, y)].set_style(diff_style(
                &self.theme,
                role,
                *kind,
            ));
        }
    }

    /// Handle one key event. Returns whether the app should quit and
    /// whether the visible state changed (only then must the screen be
    /// redrawn).
    pub fn handle_key(&mut self, key: KeyEvent) -> KeyResult {
        // Windows reports key releases too; act only on presses and repeats.
        if key.kind == KeyEventKind::Release {
            return KeyResult {
                quit: false,
                changed: false,
            };
        }
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
        if self.help {
            // The help popup takes the keyboard until it closes, so `q`
            // closes it instead of quitting.
            let close = matches!(
                key.code,
                KeyCode::Char('?') | KeyCode::Char('q') | KeyCode::Esc
            );
            self.help = !close;
            return KeyResult {
                quit: false,
                changed: close,
            };
        }
        let before = self.view_state();
        if self.inspect.is_some() {
            // hjkl, g, and G move the cursor; Esc and i stop inspecting.
            // Other keys keep their usual meaning.
            match key.code {
                KeyCode::Esc | KeyCode::Char('i') => self.inspect = None,
                KeyCode::Char('h') => self.move_cursor(-1, 0),
                KeyCode::Char('l') => self.move_cursor(1, 0),
                KeyCode::Char('j') => self.move_cursor(0, 1),
                KeyCode::Char('k') => self.move_cursor(0, -1),
                KeyCode::Char('g') => self.move_cursor(0, isize::MIN),
                KeyCode::Char('G') => self.move_cursor(0, isize::MAX),
                _ => return self.handle_plain_key(key, before),
            }
            return KeyResult {
                quit: false,
                changed: before != self.view_state(),
            };
        }
        self.handle_plain_key(key, before)
    }

    /// What keys and the mouse can change; comparing it before and
    /// after an event tells whether the screen must redraw.
    fn view_state(&self) -> ViewState {
        (
            self.scroll_y,
            self.scroll_x,
            self.selection,
            self.sidebar_visible(),
            self.help,
            self.inspect,
        )
    }

    fn handle_plain_key(&mut self, key: KeyEvent, before: ViewState) -> KeyResult {
        match key.code {
            KeyCode::Char('q') => {
                return KeyResult {
                    quit: true,
                    changed: false,
                };
            }
            KeyCode::Char('?') => self.help = true,
            KeyCode::Char('i') => self.start_inspecting(),
            KeyCode::Char('e') if self.has_sidebar() => {
                self.sidebar = Some(!self.sidebar_visible())
            }
            // vim convention: g = top, G = bottom.
            KeyCode::Char('g') => self.scroll_y = 0,
            KeyCode::Char('G') => self.scroll_y = self.max_scroll_y(),
            KeyCode::Char('n') => self.jump_change(1),
            KeyCode::Char('p') => self.jump_change(-1),
            KeyCode::Char('h') => self.scroll_x = self.scroll_x.saturating_sub(1),
            KeyCode::Char('l') => self.scroll_x = (self.scroll_x + 1).min(self.max_scroll_x()),
            KeyCode::Char('j') => self.scroll_y = (self.scroll_y + 1).min(self.max_scroll_y()),
            KeyCode::Char('k') => self.scroll_y = self.scroll_y.saturating_sub(1),
            KeyCode::Tab => self.move_selection(1),
            KeyCode::BackTab => self.move_selection(-1),
            _ => {}
        }
        KeyResult {
            quit: false,
            changed: before != self.view_state(),
        }
    }

    /// Put the inspect cursor on the first visible cell.
    fn start_inspecting(&mut self) {
        let (Some(grid), Some(display)) = (&self.grid, &self.display) else {
            return;
        };
        if grid.height == 0 {
            return;
        }
        let shown = |place: &Place| !place.cells.is_empty() && place.cells.start >= self.scroll_x;
        if let Some(step) = display.places.iter().position(shown) {
            let row = self.scroll_y.min(grid.height - 1);
            self.inspect = Some(Cursor { row, step });
        }
    }

    /// Move the inspect cursor by `dx` drawn columns and `dy` rows,
    /// stopping at the edges, then scroll it into view.
    fn move_cursor(&mut self, dx: isize, dy: isize) {
        let (Some(grid), Some(display), Some(cursor)) = (&self.grid, &self.display, self.inspect)
        else {
            return;
        };
        let drawn = |step: &usize| !display.places[*step].cells.is_empty();
        let step = match dx.signum() {
            1 => (cursor.step + 1..display.places.len()).find(drawn),
            -1 => (0..cursor.step).rev().find(drawn),
            _ => None,
        }
        .unwrap_or(cursor.step);
        let row = cursor
            .row
            .saturating_add_signed(dy)
            .min(grid.height.saturating_sub(1));
        let cells = display.places[step].cells.clone();
        self.inspect = Some(Cursor { row, step });
        let rows = self.pane_height.saturating_sub(1).max(1);
        if row < self.scroll_y {
            self.scroll_y = row;
        } else if row >= self.scroll_y + rows {
            self.scroll_y = row + 1 - rows;
        }
        if cells.start < self.scroll_x {
            self.scroll_x = cells.start;
        } else if self.pane_width > 0 && cells.end > self.scroll_x + self.pane_width {
            self.scroll_x = cells.end - self.pane_width;
        }
    }

    /// The diff cell drawn at screen position (`x`, `y`), if any.
    fn cell_at(&self, x: u16, y: u16) -> Option<Cursor> {
        let (grid, display) = (self.grid.as_ref()?, self.display.as_ref()?);
        let pane = self.areas.pane;
        if !pane.contains((x, y).into()) || y == pane.y {
            return None;
        }
        let row = self.scroll_y + usize::from(y - pane.y - 1);
        let cell = self.scroll_x + usize::from(x - pane.x);
        let step = display
            .places
            .partition_point(|place| place.cells.end <= cell);
        let place = display.places.get(step)?;
        (row < grid.height && place.cells.contains(&cell)).then_some(Cursor { row, step })
    }

    /// Handle one mouse event; returns whether the screen must redraw.
    /// The wheel scrolls the diff (sideways with Shift) or, over the
    /// sidebar, moves the selection. A click picks a file in the
    /// sidebar, and a click or drag on the overview bar moves the view
    /// there.
    pub fn handle_mouse(&mut self, mouse: MouseEvent) -> bool {
        let at = |area: Option<Rect>| {
            area.filter(|area| area.contains((mouse.column, mouse.row).into()))
        };
        if self.help {
            // Any click closes the help popup.
            let click = matches!(mouse.kind, MouseEventKind::Down(_));
            self.help = !click;
            return click;
        }
        let before = self.view_state();
        let sideways = mouse.modifiers.contains(KeyModifiers::SHIFT);
        match mouse.kind {
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp
                if at(self.areas.sidebar).is_some() =>
            {
                let down = mouse.kind == MouseEventKind::ScrollDown;
                self.move_selection(if down { 1 } else { -1 });
            }
            MouseEventKind::ScrollDown if sideways => self.scroll_right(WHEEL_CELLS),
            MouseEventKind::ScrollUp if sideways => self.scroll_left(WHEEL_CELLS),
            MouseEventKind::ScrollDown => {
                self.scroll_y = (self.scroll_y + WHEEL_ROWS).min(self.max_scroll_y());
            }
            MouseEventKind::ScrollUp => self.scroll_y = self.scroll_y.saturating_sub(WHEEL_ROWS),
            MouseEventKind::ScrollRight => self.scroll_right(WHEEL_CELLS),
            MouseEventKind::ScrollLeft => self.scroll_left(WHEEL_CELLS),
            MouseEventKind::Down(MouseButton::Left) | MouseEventKind::Drag(MouseButton::Left) => {
                if let Some(bar) = at(self.areas.overview) {
                    self.center_view_on(bar, mouse.column);
                } else if let Some(cell) = self.cell_at(mouse.column, mouse.row) {
                    self.inspect = Some(cell);
                } else if let Some(list) = at(self.areas.sidebar)
                    && mouse.kind == MouseEventKind::Down(MouseButton::Left)
                    && mouse.row > list.y
                {
                    let index = self.sidebar_offset + usize::from(mouse.row - list.y - 1);
                    if index < self.entries().len() && index != self.selection {
                        self.selection = index;
                        self.reload();
                    }
                }
            }
            _ => {}
        }
        before != self.view_state()
    }

    fn scroll_right(&mut self, cells: usize) {
        self.scroll_x = (self.scroll_x + cells).min(self.max_scroll_x());
    }

    fn scroll_left(&mut self, cells: usize) {
        self.scroll_x = self.scroll_x.saturating_sub(cells);
    }

    /// Scroll so the display cell under overview-bar column `column`
    /// sits in the middle of the pane.
    fn center_view_on(&mut self, bar: Rect, column: u16) {
        let Some(display) = &self.display else {
            return;
        };
        let cell = usize::from(column - bar.x);
        let width = usize::from(bar.width);
        let target = (2 * cell + 1) * display.width / (2 * width);
        self.scroll_x = target
            .saturating_sub(self.pane_width / 2)
            .min(self.max_scroll_x());
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

    /// Scroll sideways to the next (`dir` > 0) or previous change,
    /// counted from the pane's left edge. The vertical scroll stays put,
    /// so jumping never hints at which lines changed.
    fn jump_change(&mut self, dir: i32) {
        let Some(display) = &self.display else { return };
        let max = self.max_scroll_x();
        // Where each change puts the pane's left edge. Changes near the
        // end clamp to the same stop, and all of them are in view there.
        let stops: Vec<usize> = display
            .changes
            .iter()
            .map(|change| change.cells().start.saturating_sub(CHANGE_CONTEXT).min(max))
            .collect();
        if stops.is_empty() {
            return;
        }
        let target = if dir > 0 {
            // Past the last change, wrap around to the first.
            stops
                .iter()
                .copied()
                .find(|&x| x > self.scroll_x)
                .or(stops.first().copied())
        } else {
            // Before the first change, go back to the left edge; from
            // there, wrap around to the last change.
            stops
                .iter()
                .copied()
                .rev()
                .find(|&x| x < self.scroll_x)
                .or(if self.scroll_x == 0 {
                    stops.last().copied()
                } else {
                    Some(0)
                })
        };
        if let Some(x) = target {
            self.scroll_x = x;
        }
    }
}

pub fn run_tui(cli: &crate::cli::Cli) -> Result<(), Box<dyn std::error::Error>> {
    let source = DiffSource::from_cli(cli)?;
    let mut app = App::new(source, Theme::detect(cli.theme));
    run_tui_app(&mut app)?;
    Ok(())
}

/// Drive the terminal loop until the app signals quit. Redraws only
/// when a key actually changed the state (or the terminal resized),
/// instead of unconditionally on every event.
pub fn run_tui_app(app: &mut App) -> io::Result<()> {
    // Count the other files' changes for the sidebar while the user
    // reads the first one.
    let mut counts = match &app.source {
        DiffSource::Git { spec, files, .. } if files.len() > 1 => {
            Some(count_files_in_background(spec.clone(), files.clone()))
        }
        _ => None,
    };
    let mut terminal = ratatui::init();
    // ratatui's panic hook restores the terminal but leaves mouse
    // capture on; switch it off first.
    let restore = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = crossterm::execute!(io::stdout(), DisableMouseCapture);
        restore(info);
    }));
    let result = (|| {
        crossterm::execute!(io::stdout(), EnableMouseCapture)?;
        terminal.draw(|frame| app.render(frame))?;
        loop {
            // While counts are still coming, wake up for them too.
            if let Some(receiver) = &counts {
                let mut counted = false;
                loop {
                    match receiver.try_recv() {
                        Ok((index, stats)) => counted |= app.fill_stats(index, stats),
                        Err(std::sync::mpsc::TryRecvError::Empty) => break,
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                            counts = None;
                            break;
                        }
                    }
                }
                if counted {
                    terminal.draw(|frame| app.render(frame))?;
                }
                if !event::poll(std::time::Duration::from_millis(100))? {
                    continue;
                }
            }
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
                Event::Mouse(mouse) => {
                    if app.handle_mouse(mouse) {
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
    let _ = crossterm::execute!(io::stdout(), DisableMouseCapture);
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
    use crate::theme::{ColorDepth, Scheme};
    use ratatui::style::{Color, Modifier};
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
    fn accent_edits_compare_complete_graphemes() {
        let grid = compute("e\u{301}\n", "e\u{308}\n");
        let display = build_display(&grid);
        assert_eq!(display.rows, ["-|+", "e\u{301}|e\u{308}"]);
        assert_eq!(display.changes[0].cells(), 0..3);
        let (mut app, a, b) = files_app("e\u{301}\n", "e\u{308}\n");
        let buf = draw_buffer(&mut app);
        assert_column_backgrounds_align(&buf);
        assert_eq!(buf[(1 + G, 1)].symbol(), "-");
        assert_eq!(buf[(1 + G, 2)].symbol(), "e\u{301}");
        assert_eq!(buf[(1 + G, 2)].bg, Color::Red);
        assert_eq!(buf[(3 + G, 1)].symbol(), "+");
        assert_eq!(buf[(3 + G, 2)].symbol(), "e\u{308}");
        assert_eq!(buf[(3 + G, 2)].bg, Color::Green);
        assert_eq!(buf[(2 + G, 2)].symbol(), "│");
        assert_eq!(buf[(2 + G, 2)].bg, Color::Reset);
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
            "  a\u{301}x "
        );
        assert_eq!(display.rows[2], "中bcx");
        for row in &display.rows {
            assert_eq!(rendered_width(row), display.width);
        }
        let (mut app, a, b) = files_app(text, text);
        let buf = draw_buffer(&mut app);
        assert_eq!(buf[(3 + G, 2)].symbol(), "a\u{301}");
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
            assert_eq!(buf[(1 + G, 1)].symbol(), mark);
            assert_eq!(buf[(1 + G, 2)].symbol(), "◌\u{301}");
            assert_eq!(buf[(1 + G, 2)].bg, color);
            assert_eq!(buf[(1 + G, 21)].bg, color);
            let _ = std::fs::remove_file(&a);
            let _ = std::fs::remove_file(&b);
        }
    }

    #[test]
    fn orphan_marks_are_visible_when_other_rows_have_nonzero_width() {
        let (mut app, a, b) = files_app("\u{301}\nx\n", "\u{308}\ny\n");
        let display = app.display.as_ref().unwrap();
        assert_eq!(display.rows, ["-|+", "◌\u{301}|◌\u{308}", "x|y"]);
        let buf = draw_buffer(&mut app);
        assert_column_backgrounds_align(&buf);
        assert_eq!(buf[(1 + G, 2)].symbol(), "◌\u{301}");
        assert_eq!(buf[(3 + G, 2)].symbol(), "◌\u{308}");
        for y in [2, 3] {
            assert_eq!(buf[(1 + G, y)].bg, Color::Red);
            assert_eq!(buf[(3 + G, y)].bg, Color::Green);
            assert_eq!(buf[(2 + G, y)].symbol(), "│");
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
            "   a\u{301}x "
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
        assert_eq!(buf[(1 + G, 2)].symbol(), "👩‍❤️‍👩");
        assert_eq!(buf[(4 + G, 2)].symbol(), "x");
        assert_eq!(buf[(6 + G, 2)].symbol(), "y");
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
        assert_eq!(buf[(1 + G, 2)].symbol(), "🇺");
        assert_eq!(buf[(1 + left_width + G, 2)].symbol(), "🇸");
        assert_eq!(buf[(delete_x + G, 2)].symbol(), "x");
        assert_eq!(buf[(delete_x + G, 2)].bg, Color::Red);
        assert_eq!(buf[(delete_x + 2 + G, 2)].symbol(), "y");
        assert_eq!(buf[(delete_x + 2 + G, 2)].bg, Color::Green);
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    fn control_only_edits_show_code_points() {
        // A suffix keeps the CR inside the line, outside CRLF normalization.
        let grid = compute("\rx", "\u{7}x");
        let display = build_display(&grid);
        assert_eq!(display.rows, ["-     |+     | ", "U+000D|U+0007|x"]);
        assert_eq!(display.changes[0].cells(), 0..13);
        assert_eq!(display.width, 15);
        let (mut app, a, b) = files_app("\rx", "\u{7}x");
        let buf = draw_buffer(&mut app);
        assert_column_backgrounds_align(&buf);
        for x in 1..=6 {
            assert_eq!(buf[(x + G, 2)].bg, Color::Red);
        }
        for x in 8..=13 {
            assert_eq!(buf[(x + G, 2)].bg, Color::Green);
        }
        assert_eq!(buf[(7 + G, 2)].symbol(), "│");
        assert_eq!(buf[(7 + G, 2)].bg, Color::Reset);
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    fn control_labels_keep_neighboring_extenders_in_their_own_cells() {
        for extender in ["\u{fe0f}\u{903}", "\u{903}", "\u{301}"] {
            let (mut app, a, b) = files_app(&format!("\u{7}{extender}x\n"), "Yx\n");
            let marked = visible_block(extender, StepKind::Delete);
            assert_eq!(marked, format!("◌{extender}"));
            let separator_x = 7 + rendered_width(&marked) as u16;
            let buf = draw_buffer(&mut app);
            assert_column_backgrounds_align(&buf);
            assert_eq!(buf[(6 + G, 2)].symbol(), "7");
            assert_eq!(buf[(7 + G, 2)].symbol(), marked.as_ref());
            assert_eq!(buf[(7 + G, 2)].bg, Color::Red);
            assert_eq!(buf[(separator_x + G, 1)].symbol(), " ");
            assert_eq!(buf[(separator_x + G, 2)].symbol(), "│");
            assert_eq!(buf[(separator_x + G, 2)].bg, Color::Reset);
            assert_eq!(buf[(separator_x + 1 + G, 1)].symbol(), "+");
            assert_eq!(buf[(separator_x + 1 + G, 2)].symbol(), "Y");
            assert_eq!(buf[(separator_x + 1 + G, 2)].bg, Color::Green);
            assert_eq!(buf[(separator_x + 3 + G, 2)].symbol(), "x");
            let _ = std::fs::remove_file(&a);
            let _ = std::fs::remove_file(&b);
        }
    }

    #[test]
    fn control_label_boundaries_survive_scroll_and_resize() {
        let extender = "\u{fe0f}\u{903}";
        let marked = visible_block(extender, StepKind::Delete);
        let width = rendered_width(&marked) as u16;
        let (mut app, a, b) = files_app(&format!("\u{7}{extender}x\n"), "Yx\n");
        let mut small = Terminal::new(TestBackend::new(7, 6)).unwrap();
        app.scroll_x = 5;
        let buf = small.draw(|frame| app.render(frame)).unwrap().buffer;
        assert_eq!(app.scroll_x, 5);
        assert_column_backgrounds_align(buf);
        assert_eq!(buf[(1, 2)].symbol(), "7");
        assert_eq!(buf[(2, 2)].symbol(), marked.as_ref());
        assert_eq!(buf[(2 + width, 1)].symbol(), " ");
        assert_eq!(buf[(2 + width, 2)].symbol(), "│");
        assert_eq!(buf[(3 + width, 1)].symbol(), "+");
        assert_eq!(buf[(3 + width, 2)].symbol(), "Y");
        assert_eq!(buf[(3 + width, 2)].bg, Color::Green);
        let buf = draw_buffer(&mut app);
        assert_eq!(app.scroll_x, 0);
        assert_column_backgrounds_align(&buf);
        assert_eq!(buf[(6 + G, 2)].symbol(), "7");
        assert_eq!(buf[(7 + G, 2)].symbol(), marked.as_ref());
        assert_eq!(buf[(8 + width + G, 1)].symbol(), "+");
        assert_eq!(buf[(8 + width + G, 2)].symbol(), "Y");
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    fn dotted_circle_edits_survive_navigation_scrolling_and_resize() {
        let suffix = "z".repeat(40);
        let (mut app, a, b) = files_app(
            &format!("q\r\u{301}{suffix}\n"),
            &format!("q\r\u{308}{suffix}\n"),
        );
        // The change starts at cell 2, within CHANGE_CONTEXT of the left
        // edge, so jumping to it from further right scrolls back to 0.
        app.scroll_x = 20;
        app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE));
        assert_eq!(app.scroll_x, 0);
        let mut terminal = Terminal::new(TestBackend::new(12, 6)).unwrap();
        let buf = terminal.draw(|frame| app.render(frame)).unwrap().buffer;
        assert_column_backgrounds_align(buf);
        assert_eq!(buf[(3, 1)].symbol(), "-");
        assert_eq!(buf[(3, 2)].symbol(), "◌\u{301}");
        assert_eq!(buf[(3, 2)].bg, Color::Red);
        assert_eq!(buf[(5, 1)].symbol(), "+");
        assert_eq!(buf[(5, 2)].symbol(), "◌\u{308}");
        assert_eq!(buf[(5, 2)].bg, Color::Green);
        app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert_eq!(app.scroll_x, 0);
        let buf = draw_buffer(&mut app);
        assert_column_backgrounds_align(&buf);
        assert_eq!(buf[(3 + G, 2)].symbol(), "◌\u{301}");
        assert_eq!(buf[(5 + G, 2)].symbol(), "◌\u{308}");
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

    /// The 16-color theme: black text on red and green for every row
    /// of a changed column, so whole columns share one background.
    /// Columns before the first diff cell at 80x24, past the pane's
    /// old blank first cell: the line-number gutter is four cells (a
    /// blank, two digits, a blank).
    const G: u16 = 3;

    fn ansi16() -> Theme {
        Theme::new(ColorDepth::Ansi16, Scheme::Dark)
    }

    fn files_app(old: &str, new: &str) -> (App, std::path::PathBuf, std::path::PathBuf) {
        files_app_with(old, new, ansi16())
    }

    fn files_app_with(
        old: &str,
        new: &str,
        theme: Theme,
    ) -> (App, std::path::PathBuf, std::path::PathBuf) {
        let a = temp_file("a.txt", old);
        let b = temp_file("b.txt", new);
        let app = App::new(
            DiffSource::Files {
                old: a.clone(),
                new: b.clone(),
            },
            theme,
        );
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
        App::new(
            DiffSource::Git {
                spec,
                files,
                git: Box::new(f),
            },
            ansi16(),
        )
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

    /// The symbols on row `y` in the given columns.
    fn row_text(buf: &ratatui::buffer::Buffer, y: u16, xs: std::ops::Range<u16>) -> String {
        xs.map(|x| buf[(x, y)].symbol()).collect()
    }

    fn assert_column_backgrounds_align(buf: &ratatui::buffer::Buffer) {
        // Every pane row, from below the marker row (y=1) down to the
        // status bar on the last row.
        for x in 1..buf.area.width - 1 {
            for y in 2..buf.area.height - 1 {
                assert_eq!(buf[(x, y)].bg, buf[(x, 1)].bg, "column {x}, row {y}");
            }
        }
    }

    #[test]
    fn renders_marker_row_with_colors() {
        let (mut app, a, b) = files_app("foo\nbar baz\nquux\n", "foo\nbar qaz\nquux\n");
        let backend = draw(&mut app);
        let buf = backend.buffer();
        // The top bar is y=0; the diff pane keeps one blank cell on
        // each side, so the marker row is y=1 from x=1.
        // Marker: "    |-|+...": gap at col 5, '-' at col 6, '+' at col 8.
        assert_eq!(buf[(6 + G, 1)].symbol(), "-");
        assert_eq!(buf[(6 + G, 1)].fg, Color::Black);
        assert_eq!(buf[(6 + G, 1)].bg, Color::Red);
        assert_eq!(buf[(8 + G, 1)].symbol(), "+");
        assert_eq!(buf[(8 + G, 1)].fg, Color::Black);
        assert_eq!(buf[(8 + G, 1)].bg, Color::Green);
        assert_eq!(buf[(5 + G, 1)].symbol(), " ");
        assert_eq!(buf[(5 + G, 1)].bg, Color::Reset);
        for y in 2..=4 {
            assert_eq!(buf[(6 + G, y)].bg, Color::Red);
            assert_eq!(buf[(8 + G, y)].bg, Color::Green);
            assert_eq!(buf[(6 + G, y)].fg, Color::Black);
            assert_eq!(buf[(8 + G, y)].fg, Color::Black);
            assert_eq!(buf[(1 + G, y)].bg, Color::Reset);
            assert_eq!(buf[(5 + G, y)].symbol(), "│");
            assert_eq!(buf[(5 + G, y)].bg, Color::Reset);
            assert_eq!(buf[(5 + G, y)].fg, Color::DarkGray);
        }
        // The colors run to the pane's last row, just above the status
        // bar.
        for y in 5..=22 {
            assert_eq!(buf[(6 + G, y)].bg, Color::Red);
            assert_eq!(buf[(8 + G, y)].bg, Color::Green);
            assert_eq!(buf[(5 + G, y)].bg, Color::Reset);
            assert_eq!(buf[(12 + G, y)].bg, Color::Reset);
        }
        assert_ne!(buf[(6 + G, 23)].bg, Color::Red); // status bar
        assert_eq!(buf[(12 + G, 2)].bg, Color::Reset);
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    fn cjk_separators_align_in_tui() {
        let (mut app, a, b) = files_app("中文abc\n", "中日abc\n");
        let buf = draw_buffer(&mut app);
        // Marker "  |- |+ |   " and content "中|文|日|abc" both place
        // their separators at display cells 2, 5, 8. In buffer coords
        // (inner starts at (1,1)) those are x = 3, 6, 9: gaps on the
        // marker row (y=1) and rules on the content row (y=2).
        for x in [3, 6, 9] {
            assert_eq!(buf[(x + G, 1)].symbol(), " ", "marker gap at x={x}");
            assert_eq!(buf[(x + G, 2)].symbol(), "│", "content rule at x={x}");
        }
        // The `-`/`+` markers sit over their column's first display
        // cell: '-' at buffer (4,1) over 文 at (4,2), '+' at (7,1) over
        // 日 at (7,2). Continuation cells (x=5, x=8) are skipped;
        // TestBackend leaves them empty.
        assert_eq!(buf[(4 + G, 1)].symbol(), "-");
        assert_eq!(buf[(7 + G, 1)].symbol(), "+");
        assert_eq!(buf[(4 + G, 2)].symbol(), "文");
        assert_eq!(buf[(7 + G, 2)].symbol(), "日");
        for y in 1..=2 {
            for x in [4, 5] {
                assert_eq!(buf[(x + G, y)].bg, Color::Red);
            }
            for x in [7, 8] {
                assert_eq!(buf[(x + G, y)].bg, Color::Green);
            }
        }
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    fn zwj_cluster_stays_glued_in_wide_column() {
        // The complete emoji and 中 share a width-2 grapheme column.
        let s = "👩‍❤️‍👩\n中中\n";
        let (mut app, a, b) = files_app(s, s);
        let backend = draw(&mut app);
        let buf = backend.buffer();
        // ratatui stores the whole grapheme cluster in one cell; a pad
        // space between the ZWJ and the following emoji would split it
        // into separate symbols.
        assert_eq!(buf[(1 + G, 2)].symbol(), "👩‍❤️‍👩");
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    fn colors_padding_in_wide_changed_columns() {
        let (mut app, a, b) = files_app("中文\na文\n", "中文\nb文\n");
        let buf = draw_buffer(&mut app);
        for y in 1..=3 {
            for x in [1, 2] {
                assert_eq!(buf[(x + G, y)].bg, Color::Red);
            }
            for x in [4, 5] {
                assert_eq!(buf[(x + G, y)].bg, Color::Green);
            }
            for x in [3, 6, 7, 8] {
                assert_eq!(buf[(x + G, y)].bg, Color::Reset);
            }
        }
        assert_eq!(buf[(2 + G, 3)].symbol(), " ");
        assert_eq!(buf[(5 + G, 3)].symbol(), " ");
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
            assert_eq!(buf[(1 + G, 2)].symbol(), prefix);
            for x in 1..=prefix_width {
                assert_eq!(buf[(x + G, 2)].bg, Color::Reset);
            }
            let delete_x = prefix_width + 2;
            let insert_x = prefix_width + 4;
            assert_eq!(buf[(delete_x + G, 2)].symbol(), "x");
            assert_eq!(buf[(delete_x + G, 1)].symbol(), "-");
            assert_eq!(buf[(delete_x + G, 2)].bg, Color::Red);
            assert_eq!(buf[(insert_x + G, 2)].symbol(), "y");
            assert_eq!(buf[(insert_x + G, 1)].symbol(), "+");
            assert_eq!(buf[(insert_x + G, 2)].bg, Color::Green);
            for x in [prefix_width + 1, prefix_width + 3] {
                assert_eq!(buf[(x + G, 2)].symbol(), "│");
                assert_eq!(buf[(x + G, 2)].fg, Color::DarkGray);
                assert_eq!(buf[(x + G, 2)].bg, Color::Reset);
            }
            assert_eq!(buf[(insert_x + 1 + G, 2)].bg, Color::Reset);
            let _ = std::fs::remove_file(&a);
            let _ = std::fs::remove_file(&b);
        }
    }

    #[test]
    fn content_rows_share_grapheme_layout_boundaries() {
        let (mut app, a, b) = files_app("a\u{301}x\nabz\n", "a\u{301}y\nabz\n");
        let buf = draw_buffer(&mut app);
        assert_column_backgrounds_align(&buf);
        for (y, delete_x, insert_x, deleted, inserted) in [(2, 3, 5, "x", "y"), (3, 3, 5, "b", "b")]
        {
            assert_eq!(buf[(delete_x + G, y)].symbol(), deleted);
            assert_eq!(buf[(delete_x + G, y)].bg, Color::Red);
            assert_eq!(buf[(insert_x + G, y)].symbol(), inserted);
            assert_eq!(buf[(insert_x + G, y)].bg, Color::Green);
            assert_eq!(buf[(delete_x - 1 + G, y)].bg, Color::Reset);
            assert_eq!(buf[(delete_x + 1 + G, y)].bg, Color::Reset);
            assert_eq!(buf[(insert_x + 1 + G, y)].bg, Color::Reset);
        }
        // Blank rows retain the marker's full-height column bands.
        assert_eq!(buf[(3 + G, 21)].bg, Color::Red);
        assert_eq!(buf[(5 + G, 21)].bg, Color::Green);
        assert_eq!(buf[(7 + G, 3)].symbol(), "z");
        assert_eq!(buf[(7 + G, 3)].bg, Color::Reset);
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    fn grapheme_columns_align_across_different_scalar_counts() {
        let grid = compute("a\u{301}bcx\nab\u{301}cx\n", "a\u{301}bcy\nab\u{301}cy\n");
        let display = build_display(&grid);
        assert_eq!(
            display.rows,
            ["   |-|+", "a\u{301}bc|x|y", "ab\u{301}c|x|y"]
        );
        for row in &display.rows {
            assert_eq!(rendered_width(row), display.width);
        }
        assert_eq!(display.width, 7);
    }

    #[test]
    fn screenshot_sample_aligns_x_with_b_and_keeps_inserted_text_compact() {
        let (mut app, a, b) = files_app("a\u{301}x\n中bcx\n", "a\u{301}y\n");
        let display = app.display.as_ref().unwrap();
        assert_eq!(
            display.rows,
            ["- ---|++", "a\u{301} x  |a\u{301}y", "中bcx|  "]
        );
        assert_eq!(display.width, 8);
        let buf = draw_buffer(&mut app);
        assert_column_backgrounds_align(&buf);
        assert_eq!(buf[(1 + G, 2)].symbol(), "a\u{301}");
        assert_eq!(buf[(1 + G, 3)].symbol(), "中");
        assert_eq!(buf[(3 + G, 2)].symbol(), "x");
        assert_eq!(buf[(3 + G, 3)].symbol(), "b");
        // Tabs: "-1,4" over the five-cell delete run, "+1" over the
        // two-cell insert run, a gap between them.
        assert_eq!(row_text(&buf, 1, 1 + G..9 + G), "-1,4  +1");
        assert_eq!(buf[(4 + G, 3)].symbol(), "c");
        assert_eq!(buf[(5 + G, 3)].symbol(), "x");
        assert_eq!(buf[(6 + G, 1)].symbol(), " ");
        for y in [2, 3] {
            assert_eq!(buf[(6 + G, y)].symbol(), "│");
        }
        for y in [1, 2, 3] {
            assert_eq!(buf[(6 + G, y)].bg, Color::Reset);
        }
        assert_eq!(buf[(7 + G, 2)].symbol(), "a\u{301}");
        assert_eq!(buf[(8 + G, 2)].symbol(), "y");
        assert_eq!(buf[(7 + G, 2)].bg, Color::Green);
        assert_eq!(buf[(8 + G, 2)].bg, Color::Green);
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    fn mixed_width_rows_pad_after_complete_graphemes() {
        for (old, new, prefix, delete_x, insert_x, second_cell) in [
            (
                "a\u{301}x\n中bx\n",
                "a\u{301}y\n中by\n",
                "a\u{301}",
                4,
                7,
                "b",
            ),
            ("👩‍❤️‍👩x\n中abx\n", "👩‍❤️‍👩y\n中aby\n", "👩‍❤️‍👩", 4, 6, "a"),
        ] {
            let (mut app, a, b) = files_app(old, new);
            let buf = draw_buffer(&mut app);
            assert_column_backgrounds_align(&buf);
            assert_eq!(buf[(1 + G, 2)].symbol(), prefix);
            assert_eq!(buf[(delete_x + G, 2)].symbol(), "x");
            assert_eq!(buf[(insert_x + G, 2)].symbol(), "y");
            assert_eq!(buf[(delete_x + G, 3)].symbol(), second_cell);
            assert_eq!(buf[(insert_x + G, 3)].symbol(), second_cell);
            assert_eq!(buf[(delete_x + G, 1)].symbol(), "-");
            assert_eq!(buf[(insert_x + G, 1)].symbol(), "+");
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
        assert_eq!(buf[(3 + G, 1)].symbol(), "-");
        assert_eq!(buf[(5 + G, 1)].symbol(), "+");
        for y in [2, 21] {
            assert_eq!(buf[(1 + G, y)].symbol(), "a\u{301}");
            assert_eq!(buf[(3 + G, y)].symbol(), "x");
            assert_eq!(buf[(5 + G, y)].symbol(), "y");
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
        assert_eq!(buf[(18 + G, 1)].symbol(), "-");
        assert_eq!(buf[(18 + G, 2)].symbol(), "x");
        assert_eq!(buf[(20 + G, 1)].symbol(), "+");
        assert_eq!(buf[(20 + G, 2)].symbol(), "y");
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
        // 16 prefix cells and a separator put the change at cell 17.
        assert_eq!(app.scroll_x, 17 - CHANGE_CONTEXT);
        let mut terminal = Terminal::new(TestBackend::new(12, 6)).unwrap();
        let buf = terminal.draw(|frame| app.render(frame)).unwrap().buffer;
        assert_column_backgrounds_align(buf);
        assert_eq!(
            app.scroll_x,
            17 - CHANGE_CONTEXT,
            "trailing context must prevent scroll clamping"
        );
        assert_eq!(buf[(1, 2)].symbol(), "👩‍❤️‍👩");
        assert_eq!(buf[(4, 1)].symbol(), "-");
        assert_eq!(buf[(4, 2)].symbol(), "x");
        assert_eq!(buf[(4, 2)].bg, Color::Red);
        assert_eq!(buf[(6, 1)].symbol(), "+");
        assert_eq!(buf[(6, 2)].symbol(), "y");
        assert_eq!(buf[(6, 2)].bg, Color::Green);
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
                assert_eq!(buf[(x, 2)].symbol(), "│");
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
            assert_eq!(buf[(x + G, 2)].symbol(), "|");
            assert_eq!(buf[(x + G, 2)].bg, color);
            assert_eq!(buf[(x + G, 2)].fg, Color::Black);
            assert_eq!(buf[(2 + G, 2)].bg, Color::Reset);
            assert_eq!(buf[(2 + G, 2)].fg, Color::DarkGray);
            let _ = std::fs::remove_file(&a);
            let _ = std::fs::remove_file(&b);
        }
        let (mut app, a, b) = files_app("|\n", "|\n");
        let backend = draw(&mut app);
        assert_eq!(backend.buffer()[(1 + G, 2)].fg, Color::Reset);
        assert_eq!(backend.buffer()[(1 + G, 2)].bg, Color::Reset);
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    fn colors_pure_additions_and_deletions() {
        for (old, new, color) in [("", "中\na\n", Color::Green), ("中\na\n", "", Color::Red)] {
            let (mut app, a, b) = files_app(old, new);
            let buf = draw_buffer(&mut app);
            let width = app.display.as_ref().unwrap().width as u16;
            // The whole pane, from the marker row (y=1) to its last row
            // (y=22), carries the color.
            for y in 1..=22 {
                for x in 1..=width {
                    assert_eq!(buf[(x + G, y)].bg, color);
                    assert_eq!(buf[(x + G, y)].fg, Color::Black);
                }
                assert_eq!(buf[(width + 1 + G, y)].bg, Color::Reset);
            }
            assert_ne!(buf[(1 + G, 23)].bg, color); // status bar
            assert_ne!(buf[(1 + G, 0)].bg, color); // top bar
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
        app.sidebar = Some(false);
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();
        assert_eq!(terminal.backend().buffer()[(3 + G, 2)].bg, Color::Red);
        assert_eq!(terminal.backend().buffer()[(5 + G, 2)].bg, Color::Green);
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        terminal.draw(|frame| app.render(frame)).unwrap();
        assert_eq!(app.selection, 1);
        for y in 1..=21 {
            for x in 1..=6 {
                assert_eq!(terminal.backend().buffer()[(x + G, y)].bg, Color::Reset);
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
        for (scroll_x, next_tab) in [(1, "-"), (4, "+")] {
            // scroll_x cuts inside a wide char (中 at cells 0-1, 文 at
            // 3-4): it is skipped whole, so the first drawn char sits
            // one cell past the viewport; the marker row must still
            // show the separator gap (viewport 1, buffer x = inner.x +
            // 1 = 2) over the content row's rule, with the next tab
            // right after both.
            app.scroll_x = scroll_x;
            let backend = draw(&mut app);
            let buf = backend.buffer();
            assert_eq!(
                buf[(2 + G, 1)].symbol(),
                " ",
                "marker gap at viewport 1 (scroll_x={scroll_x})"
            );
            assert_eq!(
                buf[(2 + G, 2)].symbol(),
                "│",
                "content rule at viewport 1 (scroll_x={scroll_x})"
            );
            assert_eq!(buf[(3 + G, 1)].symbol(), next_tab, "scroll_x={scroll_x}");
        }
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    fn marker_row_is_sticky_when_scrolling() {
        // 40 rows in a 22-row pane (21 below the marker row):
        // max_scroll_y is 19, so a scroll of 10 survives the render
        // clamp (with a 20-row diff the clamp pins it to 0 and the test
        // would pass vacuously).
        let mut old = String::new();
        let mut new = String::new();
        for i in 1..=39 {
            old.push_str(&format!("{i}\n"));
            new.push_str(&format!("{i}\n"));
        }
        old.push_str("L40\n");
        new.push_str("X40\n");
        let (mut app, a, b) = files_app(&old, &new);
        let _ = draw(&mut app); // sets pane_height from the real rect (22)
        assert_eq!(app.max_scroll_y(), 19);
        app.scroll_y = 10;
        let backend = draw(&mut app);
        let buf = backend.buffer();
        // Marker "-|+|" starts at x=1 (pane padding): '-' over the
        // delete column, '+' over the insert — still at the top row
        // after vertical scroll.
        assert_eq!(buf[(1 + G, 1)].symbol(), "-");
        assert_eq!(buf[(3 + G, 1)].symbol(), "+");
        for y in [1, 2, 21] {
            assert_eq!(buf[(1 + G, y)].bg, Color::Red);
            assert_eq!(buf[(3 + G, y)].bg, Color::Green);
        }
        assert_eq!(app.scroll_y, 10, "clamp must keep a valid scroll");
        // Line 30 sits on the pane's second-to-last row: '3' at col 0,
        // '0' at col 5 (delete|insert cells, then the '0' of "30").
        assert_eq!(buf[(1 + G, 21)].symbol(), "3");
        assert_eq!(buf[(5 + G, 21)].symbol(), "0");
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    fn sidebar_shows_status_letters() {
        let mut app = git_app(vec![("foo\n", "bar\n"), ("a\n", "b\n")]);
        let backend = draw(&mut app);
        let buf = backend.buffer();
        // A title row under the top bar, then one row per file: the
        // selection mark, the status letter, two spaces, the path.
        assert_eq!(row_text(buf, 1, 0..24), " FILES              1/2 ");
        assert_eq!(row_text(buf, 2, 0..10), "▌M  f0.txt");
        assert_eq!(row_text(buf, 3, 0..10), " M  f1.txt");
        assert_eq!(buf[(1, 2)].fg, Color::Yellow);
        // The selected row is highlighted across the sidebar.
        for x in 1..24 {
            assert_eq!(buf[(x, 2)].bg, Color::DarkGray, "x={x}");
            assert_eq!(buf[(x, 3)].bg, Color::Reset, "x={x}");
        }
        // The divider follows the 24-column sidebar.
        assert_eq!(buf[(24, 2)].symbol(), "│");
    }

    #[test]
    fn sidebar_keeps_the_selection_in_view() {
        let files: Vec<(&str, &str)> = vec![("a\n", "b\n"); 30];
        let mut app = git_app(files);
        let mut terminal = Terminal::new(TestBackend::new(80, 10)).unwrap();
        // 10 rows: top bar, sidebar title, 7 entries, status bar.
        for _ in 0..12 {
            app.handle_key(key(KeyCode::Tab));
        }
        let buf = terminal
            .draw(|frame| app.render(frame))
            .unwrap()
            .buffer
            .clone();
        assert_eq!(row_text(&buf, 8, 0..11), "▌M  f12.txt");
        assert_eq!(row_text(&buf, 2, 0..11), " M  f6.txt ");
        for _ in 0..10 {
            app.handle_key(key(KeyCode::BackTab));
        }
        let buf = terminal
            .draw(|frame| app.render(frame))
            .unwrap()
            .buffer
            .clone();
        assert_eq!(row_text(&buf, 2, 0..10), "▌M  f2.txt");
    }

    #[test]
    fn sidebar_hidden_diff_takes_full_width() {
        let mut app = git_app(vec![("foo\nbar baz\nquux\n", "foo\nbar qaz\nquux\n")]);
        let buf_before = draw(&mut app);
        assert_ne!(buf_before.buffer()[(6 + G, 1)].symbol(), "-"); // (6,1) is inside the sidebar
        app.sidebar = Some(false);
        let buf_after = draw(&mut app);
        // The full-width pane keeps its blank first cell: inner starts
        // at x=1, marker "    |-|+|  " → gap at (5,1), '-' at (6,1).
        assert_eq!(buf_after.buffer()[(5 + G, 1)].symbol(), " "); // marker gap at (5,1)
        assert_eq!(buf_after.buffer()[(6 + G, 1)].symbol(), "-");
    }

    #[test]
    fn sidebar_starts_hidden_on_narrow_terminals() {
        let mut app = git_app(vec![("foo\nbar baz\nquux\n", "foo\nbar qaz\nquux\n")]);
        let mut narrow = Terminal::new(TestBackend::new(79, 24)).unwrap();
        let buf = narrow
            .draw(|frame| app.render(frame))
            .unwrap()
            .buffer
            .clone();
        assert!(!app.sidebar_visible());
        assert_eq!(
            buf[(6 + G, 1)].symbol(),
            "-",
            "the diff takes the full width"
        );
        // `e` still shows it, and the choice outlives a resize.
        let r = app.handle_key(key(KeyCode::Char('e')));
        assert!(r.changed);
        assert!(app.sidebar_visible());
        let buf = narrow
            .draw(|frame| app.render(frame))
            .unwrap()
            .buffer
            .clone();
        assert_eq!(row_text(&buf, 2, 0..10), "▌M  f0.txt");
        let _ = draw(&mut app);
        assert!(app.sidebar_visible());
        app.handle_key(key(KeyCode::Char('e')));
        let _ = draw(&mut app);
        assert!(!app.sidebar_visible(), "hidden on request even when wide");
    }

    #[test]
    fn placeholder_for_no_changes() {
        let mut app = git_app(vec![]);
        let backend = draw(&mut app);
        let buf = backend.buffer();
        // Centered in the 22-row pane: the title, then the revisions.
        let row = row_text(buf, 11, 0..80);
        assert_eq!(
            row.trim(),
            "No changes",
            "expected placeholder text, got {row:?}"
        );
        assert_eq!(row_text(buf, 12, 0..80).trim(), "HEAD~1 → HEAD");
        assert!(!app.sidebar_visible(), "no sidebar without files");
    }

    #[test]
    fn max_scroll_y_reaches_bottom_content_rows() {
        // 30 content rows in the 80x24 terminal: between the top bar
        // and the status bar the diff pane has height 22, and the
        // sticky marker row occupies one of those rows, so 21 content
        // rows are visible and the last line only becomes reachable at
        // scroll_y = 9 (h - pane_height + 1). Without the marker row
        // (h - pane_height = 8) line 30 would stay unviewable.
        let mut old = String::new();
        let mut new = String::new();
        for i in 1..=29 {
            old.push_str(&format!("L{i:02}\n"));
            new.push_str(&format!("L{i:02}\n"));
        }
        old.push_str("L30\n");
        new.push_str("X30\n"); // only line 30 differs (col 0: 'L' vs 'X')
        let (mut app, a, b) = files_app(&old, &new);
        let _ = draw(&mut app); // sets pane_height from the real rect (22)
        assert_eq!(app.max_scroll_y(), 9);
        app.scroll_y = 9;
        let backend = draw(&mut app);
        let buf = backend.buffer();
        // Last content row "L|X|3|0" (line 30, old side 'L' / new side 'X')
        // sits at the bottom row of the pane: y = 22.
        assert_eq!(buf[(1 + G, 22)].symbol(), "L");
        assert_eq!(buf[(3 + G, 22)].symbol(), "X");
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    fn max_scroll_x_reaches_last_columns() {
        // One 100-char row: the four-cell line-number gutter and the
        // blank cell at the right edge leave a 75-cell pane, so the row
        // scrolls to 25. Scrolling any less would leave its last
        // columns unviewable.
        let old = "a".repeat(100);
        let new = "a".repeat(100); // identical single-line files: width 100
        let (mut app, a, b) = files_app(&old, &new);
        let _ = draw(&mut app); // sets pane_width from the real rect (80)
        assert_eq!(app.max_scroll_x(), 25);
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
        assert_eq!(buf[(3 + G, 22)].symbol(), "X", "bottom row visible");
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

    fn key_release(code: KeyCode) -> KeyEvent {
        KeyEvent::new_with_kind(code, KeyModifiers::NONE, KeyEventKind::Release)
    }

    #[test]
    fn key_releases_are_ignored_but_repeats_scroll() {
        let tall = "a\n".repeat(50);
        let mut app = git_app(vec![(&tall, &tall), ("b\n", "c\n")]);
        let _ = draw(&mut app);
        let before = app.view_state();
        for mut event in [
            key(KeyCode::Char('q')),
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            KeyEvent::new(
                KeyCode::Char('C'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ),
            key(KeyCode::Char('i')),
            key(KeyCode::Char('?')),
            key(KeyCode::Char('j')),
            key(KeyCode::Tab),
        ] {
            event.kind = KeyEventKind::Release;
            let r = app.handle_key(event);
            assert!(!r.quit && !r.changed, "release must do nothing: {event:?}");
            assert_eq!(app.view_state(), before);
        }
        assert!(app.handle_key(key(KeyCode::Char('j'))).changed);
        let r = app.handle_key(key_release(KeyCode::Char('j')));
        assert!(!r.quit && !r.changed);
        assert_eq!(app.scroll_y, 1);
        let r = app.handle_key(KeyEvent::new_with_kind(
            KeyCode::Char('j'),
            KeyModifiers::NONE,
            KeyEventKind::Repeat,
        ));
        assert!(!r.quit && r.changed);
        assert_eq!(app.scroll_y, 2);
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
        assert!(app.sidebar_visible());
        app.handle_key(key(KeyCode::Char('e')));
        assert!(!app.sidebar_visible());
        app.handle_key(key(KeyCode::Char('e')));
        assert!(app.sidebar_visible());
        let (mut files_app, a, b) = files_app("a\n", "b\n");
        let r = files_app.handle_key(key(KeyCode::Char('e'))); // no sidebar in files mode
        assert!(!r.changed);
        assert_eq!(files_app.sidebar, None);
        assert!(!files_app.sidebar_visible());
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    fn n_p_scroll_only_sideways() {
        let (mut app, a, b) = files_app("foo\nbar baz\nquux\n", "foo\nbar qaz\nquux\n");
        app.scroll_y = 2;
        app.handle_key(key(KeyCode::Char('n')));
        // The change (|b|q|) starts at cell 5, after "bar " and a separator.
        assert_eq!(app.display.as_ref().unwrap().changes[0].cells(), 5..8);
        assert_eq!(app.scroll_x, 5 - CHANGE_CONTEXT);
        assert_eq!(app.scroll_y, 2, "jumping must not hint at changed lines");
        app.handle_key(key(KeyCode::Char('p')));
        assert_eq!(app.scroll_x, 0);
        assert_eq!(app.scroll_y, 2);
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    fn n_p_visit_every_change_on_one_line_with_its_deletion_in_view() {
        // Three changes on the same line, then enough text to scroll to
        // each of them without clamping.
        let gap = "A".repeat(10);
        let tail = "z".repeat(100);
        let (mut app, a, b) = files_app(
            &format!("ppppp1{gap}2{gap}3{tail}\n"),
            &format!("ppppp4{gap}5{gap}6{tail}\n"),
        );
        let _ = draw_buffer(&mut app);
        let starts: Vec<usize> = app
            .display
            .as_ref()
            .unwrap()
            .changes
            .iter()
            .map(|c| c.cells().start)
            .collect();
        assert_eq!(starts.len(), 3);
        // The deleted cell sits CHANGE_CONTEXT cells into the pane, with
        // the inserted one two cells after it.
        let left = G + 1 + CHANGE_CONTEXT as u16;
        for (start, (old, new)) in starts.iter().zip([("1", "4"), ("2", "5"), ("3", "6")]) {
            app.handle_key(key(KeyCode::Char('n')));
            assert_eq!(app.scroll_x, start - CHANGE_CONTEXT);
            let buf = draw_buffer(&mut app);
            assert_eq!(buf[(left, 1)].symbol(), "-");
            assert_eq!(buf[(left, 2)].symbol(), old);
            assert_eq!(buf[(left, 2)].bg, Color::Red);
            assert_eq!(buf[(left + 2, 1)].symbol(), "+");
            assert_eq!(buf[(left + 2, 2)].symbol(), new);
        }
        // Past the last change, n wraps around to the first.
        app.handle_key(key(KeyCode::Char('n')));
        assert_eq!(app.scroll_x, starts[0] - CHANGE_CONTEXT);
        // p walks back, returns to the left edge, then wraps to the last.
        app.handle_key(key(KeyCode::Char('n')));
        app.handle_key(key(KeyCode::Char('p')));
        assert_eq!(app.scroll_x, starts[0] - CHANGE_CONTEXT);
        app.handle_key(key(KeyCode::Char('p')));
        assert_eq!(app.scroll_x, 0);
        app.handle_key(key(KeyCode::Char('p')));
        assert_eq!(app.scroll_x, starts[2] - CHANGE_CONTEXT);
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    fn changes_near_the_end_share_the_last_stop() {
        let prefix = "p".repeat(30);
        let (mut app, a, b) = files_app(&format!("{prefix}xAAx\n"), &format!("{prefix}yAAy\n"));
        let mut terminal = Terminal::new(TestBackend::new(16, 6)).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();
        let max = app.max_scroll_x();
        assert!(max > 0);
        app.handle_key(key(KeyCode::Char('n')));
        assert_eq!(app.scroll_x, max);
        // Both changes are in view there, so n stays put instead of
        // scrolling past the end.
        let display = app.display.as_ref().unwrap();
        assert_eq!(display.changes.len(), 2);
        assert!(display.changes.iter().all(|c| c.cells().start >= max));
        assert!(!app.handle_key(key(KeyCode::Char('n'))).changed);
        app.handle_key(key(KeyCode::Char('p')));
        assert_eq!(app.scroll_x, 0);
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    fn layout_has_no_phantom_trailing_separator() {
        // Only the final column differs ("aa" → "ab"): steps Match,
        // Delete, Insert; the insert group ends at the last step, where
        // build_rows draws no trailing `|`. The layout must not count a
        // phantom separator after it (total width 3 steps × 1 + 2 real
        // separators = 5; without the guard it would be 6).
        let grid = compute("aa\n", "ab\n");
        let display = build_display(&grid);
        assert_eq!(display.width, 5);
        assert_eq!(display.changes[0].cells(), 2..5);
    }

    const OLD_CPP: &str =
        "#include <cstdio>\n\nint main() {\n  std::printf(\"Hello, World\\n\");\n  return 0;\n}\n";
    const NEW_CPP: &str = "#include <print>\n\nauto main() -> int {\n  std::println(\"Hello, World\");\n  return 0;\n}\n";

    #[test]
    fn diff_stats_count_a_delete_run_and_its_insert_run_once() {
        // The README example: -1/+1, -4/+4, a lone insert, a lone delete.
        assert_eq!(
            build_display(&compute(OLD_CPP, NEW_CPP)).stats,
            DiffStats {
                changes: 4,
                deleted: 19,
                inserted: 18,
            }
        );
        assert_eq!(
            build_display(&compute("x\n", "xab\n")).stats,
            DiffStats {
                changes: 1,
                deleted: 0,
                inserted: 2,
            }
        );
        assert_eq!(
            build_display(&compute("a\n", "a\n")).stats,
            DiffStats::default()
        );
    }

    #[test]
    fn changes_pair_runs_and_record_their_columns_and_cells() {
        // Display: "--|++| |---------------|+++++++++++++++| |+|...|--|"
        let display = build_display(&compute(OLD_CPP, NEW_CPP));
        let run = |steps: Range<usize>, column, cells: Range<usize>| Run {
            steps,
            column,
            cells,
        };
        assert_eq!(
            display.changes,
            [
                Change {
                    delete: Some(run(0..2, 0, 0..2)),
                    insert: Some(run(2..4, 0, 3..5)),
                },
                Change {
                    delete: Some(run(5..20, 3, 8..23)),
                    insert: Some(run(20..35, 3, 24..39)),
                },
                Change {
                    delete: None,
                    insert: Some(run(36..37, 19, 42..43)),
                },
                Change {
                    delete: Some(run(45..47, 27, 53..55)),
                    insert: None,
                },
            ]
        );
        assert_eq!(display.changes[1].cells(), 8..39);
        assert_eq!(display.changes[2].cells(), 42..43);
    }

    #[test]
    fn tab_labels_shorten_to_fit() {
        let run = |column, count: usize| Run {
            steps: 0..count,
            column,
            cells: 0..0,
        };
        assert_eq!(tab_label(StepKind::Delete, &run(3, 15), 15), "-4,15");
        assert_eq!(tab_label(StepKind::Delete, &run(3, 15), 4), "-4");
        assert_eq!(tab_label(StepKind::Insert, &run(3, 15), 1), "+");
        assert_eq!(tab_label(StepKind::Insert, &run(19, 1), 5), "+20");
        assert_eq!(tab_label(StepKind::Delete, &run(27, 2), 2), "-");
        assert_eq!(tab_label(StepKind::Delete, &run(0, 2), 0), "");
    }

    #[test]
    fn true_color_separates_tabs_cells_and_fill() {
        let theme = Theme::new(ColorDepth::TrueColor, Scheme::Dark);
        let (mut app, a, b) = files_app_with("foo\nbar baz\n", "foo\nbar qaz\n", theme.clone());
        let buf = draw_buffer(&mut app);
        // The gutter takes x=0..=3. Marker row y=1, file rows y=2..=3,
        // fill below down to y=22. Every file row of a changed column
        // gets the same tint.
        for (x, tab, cell, fill) in [
            (9, theme.delete_tab, theme.delete, theme.delete_fill),
            (11, theme.insert_tab, theme.insert, theme.insert_fill),
        ] {
            assert_eq!(buf[(x, 1)].bg, tab.bg.unwrap());
            assert_eq!(buf[(x, 1)].fg, tab.fg.unwrap());
            for y in [2, 3] {
                assert_eq!(buf[(x, y)].bg, cell.bg.unwrap());
                assert_eq!(buf[(x, y)].fg, cell.fg.unwrap());
            }
            for y in 4..=22 {
                assert_eq!(buf[(x, y)].bg, fill.bg.unwrap());
            }
        }
        assert_eq!(buf[(9, 3)].symbol(), "b");
        assert_eq!(buf[(11, 3)].symbol(), "q");
        assert_eq!(row_text(&buf, 1, 9..12), "- +");
        // Separators: a gap between tabs, a rule between file cells.
        assert_eq!(buf[(8, 1)].bg, Color::Reset);
        assert_eq!(buf[(10, 3)].symbol(), "│");
        assert_eq!(buf[(10, 3)].fg, theme.separator.fg.unwrap());
        assert_eq!(buf[(10, 3)].bg, Color::Reset);
        assert_eq!(buf[(4, 3)].bg, Color::Reset);
        // Line numbers all look alike: they number rows, not changes.
        for (y, text) in [(2, "  1 "), (3, "  2 "), (4, "  ~ ")] {
            assert_eq!(row_text(&buf, y, 0..4), text);
        }
        assert_eq!(buf[(2, 2)].fg, theme.muted.fg.unwrap());
        assert_eq!(buf[(2, 3)].fg, theme.muted.fg.unwrap());
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    fn line_numbers_stay_put_and_give_way_on_narrow_panes() {
        let line = "a".repeat(200);
        let (mut app, a, b) = files_app(&format!("{line}\nx\n"), &format!("{line}\ny\n"));
        app.scroll_x = 50;
        let buf = draw_buffer(&mut app);
        assert_eq!(row_text(&buf, 1, 0..4), "    ");
        assert_eq!(row_text(&buf, 2, 0..5), "  1 a");
        assert_eq!(row_text(&buf, 3, 0..4), "  2 ");
        assert_eq!(buf[(2, 2)].fg, Color::DarkGray);
        assert_eq!(buf[(2, 3)].fg, Color::DarkGray);
        assert_eq!(row_text(&buf, 4, 0..4), "  ~ ");
        // A diff area under 20 columns keeps them all for the diff.
        let mut narrow = Terminal::new(TestBackend::new(19, 6)).unwrap();
        let buf = narrow
            .draw(|frame| app.render(frame))
            .unwrap()
            .buffer
            .clone();
        assert_eq!(row_text(&buf, 2, 0..3), " aa");
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    fn line_numbers_widen_past_two_digits() {
        let (mut app, a, b) = files_app(&"x\n".repeat(120), &"y\n".repeat(120));
        app.scroll_y = 99;
        let buf = draw_buffer(&mut app);
        assert_eq!(row_text(&buf, 1, 0..6), "     -");
        assert_eq!(row_text(&buf, 2, 0..6), " 100 x");
        assert_eq!(row_text(&buf, 22, 0..6), " 120 x");
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    fn tab_labels_stick_to_the_pane_edge() {
        // Ten inserted columns from new column 3: cells 3..13.
        let (mut app, a, b) = files_app("ab\n", "ab0123456789\n");
        let mut narrow = Terminal::new(TestBackend::new(10, 6)).unwrap();
        let buf = narrow
            .draw(|frame| app.render(frame))
            .unwrap()
            .buffer
            .clone();
        assert_eq!(row_text(&buf, 1, 1..9), "   +3,10");
        app.scroll_x = 5;
        let buf = narrow
            .draw(|frame| app.render(frame))
            .unwrap()
            .buffer
            .clone();
        assert_eq!(row_text(&buf, 1, 1..9), "+3,10   ");
        assert_eq!(row_text(&buf, 2, 1..9), "23456789");
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    fn overview_bar_maps_changes_and_the_view() {
        // Display: delete, gap, insert, gap, then 199 matched columns:
        // 203 cells over a 75-cell bar on the pane's last row (y=22).
        let rest = "a".repeat(199);
        let (mut app, a, b) = files_app(&format!("x{rest}\n"), &format!("y{rest}\n"));
        let buf = draw_buffer(&mut app);
        assert_eq!(app.pane_height, 21, "the bar takes a pane row");
        // Bar cells 0 and 1 hold the delete and insert runs; the pane
        // shows cells 0..=27, so `┃` marks both ends of that span.
        let bar = |viewed: std::ops::RangeInclusive<usize>| -> String {
            (0..75)
                .map(|cell| match cell {
                    _ if cell == *viewed.start() || cell == *viewed.end() => '┃',
                    0 | 1 => '▌',
                    _ if viewed.contains(&cell) => '━',
                    _ => '─',
                })
                .collect()
        };
        assert_eq!(row_text(&buf, 22, 4..79), bar(0..=27));
        assert_eq!(buf[(4, 22)].fg, Color::Cyan);
        assert_eq!(buf[(5, 22)].fg, Color::Green);
        assert_eq!(buf[(6, 22)].fg, Color::Cyan);
        assert_eq!(buf[(40, 22)].fg, Color::DarkGray);
        assert_eq!(row_text(&buf, 22, 0..4), "    ");
        app.scroll_x = 128;
        let buf = draw_buffer(&mut app);
        assert_eq!(row_text(&buf, 22, 4..79), bar(47..=74));
        assert_eq!(buf[(4, 22)].fg, Color::Red);
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    fn overview_bar_marks_where_both_kinds_meet() {
        // 304 cells over a 25-cell bar: the first bar cell covers both
        // the delete and the insert run. Scrolled away from it, it is
        // not a `┃` end.
        let rest = "a".repeat(300);
        let (mut app, a, b) = files_app(&format!("x{rest}\n"), &format!("y{rest}\n"));
        let mut small = Terminal::new(TestBackend::new(30, 12)).unwrap();
        app.scroll_x = 200;
        let buf = small
            .draw(|frame| app.render(frame))
            .unwrap()
            .buffer
            .clone();
        assert_eq!(buf[(4, 10)].symbol(), "▌");
        assert_eq!(buf[(4, 10)].fg, Color::Red);
        assert_eq!(buf[(4, 10)].bg, Color::Green);
        assert_eq!(buf[(5, 10)].symbol(), "─");
        // A pane under eight rows keeps them all for the diff.
        let mut short = Terminal::new(TestBackend::new(30, 9)).unwrap();
        let buf = short
            .draw(|frame| app.render(frame))
            .unwrap()
            .buffer
            .clone();
        assert_eq!(app.pane_height, 7);
        assert!(!row_text(&buf, 7, 0..30).contains('━'));
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    fn overview_ticks_outside_the_view_are_dim() {
        let theme = Theme::new(ColorDepth::TrueColor, Scheme::Dark);
        let rest = "a".repeat(199);
        let (mut app, a, b) =
            files_app_with(&format!("x{rest}\n"), &format!("y{rest}\n"), theme.clone());
        app.scroll_x = 128;
        let buf = draw_buffer(&mut app);
        assert_eq!(buf[(4, 22)].fg, theme.delete.bg.unwrap());
        assert_eq!(buf[(5, 22)].fg, theme.insert.bg.unwrap());
        app.scroll_x = 1;
        let buf = draw_buffer(&mut app);
        assert_eq!(buf[(5, 22)].symbol(), "▌");
        assert_eq!(buf[(5, 22)].fg, theme.insert_mark.fg.unwrap());
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    fn no_overview_bar_when_the_diff_fits() {
        let (mut app, a, b) = files_app(OLD_CPP, NEW_CPP);
        let buf = draw_buffer(&mut app);
        assert_eq!(app.pane_height, 22);
        let row = row_text(&buf, 22, 0..80);
        assert!(!row.contains('━') && !row.contains('─'), "{row:?}");
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn wheel_scrolls_the_diff() {
        let line = "a".repeat(200);
        let text = format!("{line}\n").repeat(40);
        let (mut app, a, b) = files_app(&text, &text.replacen('a', "b", 1));
        let _ = draw(&mut app);
        assert!(app.handle_mouse(mouse(MouseEventKind::ScrollDown, 40, 10)));
        assert_eq!(app.scroll_y, WHEEL_ROWS);
        assert!(app.handle_mouse(mouse(MouseEventKind::ScrollUp, 40, 10)));
        assert_eq!(app.scroll_y, 0);
        assert!(!app.handle_mouse(mouse(MouseEventKind::ScrollUp, 40, 10)));
        let mut sideways = mouse(MouseEventKind::ScrollDown, 40, 10);
        sideways.modifiers = KeyModifiers::SHIFT;
        assert!(app.handle_mouse(sideways));
        assert_eq!((app.scroll_y, app.scroll_x), (0, WHEEL_CELLS));
        assert!(app.handle_mouse(mouse(MouseEventKind::ScrollRight, 40, 10)));
        assert_eq!(app.scroll_x, 2 * WHEEL_CELLS);
        assert!(app.handle_mouse(mouse(MouseEventKind::ScrollLeft, 40, 10)));
        assert_eq!(app.scroll_x, WHEEL_CELLS);
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    fn mouse_picks_files_in_the_sidebar() {
        let mut app = git_app(vec![("a\n", "b\n"), ("c\n", "d\n"), ("e\n", "f\n")]);
        let _ = draw(&mut app);
        // The wheel over the list moves the selection.
        assert!(app.handle_mouse(mouse(MouseEventKind::ScrollDown, 5, 5)));
        assert_eq!(app.selection, 1);
        // Entries start below the title row at y=1.
        assert!(app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 5, 4)));
        assert_eq!(app.selection, 2);
        assert!(!app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 5, 1)));
        assert!(!app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 5, 9)));
        assert_eq!(app.selection, 2);
    }

    #[test]
    fn mouse_on_the_overview_bar_moves_the_view() {
        let rest = "a".repeat(199);
        let (mut app, a, b) = files_app(&format!("x{rest}\n"), &format!("y{rest}\n"));
        let _ = draw(&mut app);
        // The bar spans x=4..79 on y=22; its last cell is the far right.
        let click = MouseEventKind::Down(MouseButton::Left);
        assert!(app.handle_mouse(mouse(click, 78, 22)));
        assert_eq!(app.scroll_x, app.max_scroll_x());
        assert!(app.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 41, 22)));
        // Cell 37 of 75 covers display cell 101: centered in 75 cells.
        assert_eq!(app.scroll_x, 101 - 37);
        assert!(app.handle_mouse(mouse(click, 4, 22)));
        assert_eq!(app.scroll_x, 0);
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    fn a_click_closes_the_help_popup() {
        let (mut app, a, b) = files_app("a\n", "b\n");
        app.handle_key(key(KeyCode::Char('?')));
        assert!(!app.handle_mouse(mouse(MouseEventKind::ScrollDown, 40, 10)));
        assert!(app.help);
        assert!(app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 40, 10)));
        assert!(!app.help);
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    fn inspection_ignores_key_releases() {
        let mut app = git_app(vec![("abc\nabc\n", "abd\nabd\n")]);
        let _ = draw(&mut app);
        for (code, cursor) in [
            (KeyCode::Char('i'), Some(Cursor { row: 0, step: 0 })),
            (KeyCode::Char('l'), Some(Cursor { row: 0, step: 1 })),
            (KeyCode::Char('j'), Some(Cursor { row: 1, step: 1 })),
            (KeyCode::Char('i'), None),
        ] {
            let r = app.handle_key(key(code));
            assert!(!r.quit && r.changed);
            assert_eq!(app.inspect, cursor);
            let before = app.view_state();
            let r = app.handle_key(key_release(code));
            assert!(!r.quit && !r.changed);
            assert_eq!(app.view_state(), before);
        }
    }

    #[test]
    fn inspecting_shows_what_a_cell_holds() {
        // "é" as one code point in the old file, as e + U+0301 in the new.
        let (mut app, a, b) = files_app("caf\u{e9}\n", "cafe\u{301}\n");
        let _ = draw(&mut app);
        let status = |app: &mut App| row_text(&draw_buffer(app), 23, 0..80);
        assert!(app.handle_key(key(KeyCode::Char('i'))).changed);
        assert_eq!(app.inspect, Some(Cursor { row: 0, step: 0 }));
        let bar = status(&mut app);
        assert!(
            bar.starts_with(" INSPECT   line 1  old col 1  new col 1   \"c\" 1 cell   U+0063"),
            "{bar:?}"
        );
        // The key hints fit on a wider screen.
        let mut wide = Terminal::new(TestBackend::new(100, 24)).unwrap();
        let buf = wide.draw(|frame| app.render(frame)).unwrap().buffer.clone();
        let bar = row_text(&buf, 23, 0..100);
        assert!(bar.ends_with("hjkl move   esc done "), "{bar:?}");
        let _ = draw(&mut app);
        for _ in 0..3 {
            app.handle_key(key(KeyCode::Char('l')));
        }
        let bar = status(&mut app);
        assert!(
            bar.starts_with(" INSPECT   line 1  old col 4   \"\u{e9}\" 1 cell   U+00E9 "),
            "{bar:?}"
        );
        app.handle_key(key(KeyCode::Char('l')));
        let bar = status(&mut app);
        assert!(
            bar.starts_with(" INSPECT   line 1  new col 4   \"e\u{301}\" 1 cell   U+0065 U+0301"),
            "{bar:?}"
        );
        // Display cells: "caf", gap, delete (cell 4), gap, insert
        // (cell 6). The cursor's cell is highlighted.
        let buf = draw_buffer(&mut app);
        assert_eq!(buf[(7 + G, 2)].bg, Color::Cyan);
        assert_eq!(buf[(5 + G, 2)].bg, Color::Red);
        // The cursor stops at the last column; Esc ends inspecting.
        assert!(!app.handle_key(key(KeyCode::Char('l'))).changed);
        assert!(app.handle_key(key(KeyCode::Esc)).changed);
        assert_eq!(app.inspect, None);
        assert!(status(&mut app).starts_with(" FILES "));
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    fn inspect_cursor_moves_by_rows_and_stays_in_view() {
        let (mut app, a, b) = files_app(&"x\n".repeat(50), &"y\n".repeat(50));
        let _ = draw(&mut app);
        app.handle_key(key(KeyCode::Char('i')));
        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(app.inspect, Some(Cursor { row: 1, step: 0 }));
        app.handle_key(key(KeyCode::Char('G')));
        assert_eq!(app.inspect, Some(Cursor { row: 49, step: 0 }));
        assert_eq!(app.scroll_y, app.max_scroll_y(), "the cursor row is shown");
        app.handle_key(key(KeyCode::Char('g')));
        assert_eq!((app.inspect.unwrap().row, app.scroll_y), (0, 0));
        // Other keys keep working; switching files ends inspecting.
        assert!(app.handle_key(key(KeyCode::Char('q'))).quit);
        let mut git = git_app(vec![("a\n", "b\n"), ("c\n", "d\n")]);
        git.handle_key(key(KeyCode::Char('i')));
        assert!(git.inspect.is_some());
        git.handle_key(key(KeyCode::Tab));
        assert_eq!(git.inspect, None);
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    fn clicking_a_cell_inspects_it() {
        let (mut app, a, b) = files_app(OLD_CPP, NEW_CPP);
        let _ = draw(&mut app);
        let click = MouseEventKind::Down(MouseButton::Left);
        // Line 3 is y=4; display cell 25 ("o" of "auto main") is x=4+25.
        assert!(app.handle_mouse(mouse(click, 29, 4)));
        assert_eq!(app.inspect, Some(Cursor { row: 2, step: 21 }));
        // The marker row, gutter, and separators are not cells.
        app.inspect = None;
        assert!(!app.handle_mouse(mouse(click, 29, 1)));
        assert!(!app.handle_mouse(mouse(click, 2, 4)));
        assert!(!app.handle_mouse(mouse(click, 6, 4)));
        assert_eq!(app.inspect, None);
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    fn count_file_reports_counts_binaries_and_failures() {
        let spec = RevSpec {
            old: Source::Rev("HEAD~1".into()),
            new: Source::Rev("HEAD".into()),
            diff_args: vec![],
            toplevel: String::new(),
            prefix: String::new(),
        };
        let file = |path: &str| ChangedFile {
            status: Status::Modified,
            old_path: path.into(),
            new_path: path.into(),
        };
        let mut git = FakeGit::default();
        git.set(&["cat-file", "-p", "HEAD~1:a"], Some("foo\n".into()));
        git.set(&["cat-file", "-p", "HEAD:a"], Some("fox\n".into()));
        git.set_binary(&["cat-file", "-p", "HEAD~1:b"]);
        git.set(&["cat-file", "-p", "HEAD:b"], Some("x\n".into()));
        assert_eq!(
            count_file(&git, &spec, &file("a")),
            FileStats::Counted(DiffStats {
                changes: 1,
                deleted: 1,
                inserted: 1,
            })
        );
        assert_eq!(count_file(&git, &spec, &file("b")), FileStats::Binary);
        assert_eq!(count_file(&git, &spec, &file("c")), FileStats::Failed);
    }

    #[test]
    fn sidebar_shows_each_files_counts_once_known() {
        let mut app = git_app(vec![("foo\n", "bar\n"), ("a\n", "a\n"), ("x\n", "y\n")]);
        // The loaded file is counted right away; the others wait.
        let buf = draw_buffer(&mut app);
        assert_eq!(row_text(&buf, 2, 0..24), "▌M  f0.txt        -3 +3 ");
        assert_eq!(row_text(&buf, 3, 0..24), " M  f1.txt              ");
        assert_eq!(buf[(18, 2)].fg, Color::Red);
        assert_eq!(buf[(21, 2)].fg, Color::Green);
        app.record_stats(1, FileStats::Counted(DiffStats::default()));
        app.record_stats(2, FileStats::Binary);
        let buf = draw_buffer(&mut app);
        assert_eq!(row_text(&buf, 3, 0..24), format!(" M  f1.txt{:>13} ", "="));
        assert_eq!(
            row_text(&buf, 4, 0..24),
            format!(" M  f2.txt{:>13} ", "bin")
        );
        app.record_stats(2, FileStats::Failed);
        let buf = draw_buffer(&mut app);
        assert_eq!(
            row_text(&buf, 4, 0..24),
            format!(" M  f2.txt{:>13} ", "err")
        );
    }

    #[test]
    fn counts_leave_out_a_side_with_none() {
        let (mut app, a, b) = files_app("x\n", "xab\n");
        app.labels = ("a".into(), "b".into());
        let bar = row_text(&draw_buffer(&mut app), 0, 0..80);
        assert!(bar.ends_with("  1 change   +2 cols "), "{bar:?}");
        let mut git = git_app(vec![("ab\n", "a\n"), ("x\n", "y\n")]);
        let buf = draw_buffer(&mut git);
        assert_eq!(row_text(&buf, 2, 0..24), format!("▌M  f0.txt{:>13} ", "-1"));
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    fn background_counts_do_not_replace_shown_ones() {
        let mut app = git_app(vec![("foo\n", "bar\n"), ("x\n", "y\n")]);
        let shown = app.file_stats[0];
        assert!(shown.is_some());
        assert!(!app.fill_stats(0, FileStats::Binary));
        assert_eq!(app.file_stats[0], shown);
        assert!(app.fill_stats(1, FileStats::Binary));
        assert!(!app.fill_stats(1, FileStats::Failed));
        assert_eq!(app.file_stats[1], Some(FileStats::Binary));
        // Loading a file counts it again from the shown diff.
        app.handle_key(key(KeyCode::Tab));
        assert!(matches!(app.file_stats[1], Some(FileStats::Counted(_))));
    }

    #[test]
    fn inspect_status_survives_huge_graphemes() {
        // One grapheme with thousands of combining marks: its code
        // points alone are far wider than u16.
        let old = format!("a{}\n", "\u{301}".repeat(9352));
        let (mut app, a, b) = files_app(&old, "b\n");
        let _ = draw(&mut app);
        app.handle_key(key(KeyCode::Char('i')));
        let bar = row_text(&draw_buffer(&mut app), 23, 0..80);
        assert!(bar.starts_with(" INSPECT "), "{bar:?}");
        assert!(!bar.contains("esc done"), "{bar:?}");
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    fn no_color_marks_changes_without_colors() {
        let (mut app, a, b) = files_app_with(
            "foo\nbar baz\n",
            "foo\nbar qaz\n",
            Theme::new(ColorDepth::NoColor, Scheme::Dark),
        );
        let buf = draw_buffer(&mut app);
        for y in 0..24 {
            for x in 0..80 {
                assert_eq!(buf[(x, y)].fg, Color::Reset, "({x}, {y})");
                assert_eq!(buf[(x, y)].bg, Color::Reset, "({x}, {y})");
            }
        }
        assert_eq!(buf[(6 + G, 1)].symbol(), "-");
        assert_eq!(buf[(8 + G, 1)].symbol(), "+");
        // File cells of changed columns are reversed; the space below
        // the file is not.
        for x in [6, 8] {
            for y in [2, 3] {
                assert!(buf[(x + G, y)].modifier.contains(Modifier::REVERSED));
            }
            assert!(!buf[(x + G, 4)].modifier.contains(Modifier::REVERSED));
        }
        assert!(!buf[(1 + G, 3)].modifier.contains(Modifier::REVERSED));
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    fn top_bar_names_the_files_and_counts_changes() {
        let (mut app, a, b) = files_app(OLD_CPP, NEW_CPP);
        app.labels = ("demo/old.cpp".into(), "demo/new.cpp".into());
        let buf = draw_buffer(&mut app);
        let bar = row_text(&buf, 0, 0..80);
        assert!(
            bar.starts_with(" vdiff   demo/{old.cpp → new.cpp}  "),
            "{bar:?}"
        );
        assert!(bar.ends_with("  4 changes   -19 +18 cols "), "{bar:?}");
        assert_eq!(buf[(1, 0)].bg, Color::Cyan);
        assert_eq!(buf[(67, 0)].symbol(), "-");
        assert_eq!(buf[(67, 0)].fg, Color::Red);
        assert_eq!(buf[(71, 0)].symbol(), "+");
        assert_eq!(buf[(71, 0)].fg, Color::Green);
        // A narrower bar folds the label's directories to keep the
        // counts, and drops the counts when even that is too long.
        app.labels = ("deep/demo/old.cpp".into(), "deep/demo/new.cpp".into());
        for (width, expected) in [
            (
                60,
                " vdiff   …/{old.cpp → new.cpp}     4 changes   -19 +18 cols ",
            ),
            (40, " vdiff   deep/demo/{old.cpp → new.cpp}  "),
        ] {
            let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
            let buf = terminal
                .draw(|frame| app.render(frame))
                .unwrap()
                .buffer
                .clone();
            assert_eq!(row_text(&buf, 0, 0..width), expected);
        }
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    fn top_bar_reports_identical_files_and_skipped_lcs() {
        let (mut app, a, b) = files_app("same\n", "same\n");
        app.labels = ("a.txt".into(), "b.txt".into());
        let buf = draw_buffer(&mut app);
        let bar = row_text(&buf, 0, 0..80);
        assert!(bar.starts_with(" vdiff   a.txt → b.txt  "), "{bar:?}");
        assert!(bar.ends_with(" identical "), "{bar:?}");
        let (mut app, c, d) = files_app("x\n", "y\n");
        app.labels = ("x".into(), "y".into());
        app.degraded = true;
        let bar = row_text(&draw_buffer(&mut app), 0, 0..80);
        assert!(
            bar.contains(" LCS skipped   1 change   -1 +1 cols "),
            "{bar:?}"
        );
        for path in [a, b, c, d] {
            let _ = std::fs::remove_file(path);
        }
    }

    #[test]
    fn top_bar_shows_the_selected_file_in_git_mode() {
        let mut app = git_app(vec![("a\n", "b\n"), ("c\n", "d\n")]);
        app.handle_key(key(KeyCode::Tab));
        let bar = row_text(&draw_buffer(&mut app), 0, 0..80);
        assert!(
            bar.starts_with(" vdiff   HEAD~1 → HEAD   f1.txt  "),
            "{bar:?}"
        );
    }

    #[test]
    fn status_bar_shows_mode_ranges_and_hints() {
        let (mut app, a, b) = files_app(&"x\n".repeat(40), &"y\n".repeat(40));
        let buf = draw_buffer(&mut app);
        assert_eq!(
            row_text(&buf, 23, 0..80),
            format!(
                " FILES   line 1-21/40{}hjkl scroll   n/p change   ? help ",
                " ".repeat(25)
            )
        );
        // Narrow: hints go first, `? help` last.
        let mut terminal = Terminal::new(TestBackend::new(30, 24)).unwrap();
        let buf = terminal
            .draw(|frame| app.render(frame))
            .unwrap()
            .buffer
            .clone();
        assert_eq!(row_text(&buf, 23, 0..30), " FILES   line 1-21/40  ? help ");
        // A diff wider than the pane also shows the column range.
        let line = "a".repeat(200) + "\n";
        let (mut wide, c, d) = files_app(&line, &line);
        wide.scroll_x = 10;
        let bar = row_text(&draw_buffer(&mut wide), 23, 0..80);
        assert!(
            bar.starts_with(" FILES   line 1-1/1  col 11-85/200 "),
            "{bar:?}"
        );
        for path in [a, b, c, d] {
            let _ = std::fs::remove_file(path);
        }
    }

    #[test]
    fn status_bar_hints_follow_the_mode() {
        let mut app = git_app(vec![("a\n", "b\n"), ("c\n", "d\n")]);
        let bar = row_text(&draw_buffer(&mut app), 23, 0..80);
        assert!(bar.starts_with(" GIT   line 1-1/1"), "{bar:?}");
        assert!(bar.ends_with("tab file   n/p change   ? help "), "{bar:?}");
        let mut empty = git_app(vec![]);
        let bar = row_text(&draw_buffer(&mut empty), 23, 0..80);
        assert_eq!(bar, format!(" GIT {}? help ", " ".repeat(68)));
    }

    #[test]
    fn help_ignores_key_releases() {
        let mut app = git_app(vec![("a\n", "b\n")]);
        for close in [KeyCode::Char('q'), KeyCode::Char('?'), KeyCode::Esc] {
            let r = app.handle_key(key(KeyCode::Char('?')));
            assert!(!r.quit && r.changed && app.help);
            let before = app.view_state();
            let r = app.handle_key(key_release(KeyCode::Char('?')));
            assert!(!r.quit && !r.changed);
            assert_eq!(app.view_state(), before);

            let r = app.handle_key(key(close));
            assert!(!r.quit && r.changed && !app.help);
            let before = app.view_state();
            let r = app.handle_key(key_release(close));
            assert!(!r.quit && !r.changed);
            assert_eq!(app.view_state(), before);
        }
        assert!(app.handle_key(key(KeyCode::Char('q'))).quit);
    }

    #[test]
    fn help_popup_takes_keys_until_closed() {
        let tall = "a\n".repeat(50);
        let mut app = git_app(vec![(&tall, "b\n")]);
        let r = app.handle_key(key(KeyCode::Char('?')));
        assert!(r.changed && app.help);
        let buf = draw_buffer(&mut app);
        let screen: Vec<String> = (0..24).map(|y| row_text(&buf, y, 0..80)).collect();
        assert!(
            screen.iter().any(|row| row.contains("╭ Keys ")),
            "{screen:#?}"
        );
        assert!(
            screen
                .iter()
                .any(|row| row.contains("Tab / S-Tab   next / previous file"))
        );
        assert!(screen.iter().any(|row| row.contains(" esc to close ╯")));
        // Other keys do nothing while it is open, and `q` closes it
        // instead of quitting.
        let r = app.handle_key(key(KeyCode::Char('j')));
        assert!(!r.changed && !r.quit);
        assert_eq!(app.scroll_y, 0);
        let r = app.handle_key(key(KeyCode::Char('q')));
        assert!(r.changed && !r.quit && !app.help);
        app.handle_key(key(KeyCode::Char('?')));
        assert!(app.handle_key(key(KeyCode::Esc)).changed);
        app.handle_key(key(KeyCode::Char('?')));
        assert!(app.handle_key(key(KeyCode::Char('?'))).changed);
        assert!(!app.help);
        // Files mode has no file keys to list.
        let (mut files, a, b) = files_app("a\n", "b\n");
        files.handle_key(key(KeyCode::Char('?')));
        let buf = draw_buffer(&mut files);
        let screen: String = (0..24).map(|y| row_text(&buf, y, 0..80)).collect();
        assert!(screen.contains("next / previous change"));
        assert!(!screen.contains("next / previous file"));
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    fn unreadable_files_show_a_notice() {
        let missing = std::env::temp_dir().join("vdiff-missing-file");
        let mut app = App::new(
            DiffSource::Files {
                old: missing.clone(),
                new: missing.clone(),
            },
            ansi16(),
        );
        let path = missing.display().to_string();
        assert_eq!(
            app.notice,
            Some(Notice {
                title: "Binary or unreadable file".into(),
                detail: Some(format!("{path} → {path}")),
            })
        );
        let buf = draw_buffer(&mut app);
        assert_eq!(
            row_text(&buf, 11, 0..80).trim(),
            "Binary or unreadable file"
        );
        let bar = row_text(&buf, 23, 0..80);
        assert!(bar.starts_with(" FILES  "), "{bar:?}");
        assert!(bar.ends_with("  ? help "), "{bar:?}");
    }

    #[test]
    fn tiny_terminals_do_not_panic() {
        for (width, height) in [(1, 1), (1, 3), (5, 2), (9, 4), (30, 3)] {
            let mut app = git_app(vec![("a\nb\n", "a\nc\n"), ("x\n", "y\n")]);
            app.help = true;
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| app.render(frame)).unwrap();
            app.sidebar = Some(true);
            app.help = false;
            terminal.draw(|frame| app.render(frame)).unwrap();
        }
    }
}
