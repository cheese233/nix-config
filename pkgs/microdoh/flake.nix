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

      nixosModules.default = { config, lib, pkgs, ... }:
        let
          cfg = config.services.microdoh;
        in
        {
          options.services.microdoh = {
            enable = lib.mkEnableOption "microdoh, a minimal DNS-over-HTTPS proxy (Rust + libcurl)";

            package = lib.mkOption {
              type = lib.types.package;
              default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
              defaultText = lib.literalExpression ''
                inputs.microdoh.packages.''${pkgs.stdenv.hostPlatform.system}.default
              '';
              description = "microdoh package to use.";
            };

            listen = lib.mkOption {
              type = lib.types.str;
              default = "[::1]:5443";
              description = "UDP address to listen on for DNS queries.";
            };

            upstream = lib.mkOption {
              type = lib.types.str;
              default = "https://dns.google/dns-query";
              example = "https://dns.nextdns.io/abc123";
              description = "DoH upstream URL (RFC 8484).";
            };

            bootstrapDns = lib.mkOption {
              type = lib.types.str;
              default = "127.0.0.1";
              description = "Bootstrap DNS server for resolving the DoH upstream hostname.";
            };

            tokenFile = lib.mkOption {
              type = lib.types.nullOr lib.types.path;
              default = null;
              description = ''
                File containing the bearer token for Authorization: Bearer.
                If set, the content is passed as MICRODOH_TOKEN.
              '';
            };

            timeoutSecs = lib.mkOption {
              type = lib.types.ints.positive;
              default = 30;
              description = "Request timeout in seconds.";
            };

            extraArgs = lib.mkOption {
              type = lib.types.listOf lib.types.str;
              default = [ ];
              description = "Extra arguments passed to microdoh.";
            };
          };

          config = lib.mkIf cfg.enable {
            systemd.services.microdoh = {
              description = "microdoh — DNS-over-HTTPS proxy";
              after = [ "network.target" "agenix.service" ];
              wants = [ "network.target" ];
              wantedBy = [ "multi-user.target" ];

              serviceConfig = let
                args = [
                  "${cfg.package}/bin/microdoh"
                  "--listen" cfg.listen
                  "--upstream" cfg.upstream
                  "--bootstrap-dns" cfg.bootstrapDns
                  "--timeout-secs" (toString cfg.timeoutSecs)
                ] ++ lib.optionals (cfg.tokenFile != null) [
                  "--token-file" cfg.tokenFile
                ] ++ cfg.extraArgs;
              in
              {
                Type = "simple";
                ExecStart = lib.escapeShellArgs args;
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
