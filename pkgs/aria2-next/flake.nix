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

          aria2NextSrc = pkgs.fetchFromGitHub {
            owner = "AnInsomniacy";
            repo = "aria2-next";
            rev = "v2.6.2";
            hash = "sha256-OmLC0Mhm+MDwkCENO7cpSel2Oc3cTu9QBG7IxugFyGY=";
          };

          # Upstream requires libtorrent-rasterbar >= 2.1.1 while nixpkgs only
          # ships 2.0.x; build the copy vendored inside the aria2-next tree.
          libtorrent-rasterbar-vendored = stdenv.mkDerivation {
            pname = "libtorrent-rasterbar";
            version = "2.1.1";

            src = aria2NextSrc + "/third_party/libtorrent";

            nativeBuildInputs = with pkgs; [
              cmake
              ninja
              pkg-config
            ];

            buildInputs = with pkgs; [
              boost
              openssl
            ];

            # The generated .pc file joins paths incorrectly and we consume
            # this package via its CMake config anyway.
            postInstall = ''
              rm -rf $out/lib/pkgconfig
            '';

            cmakeFlags = [
              "-DCMAKE_INSTALL_LIBDIR=lib"
              "-DCMAKE_INSTALL_INCLUDEDIR=include"
              # Upstream links libtorrent statically (its TORRENT_EXTRA_EXPORT
              # symbols stay hidden in a shared build); mirror that here.
              "-DBUILD_SHARED_LIBS=OFF"
              "-Dstatic_runtime=OFF"
              "-Ddeprecated-functions=OFF"
              "-Dextensions=ON"
              "-Dmutable-torrents=ON"
              "-Dstreaming=ON"
              "-Di2p=OFF"
              "-Dwebtorrent=OFF"
              "-Dlogging=OFF"
              "-Dencryption=ON"
              "-Ddht=ON"
            ];
          };

          aria2-next = stdenv.mkDerivation rec {
            pname = "aria2-next";
            version = "2.6.2";

            src = aria2NextSrc;

            patches = [
              ./private-network-access.patch
              ./cmake-system-deps.patch
            ];

            nativeBuildInputs = with pkgs; [
              cmake
              ninja
              pkg-config
            ];

            buildInputs = with pkgs; [
              openssl
              zlib
              expat
              sqlite
              curl
              nghttp2
              boost
              libtorrent-rasterbar-vendored
            ];

            cmakeFlags = [
              "-DARIA2_SUPERBUILD=OFF"
              "-DARIA2_ENABLE_BITTORRENT=ON"
              "-DARIA2_ENABLE_METALINK=ON"
              "-DARIA2_ENABLE_WEBSOCKET=ON"
              "-DARIA2_ENABLE_EPOLL=ON"
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
                with extensive bug fixes and a modernized CMake build system.
                HTTP/HTTPS/SFTP/Metalink transfers use libcurl + nghttp2 and
                BitTorrent uses libtorrent-rasterbar. Compatible with existing
                aria2 CLI, configuration, sessions, and JSON-RPC interfaces.
              '';
            };
          };
        in
        rec {
          inherit aria2-next;
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
