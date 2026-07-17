{
  description = "avahi2dns — bridge unicast DNS queries for a TLD to Avahi/mDNS";

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
          default = pkgs.buildGoModule rec {
            pname = "avahi2dns";
            version = "0.2.0-dirty";

            src = pkgs.fetchFromGitHub {
              owner = "LouisBrunner";
              repo = "avahi2dns";
              rev = "b7ff69df6e692f44e698462b5aea21777f8f6896";
              hash = "sha256-k9Yi6c0LJw6w5RgxfNM82HQ/xDBCqyoSdKlnSfjOpxI=";
            };

            vendorHash = "sha256-s+NuVtHmN963kNyaIsA5q9a+e1uDvQsH4qNDF63gk0Y=";

            doCheck = false;

            meta = with pkgs.lib; {
              description = "Bridge unicast DNS queries for a TLD to Avahi via D-Bus";
              homepage = "https://github.com/LouisBrunner/avahi2dns";
              license = licenses.mit;
              mainProgram = "avahi2dns";
              maintainers = [ ];
              platforms = platforms.linux;
            };
          };
        });

      nixosModules.default = { config, lib, pkgs, ... }:
        let
          cfg = config.services.avahi2dns;
        in
        {
          options.services.avahi2dns = {
            enable = lib.mkEnableOption ''
              avahi2dns, a small DNS→mDNS bridge that serves a given
              TLD (e.g. `local`) by forwarding unicast DNS queries to
              the local Avahi daemon over D-Bus
            '';

            package = lib.mkOption {
              type = lib.types.package;
              default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
              defaultText = lib.literalExpression ''
                inputs.avahi2dns.packages.''${pkgs.stdenv.hostPlatform.system}.default
              '';
              description = "avahi2dns package to use.";
            };

            address = lib.mkOption {
              type = lib.types.str;
              default = "127.0.0.1";
              description = "Address to listen on (DNS listener).";
            };

            port = lib.mkOption {
              type = lib.types.port;
              default = 5354;
              description = "UDP/TCP port for the DNS listener.";
            };

            domain = lib.mkOption {
              type = lib.types.str;
              default = "local";
              description = "TLD to bridge to Avahi (only queries below this name are forwarded).";
            };
          };

          config = lib.mkIf cfg.enable {
            systemd.services.avahi2dns = {
              description = "Avahi to DNS Bridge";
              after = [ "network.target" "avahi-daemon.service" ];
              requires = [ "avahi-daemon.service" ];
              wantedBy = [ "multi-user.target" ];
              serviceConfig = {
                ExecStart = lib.escapeShellArgs [
                  "${cfg.package}/bin/avahi2dns"
                  "-a" cfg.address
                  "-p" (toString cfg.port)
                  "-d" cfg.domain
                ];
                Restart = "always";
                DynamicUser = true;
                AmbientCapabilities = [ "CAP_NET_BIND_SERVICE" ];
                CapabilityBoundingSet = [ "CAP_NET_BIND_SERVICE" ];
              };
            };
          };
        };
    };
}
