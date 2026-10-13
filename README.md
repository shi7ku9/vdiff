# vdiff: a vertical diff viewer

`vdiff` diffs files **column-by-column** instead of line-by-line.

Normal diff tools ask *"which lines changed?"*; `vdiff` asks *"which columns changed?"*. Every character position across all lines is a diff unit: the k-th character of every line becomes one column, columns are compared, and the result is shown back as rows with a marker row on top, so you can see exactly which **column** of your file changed, with the old and new characters side by side.

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
green background, with black text. The color extends through the marker row,
file content, and empty space down to the bottom of the diff pane.
Unchanged columns use the terminal's default
colors, and group separators stay neutral.

Standalone combining marks use a dotted circle as a display base (`◌́`, `◌̈`)
so edits to accents remain visible. Changes consisting only of filtered control
characters are shown as Unicode code points, such as `U+000D`. These are TUI
representations; the source files are unchanged.

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

## Development

With Nix, enter the development shell, then install the Git hooks once per clone:

```console
$ nix develop
$ pre-commit install
```

The shell includes Rust, rustfmt, Clippy, rust-analyzer, Git, and pre-commit.
To load it automatically when entering the project, install
[direnv](https://direnv.net/) and enable its
[shell hook](https://direnv.net/docs/hook.html). For zsh, add
`eval "$(direnv hook zsh)"` to `~/.zshrc` and restart the shell. Then run once
from the project directory:

```console
$ direnv allow
$ pre-commit install
```

The checked-in `.envrc` loads the flake's development shell and watches
`Cargo.toml`, `Cargo.lock`, and `rust-toolchain.toml` for changes.
[nix-direnv](https://github.com/nix-community/nix-direnv) is recommended to cache
the shell environment. Local direnv files under `.direnv/` are ignored by Git.

Without Nix, install [rustup](https://rustup.rs/), Git, and
[pre-commit](https://pre-commit.com/#installation), then run `pre-commit install`.
`rust-toolchain.toml` selects stable Rust and the required components automatically.
Nix resolves that channel using the rust-overlay snapshot in `flake.lock`; rustup
and GitHub Actions use the current stable release.

The hooks check Rust formatting and run Clippy on Rust or Cargo changes.
Formatting is checked without rewriting files; fix failures with `cargo fmt --all`
and stage the changes again. Run the hooks manually with:

```console
$ pre-commit run --all-files
```

To run the same Rust checks as CI:

```console
$ cargo fmt --all -- --check
$ cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
$ cargo build --locked --workspace --all-targets --all-features
$ cargo test --locked --workspace --all-features
```

GitHub Actions checks formatting and Clippy on Linux, and builds and tests on
Linux, macOS, and Windows for pushes and pull requests. Tests also include
doctests. Dependabot checks Cargo and GitHub Actions dependencies weekly.
Nix users can run the build, test, Clippy, and formatting checks with `nix flake check`.
When adding new files, stage them first so Nix includes them in the flake source.

## Fun facts

- The diff engine is a classic LCS over columns, with common prefix/suffix trimmed first. Typical diffs only touch a handful of columns.
- Files that are very wide on both sides degrade gracefully: when the product of the two middle widths would exceed 1,000,000 columns, the LCS table is skipped and the middle columns are treated as one big change (the status bar tells you).
- CRLF, UTF-8, and CJK characters are handled; multi-byte characters never get split mid-character. Tabs expand to the next tab stop (8) so columns line up, and a missing trailing newline shows as a `↵` column.

Enjoy looking at your diffs sideways.
