{
  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixos-26.05";
    agenix = { url = "github:ryantm/agenix"; inputs.nixpkgs.follows = "nixpkgs"; };
    nnf.url = "github:thelegy/nixos-nftables-firewall";
    dae.url = "github:daeuniverse/flake.nix";
    dnsmasq-china-list = {
      url = "./pkgs/dnsmasq-china-list";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    secureboot = {
      url = "./pkgs/secureboot";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    mdns-publisher = {
      url = "./pkgs/mdns-publisher";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    avahi2dns = {
      url = "./pkgs/avahi2dns";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    microdoh = {
      url = "./pkgs/microdoh";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    dibbler = {
      url = "./pkgs/dibbler";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    microvm = {
      url = "github:microvm-nix/microvm.nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    nixpkgs-unstable.url = "github:nixos/nixpkgs/nixos-unstable";
  };

  outputs = { self, nixpkgs, ... }@inputs:
  let
    mkHost = hostFile: nixpkgs.lib.nixosSystem {
      system = "x86_64-linux";
      specialArgs = { inherit inputs; };
      modules = [
        ./common.nix
        hostFile
        inputs.secureboot.nixosModules.default
        inputs.agenix.nixosModules.default
        { environment.systemPackages = [ inputs.agenix.packages.x86_64-linux.default ]; }
      ];
    };
  in {
    nixosConfigurations = {
      nixos = mkHost ./hosts/nixos.nix;
    };
  };
}
