{
  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixos-26.05";
    agenix = { url = "github:ryantm/agenix"; inputs.nixpkgs.follows = "nixpkgs"; };
    nnf.url = "github:thelegy/nixos-nftables-firewall";
  };
  outputs = { self, nixpkgs, agenix, nnf, ... }@inputs: {
    nixosConfigurations = {
      nixos = nixpkgs.lib.nixosSystem {
        system = "x86_64-linux";
        modules = [
          ./hardware-configuration.nix
          ./configuration.nix
          ./kernel.nix
          agenix.nixosModules.default
          nnf.nixosModules.default
          { environment.systemPackages = [ agenix.packages.x86_64-linux.default ]; }
        ];
      };
    };
  };
}
