{
  description = "DNS-over-HTTPS with bearer token authentication support";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/release-26.05";

  outputs =
    { self, nixpkgs }:
    let
      supportedSystems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      forAllSystems = nixpkgs.lib.genAttrs supportedSystems;
    in
    {
      packages = forAllSystems (system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          default = pkgs.callPackage ./package.nix { };
          dns-over-https = pkgs.callPackage ./package.nix { };
        }
      );
    };
}
