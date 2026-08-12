# vdiff: a vertical diff viewer

`vdiff` diffs files **column-by-column** instead of line-by-line.

Normal diff tools ask *"which lines changed?"*; `vdiff` asks *"which columns changed?"*. Every character position across all lines is a diff unit: the k-th character of every line becomes one column, columns are compared, and the result is transposed back so you can see exactly which **column** of your file changed, with the old and new characters side by side.

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
```

When stdout is not a terminal, `vdiff` prints plain text instead of opening the TUI. The git mode prints each changed file with a `=== path ===` header, so it works in scripts and pipes.

## Key bindings

| Key | Action |
|---|---|
| `j` / `k` | scroll the diff vertically (marker row stays pinned) |
| `h` / `l` | scroll horizontally; wide characters (CJK) are never split |
| `Tab` / `Shift-Tab` | move the file selection (git mode; the diff updates live) |
| `n` / `p` | jump to the next / previous change group |
| `g` / `G` | scroll to top / bottom |
| `e` | toggle the file sidebar (git mode) |
| `q` / `Ctrl-C` | quit |

## Building

```console
$ cargo build --release
$ cargo run --release -- old.cpp new.cpp
```

Requires a Rust toolchain (edition 2024). Git mode requires the `git` binary.

## Fun facts

- The diff engine is a classic LCS over columns, with common prefix/suffix trimmed first. Typical diffs only touch a handful of columns.
- Files that are very wide on both sides degrade gracefully: when the product of the two middle widths would exceed 1,000,000 columns, the LCS table is skipped and the middle columns are treated as one big change (the status bar tells you).
- CRLF, UTF-8, and CJK characters are handled; multi-byte characters never get split mid-character. Tabs expand to the next tab stop (8) so columns line up, and a missing trailing newline shows as a `↵` column.

Enjoy looking at your diffs sideways.
