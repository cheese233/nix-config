{
  description = "Loyalsoldier's v2ray-rules-dat geosite.dat";

  inputs.nixpkgs.url = "github:nixos/nixpkgs/nixpkgs-unstable";

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
          default = pkgs.stdenv.mkDerivation {
            pname = "v2ray-rules-dat-geosite";
            version = "2026-07-22";

            src = pkgs.fetchurl {
              url = "https://cdn.jsdelivr.net/gh/Loyalsoldier/v2ray-rules-dat@release/geosite.dat";
              hash = "sha256-2cWrlHjm2aG6qNaOijGm0pIRTOGHOK6wNXBCJXUxdqw=";
            };

            dontUnpack = true;

            installPhase = ''
              runHook preInstall
              mkdir -p $out/share/v2ray
              cp $src $out/share/v2ray/geosite.dat
              runHook postInstall
            '';

            meta = with pkgs.lib; {
              description = "Enhanced geosite.dat from Loyalsoldier's v2ray-rules-dat";
              homepage = "https://github.com/Loyalsoldier/v2ray-rules-dat";
              license = licenses.gpl3Only;
              platforms = platforms.all;
            };
          };
        });
    };
}
