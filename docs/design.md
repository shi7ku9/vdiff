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
one insert run. The TUI status bar reports this degraded mode.

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

Deleted columns use black text on red; inserted columns use black text on
green. The backgrounds cover the marker row, file rows, and remaining space
down to the bottom of the diff pane. Match columns and separators stay
neutral. Horizontal clipping omits a grapheme that straddles a viewport edge
instead of drawing only part of it.

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
