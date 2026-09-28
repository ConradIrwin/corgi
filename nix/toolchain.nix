{ inputs, ... }:
pkgs:
let
  rustBin = inputs.rust-overlay.lib.mkRustBin { } pkgs;
  rustToolchain = (rustBin.fromRustupToolchainFile ../rust-toolchain.toml).override {
    extensions = [
      "clippy"
      "rustfmt"
    ];
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
