{
  lib,
  crane,
  rustToolchain,
  makeWrapper,
  apple-sdk_15,
  bash,
  bubblewrap,
  coreutils,
  curl,
  git,
  gnutar,
  nix,
  stdenv,
}:
let
  craneLib = crane.overrideToolchain rustToolchain;
  manifest = builtins.fromTOML (builtins.readFile ../Cargo.toml);
  runtimeTools = [
    bash
    coreutils
    curl
    git
    gnutar
    nix
  ]
  ++ lib.optional stdenv.hostPlatform.isLinux bubblewrap;
  commonArgs = {
    pname = "corgi";
    inherit (manifest.package) version;

    src = lib.fileset.toSource {
      root = ../.;
      fileset = lib.fileset.unions [
        ../Cargo.toml
        ../Cargo.lock
        ../README.md
        ../src
        ../tests
      ];
    };

    strictDeps = true;
    buildInputs = lib.optional stdenv.hostPlatform.isDarwin apple-sdk_15;
    cargoExtraArgs = "--locked --bin corgi";
  };
in
craneLib.buildPackage (
  commonArgs
  // {
    cargoArtifacts = craneLib.buildDepsOnly commonArgs;
    nativeBuildInputs = [ makeWrapper ];

    # --bin above keeps namespace-dependent integration tests outside Nix's sandbox.
    doCheck = true;

    postInstall = ''
      wrapProgram "$out/bin/corgi" \
        --suffix PATH : ${lib.makeBinPath runtimeTools}
    '';

    passthru = {
      inherit rustToolchain runtimeTools;
    };

    meta = {
      inherit (manifest.package) description;
      homepage = manifest.package.repository;
      license = lib.licenses.mit;
      mainProgram = "corgi";
      platforms = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];
    };
  }
)
