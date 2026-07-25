{
  description = "Auto_Bangumi — automated anime download manager";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
    pyproject-nix = {
      url = "github:pyproject-nix/pyproject.nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    uv2nix = {
      url = "github:pyproject-nix/uv2nix";
      inputs.pyproject-nix.follows = "pyproject-nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    pnpm2nix-nzbr = {
      url = "github:FliegendeWurst/pnpm2nix-nzbr";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, pyproject-nix, uv2nix, pnpm2nix-nzbr }:
    let
      supportedSystems = [ "x86_64-linux" "aarch64-linux" ];
      forAllSystems = nixpkgs.lib.genAttrs supportedSystems;
    in
    {
      packages = forAllSystems (system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          lib = pkgs.lib;
          stdenv = pkgs.stdenv;

          version = "3.3.4";

          src = builtins.fetchTree {
            type = "github";
            owner = "EstrellaXD";
            repo = "Auto_Bangumi";
            rev = "bd6d35dbe50d6caa04e635d7bbbc244b3e138ea7";
          };

          webui = pnpm2nix-nzbr.packages.${system}.mkPnpmPackage {
            pname = "auto-bangumi-webui";
            inherit version;
            src = "${src}/webui";
            nodejs = pkgs.nodejs_22;
            pnpm = pkgs.pnpm_10;
            pnpmLockYaml = "${src}/webui/pnpm-lock.yaml";
            script = "build";
            distDir = "dist";
            distDirIsOut = true;
            buildEnv = { NODE_ENV = "production"; };
            env = { COREPACK_ENABLE_STRICT = "0"; PNPM_SELF_INSTALL = "false"; };
            preConfigure = ''
              sed -i '/"packageManager":/d' package.json
            '';
          };

          python = pkgs.python313;

          workspace = uv2nix.lib.workspace.loadWorkspace {
            workspaceRoot = "${src}/backend";
          };

          overlay = workspace.mkPyprojectOverlay {
            sourcePreference = "wheel";
          };

          pyproject-nix' = import "${pyproject-nix}/default.nix" { lib = pkgs.lib; };

          pythonSet =
            (pkgs.callPackage pyproject-nix'.build.packages { inherit python; })
            .overrideScope overlay;

          auto-bangumi-env = pythonSet.mkVirtualEnv "auto-bangumi-env"
            workspace.deps.default;

          auto-bangumi = stdenv.mkDerivation {
            pname = "auto-bangumi";
            inherit version src;

            dontBuild = true;
            doCheck = false;

            installPhase = ''
              mkdir -p $out/lib $out/bin

              # Symlink Python virtualenv
              mkdir -p $out/lib/.venv/lib
              ln -s ${auto-bangumi-env}/lib/python3.13 $out/lib/.venv/lib/python3.13
              ln -s ${auto-bangumi-env}/bin $out/lib/.venv/bin

              # Copy backend source
              cp -r backend/src/* $out/lib/

              # Copy webui dist
              mkdir -p $out/lib/dist
              cp -r ${webui}/* $out/lib/dist/

              # Write __version__.py (generated at build time in upstream Docker)
              cat > $out/lib/module/__version__.py << EOF
              __version__ = "${version}"
              __version_tuple__ = (${lib.concatStringsSep ", " (lib.splitVersion version)})
              EOF

              # Write IMAGE_VERSION
              echo "${version}" > $out/lib/IMAGE_VERSION

              # Copy public key
              cp backend/src/ab_update_pubkey.pem $out/lib/ 2>/dev/null || true

              # Write wrapper script
              cat > $out/bin/auto-bangumi << WRAPPER
              #!${stdenv.shell}
              export PYTHONPATH="${auto-bangumi-env}/lib/python3.13/site-packages:$out/lib:\$PYTHONPATH"
              export PATH="${auto-bangumi-env}/bin:\$PATH"
              exec python "$out/lib/main.py" "\$@"
              WRAPPER
              chmod +x $out/bin/auto-bangumi
            '';

            meta = with lib; {
              description = "AutoBangumi - Automated anime download manager";
              homepage = "https://github.com/EstrellaXD/Auto_Bangumi";
              license = licenses.mit;
              mainProgram = "auto-bangumi";
              platforms = platforms.linux;
            };
          };
        in
        {
          default = auto-bangumi;
          inherit auto-bangumi;
        }
      );

      nixosModules.default = { config, lib, pkgs, ... }:
        let
          cfg = config.services.auto-bangumi;
        in
        {
          options.services.auto-bangumi = {
            enable = lib.mkEnableOption "Auto_Bangumi — automated anime download manager";
            package = lib.mkPackageOption pkgs "auto-bangumi" { };
            host = lib.mkOption {
              type = lib.types.str;
              default = "0.0.0.0";
            };
            port = lib.mkOption {
              type = lib.types.port;
              default = 7892;
            };
            dataDir = lib.mkOption {
              type = lib.types.path;
              default = "/var/lib/auto-bangumi";
            };
            openFirewall = lib.mkOption {
              type = lib.types.bool;
              default = false;
            };
            extraEnv = lib.mkOption {
              type = lib.types.attrsOf lib.types.str;
              default = { };
            };
          };

          config = lib.mkIf cfg.enable {
            environment.systemPackages = [ cfg.package ];

            systemd.services.auto-bangumi = {
              description = "Auto_Bangumi — automated anime download manager";
              after = [ "network-online.target" ];
              wants = [ "network-online.target" ];
              wantedBy = [ "multi-user.target" ];

              serviceConfig = {
                Type = "simple";
                ExecStart = "${cfg.package}/bin/auto-bangumi";
                WorkingDirectory = cfg.dataDir;
                Restart = "always";
                RestartSec = "5s";
                DynamicUser = true;
                StateDirectory = "auto-bangumi";
                LogsDirectory = "auto-bangumi";
                NoNewPrivileges = true;
                ProtectSystem = "strict";
                ProtectHome = true;
                PrivateTmp = true;
                CapabilityBoundingSet = [ "" ];
                SystemCallArchitectures = "native";
                RestrictAddressFamilies = [ "AF_INET" "AF_INET6" "AF_UNIX" ];
              };

              environment = {
                HOST = cfg.host;
                PORT = toString cfg.port;
                HOME = cfg.dataDir;
              } // cfg.extraEnv;
            };

            networking.firewall = lib.mkIf cfg.openFirewall {
              allowedTCPPorts = [ cfg.port ];
            };

            systemd.tmpfiles.rules = [
              "d ${cfg.dataDir}/config 0750 - - - -"
              "d ${cfg.dataDir}/data 0750 - - - -"
              "d ${cfg.dataDir}/data/posters 0750 - - - -"
            ];
          };
        };
    };
}
