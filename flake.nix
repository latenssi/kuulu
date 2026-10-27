{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
  };

  outputs = { nixpkgs, ... }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
      forAllSystems = fn: nixpkgs.lib.genAttrs systems (system: fn nixpkgs.legacyPackages.${system});
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
          ];
          RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
        };
      });
    };
}
