{
  description = "mdns-publisher — minimal mDNS hostname responder (pure Go, no CGO)";

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
          default = pkgs.buildGoModule {
            pname = "mdns-publisher";
            version = "0.1.0";
            src = ./.;

            # Hash of the vendored Go module tree. On first build Nix
            # prints the correct SRI; paste it here, or set
            # `nixpkgs.lib.fakeHash` to bootstrap.
            vendorHash = "";

            # Pure-Go build — no C toolchain, no libc linkage.
            env.CGO_ENABLED = 0;

            # strip debug info + symbol table for a smaller binary.
            ldflags = [ "-s" "-w" ];

            # Run `go test ./...` during the build.  Tests that need
            # port 5353 automatically skip in the Nix sandbox.
            doCheck = true;

            meta = with pkgs.lib; {
              description = "Minimal mDNS responder publishing this host's A/AAAA records as <hostname>.local";
              longDescription = ''
                mdns-publisher listens on a single network interface and
                answers mDNS A/AAAA queries for "<hostname>.local" with
                the interface's global unicast IPv4/IPv6 addresses.

                Pair with smartdns's `mdns-lookup yes` (or any other
                mDNS-aware DNS resolver) to expose this host under a
                .local name on the LAN without a full Zeroconf stack
                (avahi/bonjour).

                No service browsing, no reflection between interfaces,
                no dbus, no extra daemons.
              '';
              license = licenses.mit;
              mainProgram = "mdns-publisher";
              maintainers = [ ];
              platforms = platforms.linux;
            };
          };
        });

      # `nix flake check` runs the test suite.
      checks = forAllSystems (system: {
        default = self.packages.${system}.default;
      });

      # Standalone-isable NixOS module.
      #
      # Usage in another flake:
      #   inputs.mdns-publisher.url = "github:<you>/mdns-publisher";
      #   modules = [ inputs.mdns-publisher.nixosModules.default ];
      #   services.mdns-publisher.enable = true;
      nixosModules.default = { config, lib, pkgs, ... }:
        let
          cfg = config.services.mdns-publisher;
        in
        {
          options.services.mdns-publisher = {
            enable = lib.mkEnableOption ''
              mdns-publisher, a minimal mDNS responder that advertises
              this host as "<hostname>.local" on a chosen interface
            '';

            package = lib.mkOption {
              type = lib.types.package;
              default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
              defaultText = lib.literalExpression ''
                inputs.mdns-publisher.packages.''${pkgs.stdenv.hostPlatform.system}.default
              '';
              description = "mdns-publisher package to use.";
            };

            interface = lib.mkOption {
              type = lib.types.str;
              example = "br-lan";
              description = ''
                Network interface whose IPv4/IPv6 addresses will be
                advertised as the A/AAAA records of "<hostname>.local".
                The mDNS multicast listener is bound to this interface.
              '';
            };

            hostname = lib.mkOption {
              type = lib.types.nullOr lib.types.str;
              default = null;
              example = "nixos";
              description = ''
                Hostname (without the trailing ".local") to advertise.
                Null = use the system hostname ({option}`networking.hostName`).
              '';
            };

            ttl = lib.mkOption {
              type = lib.types.ints.positive;
              default = 120;
              description = "TTL in seconds for published A/AAAA records.";
            };

            openFirewall = lib.mkOption {
              type = lib.types.bool;
              default = false;
              description = ''
                Open {option}`networking.firewall` (the legacy
                iptables-based firewall) for incoming mDNS multicast
                (UDP 5353) so the responder can receive queries.
                No effect when using `networking.nftables.firewall` or
                any other firewall stack — configure those manually.
              '';
            };
          };

          config = lib.mkIf cfg.enable {
            systemd.services.mdns-publisher = {
              description = "mdns-publisher — minimal mDNS hostname responder";
              after = [ "network-online.target" ];
              wants = [ "network-online.target" ];
              wantedBy = [ "multi-user.target" ];

              serviceConfig = let
                args = [ "${cfg.package}/bin/mdns-publisher" "-iface" cfg.interface ]
                  ++ lib.optionals (cfg.hostname != null) [ "-hostname" cfg.hostname ]
                  ++ [ "-ttl" (toString cfg.ttl) ];
              in {
                Type = "simple";
                ExecStart = lib.escapeShellArgs args;
                Restart = "always";
                RestartSec = 5;
                # hardening: tiny privilege surface. mDNS needs:
                #   AF_INET / AF_INET6: send + receive UDP multicast
                #   AF_NETLINK:          read interface table via netlink
                #   AF_UNIX:             Go runtime / systemd-notify
                DynamicUser = true;
                NoNewPrivileges = true;
                CapabilityBoundingSet = [ "" ];
                ProtectSystem = "strict";
                ProtectHome = true;
                PrivateTmp = true;
                PrivateDevices = true;
                ProtectKernelTunables = true;
                ProtectKernelModules = true;
                ProtectControlGroups = true;
                RestrictAddressFamilies = [ "AF_INET" "AF_INET6" "AF_NETLINK" "AF_UNIX" ];
                RestrictNamespaces = true;
                RestrictRealtime = true;
                RestrictSUIDSGID = true;
                LockPersonality = true;
                MemoryDenyWriteExecute = true;
                SystemCallArchitectures = "native";
                SystemCallFilter = [ "@system-service" ];
                SystemCallErrorNumber = "EPERM";
              };
            };

            networking.firewall =
              lib.optionalAttrs cfg.openFirewall { allowedUDPPorts = [ 5353 ]; };
          };
        };
    };
}
