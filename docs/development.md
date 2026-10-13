# Development

[Back to the README](../README.md) · [Design](design.md)

## Development environment

### With Nix

From the project directory, enter the development shell and install the Git
hooks once per clone:

```console
$ nix develop
$ pre-commit install
```

The shell includes Rust, rustfmt, Clippy, rust-analyzer, Git, and pre-commit.
The toolchain comes from [rust-toolchain.toml](../rust-toolchain.toml), resolved
using the rust-overlay snapshot in [flake.lock](../flake.lock).

### Automatic loading with direnv

Install [direnv](https://direnv.net/) and enable its
[shell hook](https://direnv.net/docs/hook.html). For zsh, add
`eval "$(direnv hook zsh)"` to `~/.zshrc` and restart the shell. Then run once
from the project directory:

```console
$ direnv allow
$ pre-commit install
```

The checked-in [.envrc](../.envrc) loads the flake's development shell and
watches `Cargo.toml`, `Cargo.lock`, and `rust-toolchain.toml` for changes.
[nix-direnv](https://github.com/nix-community/nix-direnv) can cache the shell
environment. Local direnv files under `.direnv/` are ignored by Git.

### Without Nix

Install [rustup](https://rustup.rs/), Git, and
[pre-commit](https://pre-commit.com/#installation), then run from the project
directory:

```console
$ pre-commit install
```

`rust-toolchain.toml` selects stable Rust and the required components
automatically. Rustup and GitHub Actions use the current stable release; Nix
uses the stable toolchain available in the locked rust-overlay snapshot.

## Git hooks

[.pre-commit-config.yaml](../.pre-commit-config.yaml) defines hooks that check
Rust formatting and run Clippy on Rust or Cargo changes.

Formatting is checked without rewriting files. Fix failures with
`cargo fmt --all` and stage the changes again. Run the hooks manually with:

```console
$ pre-commit run --all-files
```

## Build and checks

Run the same Rust checks as CI:

```console
$ cargo fmt --all -- --check
$ cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
$ cargo build --locked --workspace --all-targets --all-features
$ cargo test --locked --workspace --all-features
```

Nix users can run the build, test, Clippy, and formatting checks with:

```console
$ nix flake check
```

When adding new files, stage them first so Nix includes them in the flake
source. The shell and Nix checks are defined in [flake.nix](../flake.nix).

## Continuous integration

[GitHub Actions](../.github/workflows/ci.yml) checks formatting and Clippy on
Linux, and builds and tests on Linux, macOS, and Windows for pushes and pull
requests. Tests include doctests.

[Dependabot](../.github/dependabot.yml) checks Cargo and GitHub Actions
dependencies weekly, grouping updates for each ecosystem.
