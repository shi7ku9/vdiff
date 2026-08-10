{
  description = "vdiff — a vertical diff viewer";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
    rust-overlay.url = "github:oxalica/rust-overlay";
    rust-overlay.inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs = inputs@{ flake-parts, ... }:
    flake-parts.lib.mkFlake { inherit inputs; } {
      imports = [ flake-parts.flakeModules.easyOverlay ];

      systems = [ "x86_64-linux" "aarch64-linux" ];

      perSystem = { pkgs, final, ... }:
        let
          toolchain = final.rust-bin.stable.latest.default;
          rustPlatform = pkgs.makeRustPlatform {
            cargo = toolchain;
            rustc = toolchain;
          };
          cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);
          src = pkgs.lib.cleanSource ./.;
          # git for integration tests / runtime (git diff subcommand).
          nativeBuildInputs = [ pkgs.git ];
          # Vendored crates from Cargo.lock so the checks run offline in the build sandbox.
          cargoDeps = rustPlatform.importCargoLock { lockFile = ./Cargo.lock; };
          offlineCargo = ''
            export CARGO_HOME="$(mktemp -d)"
            cat > "$CARGO_HOME/config.toml" <<EOF
            [source.crates-io]
            replace-with = "vendored-sources"
            [source.vendored-sources]
            directory = "${cargoDeps}"
            EOF
          '';
        in {
          overlayAttrs = inputs.rust-overlay.overlays.default final pkgs;

          devShells.default = pkgs.mkShell {
            packages = [ toolchain pkgs.git ];
          };

          packages.default = rustPlatform.buildRustPackage {
            pname = cargoToml.package.name;
            version = cargoToml.package.version;
            inherit src nativeBuildInputs;
            cargoLock.lockFile = ./Cargo.lock;
          };

          checks = {
            build = pkgs.runCommand "vdiff-build"
              { nativeBuildInputs = [ toolchain pkgs.git ]; } ''
              ${offlineCargo}
              cp -r ${src} src
              chmod -R +w src
              cd src
              cargo build --offline --all-features --all-targets
              mkdir $out
            '';
            test = pkgs.runCommand "vdiff-test"
              { nativeBuildInputs = [ toolchain pkgs.git ]; } ''
              ${offlineCargo}
              cp -r ${src} src
              chmod -R +w src
              cd src
              cargo test --offline --all-features
              mkdir $out
            '';
            clippy = pkgs.runCommand "vdiff-clippy"
              { nativeBuildInputs = [ toolchain ]; } ''
              ${offlineCargo}
              cp -r ${src} src
              chmod -R +w src
              cd src
              cargo clippy --offline --all-targets -- -D warnings
              mkdir $out
            '';
            fmt = pkgs.runCommand "vdiff-fmt"
              { nativeBuildInputs = [ toolchain ]; } ''
              cp -r ${src} src
              chmod -R +w src
              cd src
              cargo fmt --check
              mkdir $out
            '';
          };
        };
    };
}
