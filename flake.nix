{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    # Nightly Rust for cargo-fuzz only (`nix develop .#fuzz`).
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { nixpkgs, rust-overlay, ... }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
      forAllSystems = fn: nixpkgs.lib.genAttrs systems (system: fn nixpkgs.legacyPackages.${system});
      # Same nightly as the fuzz job in .github/workflows/ci.yml.
      fuzzNightly = "2026-10-20";
    in
    {
      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [
            # Rust
            cargo rustc rustfmt clippy rust-analyzer
            cargo-deny cargo-audit
            # Build deps: opus (audiopus), protoc (prost-build)
            pkg-config cmake libopus protobuf
            # Desktop UI
            nodejs
          ] ++ lib.optionals stdenv.isLinux [
            # cpal + Tauri 2 on Linux
            alsa-lib openssl webkitgtk_4_1 gtk3 libsoup_3 librsvg glib
            # monio (push-to-talk hook): Xlib + XRecord
            libx11 libxtst
          ];
          RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
        };
        # cargo-fuzz needs nightly; the default shell stays on stable.
        fuzz = pkgs.mkShell {
          packages = [
            (rust-overlay.lib.mkRustBin { } pkgs).nightly.${fuzzNightly}.minimal
            pkgs.cargo-fuzz
            pkgs.protobuf
          ];
        };
      });
    };
}
