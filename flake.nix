{
  description = "RustDesk client development environment";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
    rust-overlay.inputs.nixpkgs.follows = "nixpkgs";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, rust-overlay, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs { inherit system overlays; };

        rustToolchain = pkgs.rust-bin.stable."1.82.0".default.override {
          extensions = [ "rust-src" "rust-analyzer" "clippy" "rustfmt" ];
        };
      in
      {
        # Mirrors nixpkgs rustdesk package.nix buildInputs.
        # Use `linux-pkg-config` feature (no vcpkg needed).
        # Flutter excluded — install separately or use `nix shell nixpkgs#flutter`.
        devShells.default = pkgs.mkShell {
          name = "rustdesk-client";

          nativeBuildInputs = with pkgs; [
            rustToolchain
            cargo-watch
            pkg-config
            perl   # needed by openssl-sys
            git
            rustPlatform.bindgenHook  # handles LIBCLANG_PATH automatically
          ];

          buildInputs = with pkgs; [
            # From nixpkgs rustdesk package.nix
            atk
            bzip2
            cairo
            dbus
            gdk-pixbuf
            glib
            gst_all_1.gst-plugins-base
            gst_all_1.gstreamer
            gtk3
            libgit2
            libpulseaudio
            libsodium
            libxtst
            libvpx
            libyuv
            libopus
            libaom
            libxkbcommon
            openssl
            pam
            pango
            zlib
            zstd
            alsa-lib
            xdotool
          ];

          env = {
            SODIUM_USE_PKG_CONFIG = "true";
            ZSTD_SYS_USE_PKG_CONFIG = "true";
          };

          # Disable hardening — fixes linker issues with large nix closures
          hardeningDisable = [ "all" ];

          shellHook = ''
            echo "RustDesk client dev shell"
            echo "  Rust: $(rustc --version)"
            echo ""
            echo "Build:  cargo build --features linux-pkg-config"
            echo "Test:   cargo test --features linux-pkg-config"
            echo ""
            echo "Init submodules first:"
            echo "  git submodule update --init --recursive"
          '';
        };
      }
    );
}
