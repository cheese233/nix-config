{
  description = "mdns-repeater — Multicast DNS repeater for bridging mDNS between interfaces";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";

  outputs = { self, nixpkgs }:
    let
      supportedSystems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = nixpkgs.lib.genAttrs supportedSystems;
    in
    {
      packages = forAllSystems (system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          default = pkgs.stdenv.mkDerivation rec {
            pname = "mdns-repeater";
            version = "1.11";

            src = pkgs.fetchFromGitHub {
              owner = "geekman";
              repo = "mdns-repeater";
              rev = "f714ec4d8a0eee248ead29c2ae1bb37f124a568f";
              hash = "sha256-D7jQhFepN2Dqg8fB7S+MOfL70p8U68+7t8T/pG0L9nI="; # Placeholder, will be corrected during build
            };

            nativeBuildInputs = [ pkgs.gcc pkgs.gnumake ];

            buildPhase = ''
              make
            '';

            installPhase = ''
              mkdir -p $out/bin
              cp mdns-repeater $out/bin/
            '';

            meta = with pkgs.lib; {
              description = "Multicast DNS repeater";
              homepage = "https://github.com/geekman/mdns-repeater";
              license = licenses.gpl2Only;
              platforms = platforms.linux;
            };
          };
        });

      # Systemd-based NixOS module
      nixosModules.default = { config, lib, pkgs, ... }:
        let
          cfg = config.services.mdns-repeater;
        in
        {
          options.services.mdns-repeater = {
            enable = lib.mkEnableOption "mdns-repeater daemon to bridge mDNS between interfaces";

            package = lib.mkOption {
              type = lib.types.package;
              default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
              defaultText = lib.literalExpression "inputs.mdns-repeater.packages.\${pkgs.stdenv.hostPlatform.system}.default";
              description = "The mdns-repeater package to use.";
            };

            interfaces = lib.mkOption {
              type = lib.types.listOf lib.types.str;
              example = [ "br-lan" "enp2s0f1" ];
              description = "List of network interfaces between which to repeat mDNS packets.";
            };
          };

          config = lib.mkIf cfg.enable {
            systemd.services.mdns-repeater = {
              description = "mdns-repeater — Multicast DNS repeater";
              after = [ "network-online.target" ];
              wants = [ "network-online.target" ];
              wantedBy = [ "multi-user.target" ];

              serviceConfig = {
                Type = "simple";
                ExecStart = "${cfg.package}/bin/mdns-repeater ${lib.concatStringsSep " " cfg.interfaces}";
                Restart = "always";
                RestartSec = 5;
                # Hardening / sandboxing
                DynamicUser = true;
                AmbientCapabilities = [ "CAP_NET_RAW" "CAP_NET_BIND_SERVICE" ];
                CapabilityBoundingSet = [ "CAP_NET_RAW" "CAP_NET_BIND_SERVICE" ];
                ProtectSystem = "strict";
                ProtectHome = true;
                PrivateTmp = true;
                PrivateDevices = true;
              };
            };
          };
        };
    };
}
