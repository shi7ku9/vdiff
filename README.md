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

In the TUI, deleted columns have a red background and inserted columns have a
green background, with black text. The color extends down to the bottom of the
diff pane. Unchanged columns and group separators stay neutral.

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
| `e` | toggle the file sidebar (git mode) |
| `q` / `Ctrl-C` | quit |

## Documentation

- [Development](docs/development.md): Rust and Nix setup, direnv, Git hooks, and CI checks.
- [Design](docs/design.md): column comparison, Unicode rendering, storage, and the LCS limit.

Enjoy looking at your diffs sideways.
