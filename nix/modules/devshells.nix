{
  perSystem =
    { config, pkgs, ... }:
    let
      corgi = config.packages.corgi;
      selfTest = pkgs.writeShellApplication {
        name = "corgi-self-test";
        runtimeInputs = [ corgi ];
        text = ''
          target_directory="$PWD/target/corgi-self-test"
          corgi build -p corgi-build --bin corgi --target-dir "$target_directory"
          "$target_directory/debug/corgi" --version
          corgi test -p corgi-build --bin corgi --force --target-dir "$target_directory"
          corgi build -p corgi-build --bin corgi --target-dir "$target_directory"
        '';
      };
      selfTestLauncher = pkgs.writeShellApplication {
        name = "corgi-self-test";
        runtimeInputs = [ pkgs.nix ];
        text = ''
          exec nix develop --command ${selfTest}/bin/corgi-self-test "$@"
        '';
      };
    in
    {
      apps.self-test = {
        type = "app";
        program = "${selfTestLauncher}/bin/corgi-self-test";
        meta.description = "Build and test the Corgi checkout with packaged Corgi";
      };

      devShells.default = (pkgs.mkShell.override { inherit (corgi) stdenv; }) {
        name = "corgi-dev";
        inputsFrom = [ corgi ];
        packages = [
          corgi
          corgi.rustToolchain
          selfTest
          config.formatter
        ]
        ++ corgi.runtimeTools;

        env = {
          CORGI_RUST_TOOLCHAIN = "${corgi.rustToolchain}";
          CORGI_CC = "${pkgs.clang}/bin/clang";
          # A dev shell has no installed library output; don't embed outputs/out/lib.
          NIX_NO_SELF_RPATH = "1";
        }
        // pkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isDarwin {
          SDKROOT = pkgs.apple-sdk_15.sdkroot;
        };
      };
    };
}
