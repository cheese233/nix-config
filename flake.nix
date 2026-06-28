{
  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixos-26.05";
    agenix = { url = "github:ryantm/agenix"; inputs.nixpkgs.follows = "nixpkgs"; };
    nnf.url = "github:thelegy/nixos-nftables-firewall";
    dae.url = "github:daeuniverse/flake.nix";
  };
  outputs = { self, nixpkgs, agenix, nnf, dae, ... }@inputs: {
    nixosConfigurations = {
      nixos = nixpkgs.lib.nixosSystem {
        system = "x86_64-linux";
        modules = [
          ./hardware-configuration.nix
          ./configuration.nix
          ./kernel.nix
          agenix.nixosModules.default
          nnf.nixosModules.default
          inputs.dae.nixosModules.dae
          { environment.systemPackages = [ agenix.packages.x86_64-linux.default ]; }
        ];
      };
    };
  };
}
