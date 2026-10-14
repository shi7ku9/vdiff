# vdiff: a vertical diff viewer

`vdiff` compares files **column-by-column**. Characters at the same position
across every line form a column, and changed columns are shown with their old
and new contents side by side.

Yes, this is counterintuitive. That's the point. It's a toy, a fun way to look at your files sideways.

## What it looks like

Diffing `old.cpp` against `new.cpp`:

```cpp
// old.cpp
#include <cstdio>

int main() {
  std::printf("Hello, World\n");
  return 0;
}
```

```cpp
// new.cpp
#include <print>

auto main() -> int {
  std::println("Hello, World");
  return 0;
}
```

`vdiff old.cpp new.cpp` prints:

```
--|++| |---------------|+++++++++++++++| |+|        |--|
#i|#i|n|clude <cstdio> |clude <print>  | | |        |  |   
  |  | |               |               | | |        |  |   
in|au|t| main() {      |o main() -> int| |{|        |  |   
  |  |s|td::printf("Hel|td::println("He|l|l|o, World|\n|");
  |  |r|eturn 0;       |eturn 0;       | | |        |  |   
} |} | |               |               | | |        |  |   
```

The first row is the **marker row**: `-`/`+` mark the columns that changed. The `|` separators group runs of changes; the one between `-` and `+` separates the delete run and insert run of the same changed column. Each following row is one line of the file, with the old and new characters of each changed column shown side by side.

In the TUI, the marker row becomes red and green tabs over the changed
columns, labelled like git hunk headers: `-4,15` marks 15 columns starting at
column 4 of the old file. Deleted columns are tinted red and inserted columns
green, and a fainter tint continues each changed column down to the bottom of
the diff pane. Line numbers stay on the left while you scroll sideways, and
when the diff is wider than the pane, an overview bar along its bottom shows
where the changed columns are, with the part on screen bright between two
`┃`.
Unchanged columns stay neutral, and group separators are thin rules. A top bar
names what is being compared and counts the changes; the status bar shows the
visible lines and the keys worth knowing.

Colors follow what the terminal supports: 24-bit color when `COLORTERM` is
`truecolor` or `24bit`, a 256-color palette when `TERM` contains `256color`,
and otherwise the 16 basic colors, with black text on red and green. With
`NO_COLOR` set, changed cells are shown in reverse video instead.

`--theme light` suits terminals with a light background, and `--theme
dark-colorblind` or `--theme light-colorblind` swap red and green for orange
and blue. The `VDIFF_THEME` environment variable sets the default.

## Building

Requires a Rust toolchain (edition 2024). Git mode requires the `git` binary.

```console
$ cargo build --release
$ cargo run --release -- old.cpp new.cpp
```

The built executable is `target/release/vdiff`. To install it on your Cargo
binary path, run `cargo install --path . --locked`.

## Usage

```console
# Diff two files
$ vdiff old.cpp new.cpp

# Diff a git repository (git diff semantics)
$ vdiff git                 # worktree vs index
$ vdiff git --cached        # index vs HEAD
$ vdiff git HEAD^           # worktree vs a revision
$ vdiff git HEAD^..HEAD     # between two revisions
$ vdiff git main...feature  # merge-base vs the second revision

# Colors for a light terminal
$ vdiff --theme light old.cpp new.cpp
```

When stdout is not a terminal, `vdiff` prints plain text instead of opening the TUI. The git mode prints each changed file with a `=== path ===` header, so it works in scripts and pipes.

## Key bindings

| Key | Action |
|---|---|
| `j` / `k` | scroll the diff vertically (marker row stays pinned) |
| `h` / `l` | scroll horizontally; complete characters and emoji are never split |
| `Tab` / `Shift-Tab` | move the file selection (git mode; the diff updates live) |
| `n` / `p` | jump to the next / previous change group |
| `g` / `G` | scroll to top / bottom |
| `e` | toggle the file sidebar (git mode; starts hidden below 80 columns) |
| `i` | inspect cells: a cursor shows its cell's grapheme, code points, width, and column in each file (`h`/`j`/`k`/`l` move it, `Esc` ends) |
| `?` | show or hide the key help (`Esc` or `q` also closes it) |
| `q` / `Ctrl-C` | quit |

The mouse works too: the wheel scrolls the diff (sideways with Shift held) or,
over the sidebar, moves the file selection; clicking a file opens it,
clicking a cell inspects it, and clicking or dragging on the overview bar
moves the view there. While vdiff
holds the mouse, most terminals still select text with Shift held.

## Documentation

- [Development](docs/development.md): Rust and Nix setup, direnv, Git hooks, and CI checks.
- [Design](docs/design.md): column comparison, Unicode rendering, storage, and the LCS limit.

Enjoy looking at your diffs sideways.
