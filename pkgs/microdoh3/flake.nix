{
  description = "microdoh3 — minimal DNS-over-HTTP/3 proxy (noq, prefork, 0-RTT)";

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
          microdoh3 = pkgs.callPackage ./package.nix { };
        }
      );

      nixosModules.default = { config, lib, pkgs, ... }:
        let
          cfg = config.services.microdoh3;
        in
        {
          options.services.microdoh3 = {
            enable = lib.mkEnableOption "microdoh3, a minimal DNS-over-HTTP/3 proxy (Rust + noq QUIC)";

            package = lib.mkOption {
              type = lib.types.package;
              default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
              defaultText = lib.literalExpression ''
                inputs.microdoh3.packages.''${pkgs.stdenv.hostPlatform.system}.default
              '';
              description = "microdoh3 package to use.";
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
              description = "DoH upstream URL (HTTP/3 only, RFC 8484).";
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

            workers = lib.mkOption {
              type = lib.types.ints.unsigned;
              default = 0;
              description = ''
                Number of worker processes. 0 = one per physical CPU core.
                Each worker is pinned to its own core and opens its own
                SO_REUSEPORT DNS socket and QUIC connection.
              '';
            };

            cpus = lib.mkOption {
              type = lib.types.nullOr (lib.types.listOf lib.types.ints.unsigned);
              default = null;
              example = [ 0 2 ];
              description = ''
                CPU core IDs to pin workers to (one worker per listed core).
                Overrides `workers` when set.
              '';
            };

            pad = lib.mkOption {
              type = lib.types.bool;
              default = false;
              description = "Pad DNS queries with EDNS0 padding to 128-byte blocks (RFC 8467).";
            };

            busyPoll = lib.mkOption {
              type = lib.types.bool;
              default = false;
              description = "Enable SO_BUSY_POLL on the DNS socket (lower latency, more CPU).";
            };

            spin = lib.mkOption {
              type = lib.types.bool;
              default = false;
              description = "Do a non-blocking event sweep before sleeping in epoll.";
            };

            mlockall = lib.mkOption {
              type = lib.types.bool;
              default = false;
              description = "Lock all current and future memory (avoid page faults in hot path).";
            };

            verbose = lib.mkOption {
              type = lib.types.bool;
              default = false;
              description = "Enable verbose logging (debug level).";
            };

            extraArgs = lib.mkOption {
              type = lib.types.listOf lib.types.str;
              default = [ ];
              description = "Extra arguments passed to microdoh3.";
            };
          };

          config = lib.mkIf cfg.enable {
            systemd.services.microdoh3 = {
              description = "microdoh3 — DNS-over-HTTP/3 proxy";
              after = [ "network.target" "agenix.service" ];
              wants = [ "network.target" ];
              wantedBy = [ "multi-user.target" ];

              serviceConfig = let
                args = [
                  "${cfg.package}/bin/microdoh3"
                  "--listen" cfg.listen
                  "--upstream" cfg.upstream
                  "--bootstrap-dns" cfg.bootstrapDns
                  "--timeout-secs" (toString cfg.timeoutSecs)
                  "--workers" (toString cfg.workers)
                ] ++ lib.optionals (cfg.cpus != null) [
                  "--cpus" (lib.concatStringsSep "," (map toString cfg.cpus))
                ] ++ lib.optionals (cfg.tokenFile != null) [
                  "--token-file" cfg.tokenFile
                ] ++ lib.optionals cfg.verbose [
                  "--verbose"
                ] ++ lib.optionals cfg.pad [
                  "--pad"
                ] ++ lib.optionals cfg.busyPoll [
                  "--busy-poll"
                ] ++ lib.optionals cfg.spin [
                  "--spin"
                ] ++ lib.optionals cfg.mlockall [
                  "--mlockall"
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
