{ inputs, ... }:
pkgs:
let
  rustBin = inputs.rust-overlay.lib.mkRustBin { } pkgs;
  rustToolchain = (rustBin.fromRustupToolchainFile ../rust-toolchain.toml).override {
    extensions = [
      "clippy"
      "rustfmt"
      # build-std tests compile std from the supplied sysroot's sources.
      "rust-src"
    ];
    # Wasm tests link against a prebuilt standard library when not using build-std.
    targets = [ "wasm32-unknown-unknown" ];
  };
in
pkgs.callPackage ./package.nix {
  crane = (inputs.crane.mkLib pkgs).overrideScope (
    _: _: {
      stdenvSelector = packages: packages.clangStdenv;
    }
  );
  inherit rustToolchain;
}
