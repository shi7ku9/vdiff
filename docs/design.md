# Design

[Back to the README](../README.md) · [Development](development.md)

## Columns and graphemes

The diff unit is a column: the k-th complete grapheme from every line of a
file. Missing cells are treated as spaces, and both files' columns use the
same logical height.

A grapheme is a complete displayed character. `á` (a letter plus a combining
accent) occupies one column, and a ZWJ emoji such as `👩‍❤️‍👩` also occupies one
column. An accent edit such as `é` → `ë` compares the two complete graphemes.
Original grapheme text is compared without Unicode normalization.

Columns compare across every file line. Removing a line can therefore mark a
whole column as changed even when some characters in that column are unchanged.

## Input handling

[src/diff.rs](../src/diff.rs) expands tabs to the next tab stop (8 display
cells), using complete grapheme widths. CRLF line endings are handled by
removing trailing carriage returns from the split lines. A difference in the
presence of a trailing newline produces a synthetic `↵` column and an extra
row so the change remains visible.

## Column storage and comparison

`Column` records its logical height and shares packed backing data through an
`Arc`. The backing data stores UTF-8 text, row indices, and byte-end offsets.
Space cells are implicit, so sparse files do not allocate a separate string
for every padded cell. Cloning a column into a diff step shares that data.

Equal packed columns are interned across both inputs before diffing. Equality
checks logical height and can use shared backing identity; other comparisons
use the packed text and primitive boundary arrays. Row indices and byte
boundaries remain part of equality, so the same concatenated text with
different cell boundaries does not become an accidental match.

## Diff algorithm

The engine trims the common column prefix and suffix, then uses a classic
longest common subsequence (LCS) table for the remaining columns. It emits
`Match`, `Delete`, and `Insert` steps and groups consecutive steps of the same
kind. Identical inputs take a direct path to an all-match grid.

If the product of the two remaining column counts exceeds 1,000,000, the
engine skips the LCS table and emits the middle columns as one delete run and
one insert run. The TUI top bar reports this degraded mode.

## Display layout

Each column uses the widest cell's display width across the file rows. A
one-cell grapheme is padded when it shares a column with a two-cell Chinese
character. Padding comes after the complete grapheme, preserving alignment
without separating combining marks or parts of a ZWJ emoji.

For changed inputs, the plain-text renderer in `src/diff.rs` emits a marker
row followed by file rows, with `|` separators between groups. All-match grids
omit the marker row.

The TUI in [src/app.rs](../src/app.rs) builds its layout from the visible
representation of each cell. Marker positions, red/green backgrounds,
separators, and change-navigation offsets share that layout. Content is drawn
at explicit cell positions, preserving boundaries that could be lost by
concatenating and re-segmenting a whole row.

Match columns stay neutral. A separator is a blank gap between the marker
tabs and a thin `│` rule between file cells; the plain-text renderer keeps
its `|`. Horizontal clipping omits a grapheme that straddles a viewport edge
instead of drawing only part of it.

### Changes and tabs

A change is a delete run, an insert run, or a delete run directly followed
by an insert run. Each run is a tab on the marker row, labelled like a git
hunk header: the run's first column in its own file and its column count,
`-4,15`, shortened to `-4` and then `-` when the tab is narrower. A run
scrolled partly out of view keeps its label at the pane's edge.

Below its tab, a changed column is tinted on every file row, and a fainter
fill continues from the end of the file down to the bottom of the diff pane.
The tint marks the column as a whole; the TUI does not mark which lines
changed.

### Line numbers

A gutter left of the diff numbers the file rows: a blank cell, the numbers
right-aligned in at least two digits, and a blank cell. It stays in place
while the diff scrolls sideways. The numbers are muted, and `~` marks the
rows below the file. Diff areas narrower than 20 columns leave out the
gutter.

### Screen layout

The diff pane sits between a one-row top bar and a one-row status bar, with
no border: the line-number gutter on its left and a blank cell on its right. The top bar shows the compared
paths and the change counts: changes, where a delete run directly followed
by an insert run counts once, and deleted and inserted columns. Paths are
shortened the way git prints renames, `dir/{old → new}`, and their leading
directories fold into `…` before the counts are dropped on a narrow screen.
The status bar shows the mode, the visible line and column ranges, and a few
key hints; `?` opens the full key list in a popup. These parts live in
[src/app/chrome.rs](../src/app/chrome.rs).

In git mode, the file sidebar sits to the left of a divider. Its width
follows the longest path, within 24 to 40 columns and at most two fifths of
the screen. It starts hidden on terminals narrower than 80 columns; `e`
overrides that choice either way.

### Colors

[src/theme.rs](../src/theme.rs) holds every style the TUI draws, by role:
the tab, cell, and fill styles per change kind, separators, muted text,
badges, and the sidebar selection. The theme is chosen by color depth:

- 24-bit color when `COLORTERM` is `truecolor` or `24bit`, or in Windows
  Terminal (`WT_SESSION`).
- The 256-color palette when `TERM` contains `256color`. Its darkest red and
  green are the only tints dark enough for light text, so file cells and the
  fill share them.
- Otherwise the 16 basic colors: black text on red and green for every row
  of a changed column, the TUI's original look.
- A non-empty `NO_COLOR` turns colors off. Changed cells use reverse video,
  and the marker row and separators keep changes readable.

Every style that sets a background also sets the text color, so tinted cells
stay legible on light and dark terminals.

### Standalone marks and controls

Standalone combining marks and leading extending graphemes use a dotted
circle as a display base, such as `◌́` or `◌̈`. This keeps them visible and
prevents them from attaching to separators, padding, or a neighboring control
label.

Changed cells consisting only of filtered control characters are shown as
Unicode code points, such as `U+000D`. Raw grapheme boundaries are retained
until drawing, so control filtering cannot shift the indices used for layout.
These substitutions are TUI representations; the source files and the diff
comparison retain the original grapheme text.
