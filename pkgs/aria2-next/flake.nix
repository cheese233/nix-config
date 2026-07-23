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
              ln -s $out/bin/aria2-next $out/bin/aria2c
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

      nixosModules.default = { nixpkgs, ... }: {
        nixpkgs.overlays = [
          (final: prev: {
            aria2 = self.packages.${prev.stdenv.hostPlatform.system}.default;
          })
        ];
      };
    };
}
