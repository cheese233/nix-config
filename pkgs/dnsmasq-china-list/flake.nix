{
  description = "dnsmasq-china-list converted to plain domain list";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixpkgs-unstable";
    dnsmasq-china-list = {
      url = "github:felixonmars/dnsmasq-china-list";
      flake = false;
    };
  };

  outputs = { self, nixpkgs, dnsmasq-china-list, ... }:
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
            pname = "dnsmasq-china-list";
            version = toString (dnsmasq-china-list.lastModified or "unstable");
            src = dnsmasq-china-list;

            dontBuild = true;

            installPhase = ''
              runHook preInstall
              mkdir -p $out/etc
              awk -F'/' '{print $2}' accelerated-domains.china.conf \
                | sed '/^$/d' \
                > $out/etc/china-domain-list.txt
              runHook postInstall
            '';

            meta = with pkgs.lib; {
              description = "dnsmasq-china-list converted to plain domain list format";
              license = licenses.mit;
              maintainers = [ ];
            };
          };
        });
    };
}
