{
  pkgs ? import <nixpkgs> { },
}:
let
  inherit (pkgs) lib;
in
rec {
  packages = lib.recurseIntoAttrs (import ./nix/packages { inherit pkgs; });

  nixosModules.sluice = {
    imports = [ ./nix/modules/sluice.nix ];
    config.services.sluice.package = lib.mkDefault packages.sluice;
  };

  passthru = { inherit pkgs; };
}
