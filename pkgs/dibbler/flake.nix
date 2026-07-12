{
  description = "Dibbler — portable DHCPv6 server, client, relay and requestor";

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
          stdenv = pkgs.stdenv;
        in
        rec {
          dibbler = stdenv.mkDerivation rec {
            pname = "dibbler";
            version = "1.0.2RC1";

            src = pkgs.fetchFromGitHub {
              owner = "tomaszmrugalski";
              repo = "dibbler";
              rev = "bb6e00b99361674ea6e4d4e6d19eff08f0e2f5d4";
              hash = ""; # FIXME:
            };

            # We use the pre-generated configure script (no autoreconf).
            # flex & bison are not required: parsers/lexers are pre-generated
            # and AM_MAINTAINER_MODE([disable]) prevents regeneration.

            nativeBuildInputs = with pkgs; [
              pkg-config
            ];

            buildInputs = with pkgs; [ ];

            # The configure script bundles bison++ as a sub-configure. It's
            # harmless but wastes a few cycles. We patch configure to skip it.
            postPatch = ''
              # Skip bison++ sub-configure (not needed for build)
              substituteInPlace configure \
                --replace 'subdirs="$subdirs bison++"' 'subdirs="$subdirs"'
            '';

            configureFlags = [
              # All features default to off; no explicit flags needed.
            ];

            enableParallelBuilding = true;

            # Dibbler's Makefile builds all subdirectories then links the
            # final binaries (dibbler-server, -client, -relay, -requestor).
            # `make` (default target) builds everything.

            installPhase = ''
              runHook preInstall

              mkdir -p $out/bin $out/share/dibbler/examples $out/share/dibbler/scripts

              for bin in dibbler-server dibbler-client dibbler-relay dibbler-requestor; do
                if [ -f "$bin" ]; then
                  install -m755 "$bin" "$out/bin/$bin"
                fi
              done

              for f in server client relay; do
                if [ -f "doc/examples/$f.conf" ]; then
                  cp "doc/examples/$f.conf" "$out/share/dibbler/examples/$f.conf.example"
                fi
              done

              cp scripts/notify-scripts/server-notify.sh "$out/share/dibbler/scripts/" 2>/dev/null || true
              cp scripts/notify-scripts/client-notify-linux.sh "$out/share/dibbler/scripts/" 2>/dev/null || true
              chmod +x "$out/share/dibbler/scripts/"*.sh 2>/dev/null || true

              runHook postInstall
            '';

            meta = with pkgs.lib; {
              description = "Portable DHCPv6 implementation (server, client, relay, requestor)";
              homepage = "http://klub.com.pl/dhcpv6/";
              license = licenses.gpl2Only;
              mainProgram = "dibbler-server";
              platforms = platforms.linux;
              longDescription = ''
                Dibbler is a portable, open source implementation of DHCPv6
                (RFC 3315, RFC 8415). It provides:
                - dibbler-server:  DHCPv6 server
                - dibbler-client:  DHCPv6 client
                - dibbler-relay:   DHCPv6 relay agent
                - dibbler-requestor: requestor/testing tool
              '';
            };
          };

          dibbler-server = dibbler;
          dibbler-client = dibbler;
          dibbler-relay = dibbler;
          dibbler-requestor = dibbler;
          default = dibbler;
        }
      );

      # ── NixOS modules ────────────────────────────────────────────

      nixosModules = {

        server = { config, lib, pkgs, ... }:
          let
            cfg = config.services.dibbler-server;
            pkg = cfg.package;
            # If inline config is provided, write it to the nix store;
            # otherwise fall back to configFile.
            serverConf = if cfg.config != null
              then pkgs.writeText "dibbler-server.conf" cfg.config
              else cfg.configFile;
          in
          {
            options.services.dibbler-server = {
              enable = lib.mkEnableOption "Dibbler DHCPv6 server";

              package = lib.mkOption {
                type = lib.types.package;
                default = self.packages.${pkgs.stdenv.hostPlatform.system}.dibbler;
                defaultText = lib.literalExpression ''
                  inputs.dibbler.packages.''${pkgs.stdenv.hostPlatform.system}.dibbler
                '';
                description = "Dibbler package to use.";
              };

              config = lib.mkOption {
                type = lib.types.nullOr lib.types.lines;
                default = null;
                example = ''
                  log-level 8
                  log-mode short
                  iface "eth0" {
                    class { pool 2001:db8:1111::/64 }
                    option dns-server 2000::ff,2000::fe
                    option domain example.com
                  }
                '';
                description = ''
                  Inline Dibbler server configuration. When set, this is written
                  to the nix store and used as the config file, overriding `configFile`.
                '';
              };

              configFile = lib.mkOption {
                type = lib.types.path;
                default = "${pkg}/share/dibbler/examples/server.conf.example";
                description = ''
                  Path to the dibbler server configuration file.
                  Ignored when `config` is set.
                '';
              };

              workDir = lib.mkOption {
                type = lib.types.str;
                default = "/var/lib/dibbler";
                description = "Runtime directory (PID file, lease database).";
              };

              logDir = lib.mkOption {
                type = lib.types.str;
                default = "/var/log/dibbler";
                description = "Log directory.";
              };
            };

            config = lib.mkIf cfg.enable {
              systemd.services.dibbler-server = {
                description = "Dibbler DHCPv6 Server";
                documentation = [ "http://klub.com.pl/dhcpv6/" ];
                after = [ "network.target" ];
                wants = [ "network.target" ];
                wantedBy = [ "multi-user.target" ];

                preStart = ''
                  mkdir -p ${cfg.workDir} ${cfg.logDir}
                '';

                serviceConfig = {
                  Type = "simple";
                  ExecStart = "${pkg}/bin/dibbler-server run";
                  WorkingDirectory = cfg.workDir;

                  Restart = "on-failure";
                  RestartSec = "5s";

                  # DHCPv6 needs raw sockets (UDP 546/547) and possibly
                  # NET_ADMIN for interface manipulation.
                  AmbientCapabilities = [
                    "CAP_NET_RAW"
                    "CAP_NET_BIND_SERVICE"
                    "CAP_NET_ADMIN"
                  ];
                  CapabilityBoundingSet = [
                    "CAP_NET_RAW"
                    "CAP_NET_BIND_SERVICE"
                    "CAP_NET_ADMIN"
                  ];

                  NoNewPrivileges = true;
                  PrivateTmp = true;
                  ProtectSystem = "strict";
                  ProtectHome = true;

                  ReadWritePaths = [
                    cfg.workDir
                    cfg.logDir
                  ];

                  BindReadOnlyPaths = [
                    "${serverConf}:/etc/dibbler/server.conf"
                  ];

                  RestrictAddressFamilies = [
                    "AF_INET"
                    "AF_INET6"
                    "AF_UNIX"
                    "AF_NETLINK"
                  ];

                  RestrictRealtime = true;
                  MemoryDenyWriteExecute = false;
                };
              };
            };
          };

        client = { config, lib, pkgs, ... }:
          let
            cfg = config.services.dibbler-client;
            pkg = cfg.package;
            clientConf = if cfg.config != null
              then pkgs.writeText "dibbler-client.conf" cfg.config
              else cfg.configFile;
          in
          {
            options.services.dibbler-client = {
              enable = lib.mkEnableOption "Dibbler DHCPv6 client";

              package = lib.mkOption {
                type = lib.types.package;
                default = self.packages.${pkgs.stdenv.hostPlatform.system}.dibbler;
                defaultText = lib.literalExpression ''
                  inputs.dibbler.packages.''${pkgs.stdenv.hostPlatform.system}.dibbler
                '';
                description = "Dibbler package to use.";
              };

              config = lib.mkOption {
                type = lib.types.nullOr lib.types.lines;
                default = null;
                example = ''
                  log-mode short
                  log-level 7
                  iface "eth0" {
                    ia
                    option dns-server
                    option domain
                  }
                '';
                description = ''
                  Inline Dibbler client configuration. When set, this is written
                  to the nix store and used as the config file, overriding `configFile`.
                '';
              };

              configFile = lib.mkOption {
                type = lib.types.path;
                default = "${pkg}/share/dibbler/examples/client.conf.example";
                description = ''
                  Path to the dibbler client configuration file.
                  Ignored when `config` is set.
                '';
              };

              workDir = lib.mkOption {
                type = lib.types.str;
                default = "/var/lib/dibbler";
                description = "Runtime directory (PID file, DUID).";
              };

              logDir = lib.mkOption {
                type = lib.types.str;
                default = "/var/log/dibbler";
                description = "Log directory.";
              };
            };

            config = lib.mkIf cfg.enable {
              systemd.services.dibbler-client = {
                description = "Dibbler DHCPv6 Client";
                documentation = [ "http://klub.com.pl/dhcpv6/" ];
                after = [ "network.target" ];
                wants = [ "network.target" ];
                wantedBy = [ "multi-user.target" ];

                preStart = ''
                  mkdir -p ${cfg.workDir} ${cfg.logDir}
                '';

                serviceConfig = {
                  Type = "simple";
                  ExecStart = "${pkg}/bin/dibbler-client run";
                  WorkingDirectory = cfg.workDir;

                  Restart = "on-failure";
                  RestartSec = "5s";

                  AmbientCapabilities = [
                    "CAP_NET_RAW"
                    "CAP_NET_BIND_SERVICE"
                    "CAP_NET_ADMIN"
                  ];
                  CapabilityBoundingSet = [
                    "CAP_NET_RAW"
                    "CAP_NET_BIND_SERVICE"
                    "CAP_NET_ADMIN"
                  ];

                  NoNewPrivileges = true;
                  PrivateTmp = true;
                  ProtectSystem = "strict";
                  ProtectHome = true;

                  ReadWritePaths = [
                    cfg.workDir
                    cfg.logDir
                  ];

                  BindReadOnlyPaths = [
                    "${clientConf}:/etc/dibbler/client.conf"
                  ];

                  RestrictAddressFamilies = [
                    "AF_INET"
                    "AF_INET6"
                    "AF_UNIX"
                    "AF_NETLINK"
                  ];

                  RestrictRealtime = true;
                  MemoryDenyWriteExecute = false;
                };
              };
            };
          };

        relay = { config, lib, pkgs, ... }:
          let
            cfg = config.services.dibbler-relay;
            pkg = cfg.package;
            relayConf = if cfg.config != null
              then pkgs.writeText "dibbler-relay.conf" cfg.config
              else cfg.configFile;
          in
          {
            options.services.dibbler-relay = {
              enable = lib.mkEnableOption "Dibbler DHCPv6 relay agent";

              package = lib.mkOption {
                type = lib.types.package;
                default = self.packages.${pkgs.stdenv.hostPlatform.system}.dibbler;
                defaultText = lib.literalExpression ''
                  inputs.dibbler.packages.''${pkgs.stdenv.hostPlatform.system}.dibbler
                '';
                description = "Dibbler package to use.";
              };

              config = lib.mkOption {
                type = lib.types.nullOr lib.types.lines;
                default = null;
                example = ''
                  log-level 8
                  log-mode short
                  iface "eth0" {
                    server-unicast 2001:db8::1
                    client-unicast 2001:db8::2
                  }
                '';
                description = ''
                  Inline Dibbler relay configuration. When set, this is written
                  to the nix store and used as the config file, overriding `configFile`.
                '';
              };

              configFile = lib.mkOption {
                type = lib.types.path;
                default = "${pkg}/share/dibbler/examples/relay.conf.example";
                description = ''
                  Path to the dibbler relay configuration file.
                  Ignored when `config` is set.
                '';
              };

              workDir = lib.mkOption {
                type = lib.types.str;
                default = "/var/lib/dibbler";
                description = "Runtime directory (PID file).";
              };

              logDir = lib.mkOption {
                type = lib.types.str;
                default = "/var/log/dibbler";
                description = "Log directory.";
              };
            };

            config = lib.mkIf cfg.enable {
              systemd.services.dibbler-relay = {
                description = "Dibbler DHCPv6 Relay Agent";
                documentation = [ "http://klub.com.pl/dhcpv6/" ];
                after = [ "network.target" ];
                wants = [ "network.target" ];
                wantedBy = [ "multi-user.target" ];

                preStart = ''
                  mkdir -p ${cfg.workDir} ${cfg.logDir}
                '';

                serviceConfig = {
                  Type = "simple";
                  ExecStart = "${pkg}/bin/dibbler-relay run";
                  WorkingDirectory = cfg.workDir;

                  Restart = "on-failure";
                  RestartSec = "5s";

                  AmbientCapabilities = [
                    "CAP_NET_RAW"
                    "CAP_NET_BIND_SERVICE"
                    "CAP_NET_ADMIN"
                  ];
                  CapabilityBoundingSet = [
                    "CAP_NET_RAW"
                    "CAP_NET_BIND_SERVICE"
                    "CAP_NET_ADMIN"
                  ];

                  NoNewPrivileges = true;
                  PrivateTmp = true;
                  ProtectSystem = "strict";
                  ProtectHome = true;

                  ReadWritePaths = [
                    cfg.workDir
                    cfg.logDir
                  ];

                  BindReadOnlyPaths = [
                    "${relayConf}:/etc/dibbler/relay.conf"
                  ];

                  RestrictAddressFamilies = [
                    "AF_INET"
                    "AF_INET6"
                    "AF_UNIX"
                    "AF_NETLINK"
                  ];

                  RestrictRealtime = true;
                  MemoryDenyWriteExecute = false;
                };
              };
            };
          };

        # Combined module: imports server, client, and relay modules.
        default = { ... }:
          {
            imports = [
              self.nixosModules.server
              self.nixosModules.client
              self.nixosModules.relay
            ];
          };

      };
    };
}
