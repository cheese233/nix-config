{
  description = "aria2-next — maintained aria2 fork with bug fixes and modernized architecture";

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
          aria2-next = stdenv.mkDerivation rec {
            pname = "aria2-next";
            version = "2.5.2";

            src = pkgs.fetchFromGitHub {
              owner = "AnInsomniacy";
              repo = "aria2-next";
              rev = "v${version}";
              hash = "sha256-1QcdgU03qTk1yovTYv4udMA6TKKZEWPEXOCtlIdNhZw=";
            };

            nativeBuildInputs = with pkgs; [
              cmake
              ninja
              pkg-config
            ];

            buildInputs = with pkgs; [
              openssl
              c-ares
              libssh2
              sqlite
              zlib
              expat
            ];

            cmakeFlags = [
              "-DARIA2_ENABLE_SSL=ON"
              "-DARIA2_ENABLE_BITTORRENT=ON"
              "-DARIA2_ENABLE_METALINK=ON"
              "-DARIA2_ENABLE_WEBSOCKET=ON"
              "-DARIA2_ENABLE_EPOLL=ON"
              "-DARIA2_WITH_OPENSSL=ON"
              "-DARIA2_WITH_CARES=ON"
              "-DARIA2_WITH_SQLITE3=ON"
              "-DARIA2_WITH_LIBSSH2=ON"
              "-DARIA2_WITH_EXPAT=ON"
            ];

            enableParallelBuilding = true;

            postInstall = ''
              mkdir -p $out/share/bash-completion/completions
              cp $src/doc/bash_completion/aria2c $out/share/bash-completion/completions/aria2-next 2>/dev/null || true
            '';

            meta = with pkgs.lib; {
              description = "Maintained aria2 fork with extensive bug fixes and modernized architecture";
              homepage = "https://github.com/AnInsomniacy/aria2-next";
              license = licenses.gpl2Only;
              mainProgram = "aria2-next";
              platforms = platforms.linux;
              longDescription = ''
                Aria2 Next is an actively maintained aria2-compatible download engine
                with extensive bug fixes, modernized CMake build system, and native
                ED2K/eMule support. Compatible with existing aria2 CLI, configuration,
                sessions, and JSON-RPC interfaces.
              '';
            };
          };

          default = aria2-next;
        }
      );

      nixosModules.default = { config, lib, pkgs, ... }:
      let
        cfg = config.services.aria2-next;
      in
      {
        options.services.aria2-next = {
          enable = lib.mkEnableOption "aria2-next — maintained aria2 fork";
          package = lib.mkOption {
            type = lib.types.package;
            default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
            description = "The aria2-next package to use.";
          };
        };

        config = lib.mkIf cfg.enable {
          services.aria2.enable = true;
          systemd.services.aria2.serviceConfig.ExecStart = lib.mkForce (
            "${cfg.package}/bin/aria2-next --conf-path=${config.services.aria2.settings.conf-path}"
          );
        };
      };
    };
}
