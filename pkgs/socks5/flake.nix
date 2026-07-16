{
  description = "socks5 — SOCKS5 proxy server";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
  inputs.socks5-src = {
    url = "github:homovetus/socks5";
    flake = false;
  };

  outputs = { self, nixpkgs, socks5-src }:
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
          default = pkgs.haskell.lib.compose.dontCheck (
            pkgs.haskell.lib.compose.doJailbreak (
              pkgs.haskellPackages.callCabal2nix "socks5" socks5-src { }
            )
          );
        }
      );

      nixosModules.default = { config, lib, pkgs, ... }:
        let
          cfg = config.services.socks5;
        in
        {
          options.services.socks5 = {
            enable = lib.mkEnableOption "SOCKS5 proxy server";

            package = lib.mkOption {
              type = lib.types.package;
              default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
              description = "The SOCKS5 package to use.";
            };

            host = lib.mkOption {
              type = lib.types.str;
              default = "::";
              description = "IP address/hostname to bind the SOCKS5 server to.";
            };

            port = lib.mkOption {
              type = lib.types.port;
              default = 1080;
              description = "Port to listen on.";
            };

            userPass = lib.mkOption {
              type = lib.types.listOf lib.types.str;
              default = [ ];
              example = [ "user:pass" ];
              description = "List of username:password pairs for authentication.";
            };
          };

          config = lib.mkIf cfg.enable {
            systemd.services.socks5 = {
              description = "SOCKS5 proxy server";
              after = [ "network.target" ];
              wantedBy = [ "multi-user.target" ];

              serviceConfig = {
                Type = "simple";
                ExecStart = lib.escapeShellArgs (
                  [
                    "${cfg.package}/bin/socks5"
                    "--host" cfg.host
                    "--port" (toString cfg.port)
                  ] ++ lib.concatMap (up: [ "--user-pass" up ]) cfg.userPass
                );
                Restart = "on-failure";
                RestartSec = "5s";
                DynamicUser = true;
                AmbientCapabilities = [ "CAP_NET_BIND_SERVICE" ];
                CapabilityBoundingSet = [ "CAP_NET_BIND_SERVICE" ];
                NoNewPrivileges = true;
                PrivateTmp = true;
                ProtectSystem = "strict";
                ProtectHome = true;
                RestrictAddressFamilies = [ "AF_INET" "AF_INET6" "AF_UNIX" ];
              };
            };
          };
        };
    };
}
