{
  description = "microdoh — minimal DNS-over-HTTPS proxy";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";

  outputs = { self, nixpkgs }:
    let
      supportedSystems = [ "x86_64-linux" "aarch64-linux" ];
      forAllSystems = nixpkgs.lib.genAttrs supportedSystems;
    in
    {
      packages = forAllSystems (system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          default = pkgs.callPackage ./package.nix { };
          microdoh = pkgs.callPackage ./package.nix { };
          microdoh-h3 = pkgs.callPackage ./package.nix { enableH3 = true; };
        }
      );
    };
}
