{ inputs, ... }:
{
  perSystem =
    { pkgs, ... }:
    let
      mkCorgi = import ../toolchain.nix { inherit inputs; };
      corgi = mkCorgi pkgs;
    in
    {
      packages = {
        inherit corgi;
        default = corgi;
      };
      checks.corgi = corgi;
      formatter = pkgs.nixfmt-tree;
    };
}
