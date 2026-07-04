{
  description = "smartdns — patched fork of pymumu/smartdns";

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
          # Mirrors nixpkgs':pkgs/by-name/sm/smartdns/package.nix` but
          # adds ./mdns-ipv6.patch, which fixes `mdns-lookup yes`
          # silently adding zero mDNS upstreams on IPv6-only interfaces
          # (e.g. a bridge carrying only a ULA address). Without the
          # patch, `.local` queries fall through to public recursion
          # and you get back a Verisign SOA instead of an mDNS answer.
          default = pkgs.stdenv.mkDerivation (finalAttrs: {
            pname = "smartdns";
            version = "47";

            src = pkgs.fetchFromGitHub {
              owner = "pymumu";
              repo = "smartdns";
              rev = "Release${finalAttrs.version}";
              hash = "sha256-8OK1OV3Jvj/5nUOxnWTTQAa1Qe3RGxNwJhYEZ7O1RIE=";
            };

            patches = [ ./mdns-ipv6.patch ];

            buildInputs = [ pkgs.openssl ];

            makeFlags = [
              "PREFIX=${placeholder "out"}"
              "SYSTEMDSYSTEMUNITDIR=${placeholder "out"}/lib/systemd/system"
              "RUNSTATEDIR=/run"
              # by default it is the build time... weird... https://github.com/pymumu/smartdns/search?q=ver
              "VER=${finalAttrs.version}"
            ];

            installFlags = [ "SYSCONFDIR=${placeholder "out"}/etc" ];

            passthru.tests = {
              version = pkgs.testers.testVersion {
                package = finalAttrs.finalPackage;
                command = "smartdns -v";
              };
            };

            meta = {
              description = "Local DNS server to obtain the fastest website IP for the best Internet experience";
              longDescription = ''
                SmartDNS is a local DNS server. SmartDNS accepts DNS
                query requests from local clients, obtains DNS query
                results from multiple upstream DNS servers, and returns
                the fastest access results to clients. Avoiding DNS
                pollution and improving network access speed, supports
                high-performance ad filtering.

                This is a patched build: ./mdns-ipv6.patch fixes
                `_dns_client_add_mdns_server()` so that IPv6 interfaces
                are recognised by `is_private_addr()` — without the
                fix, `mdns-lookup yes` silently adds zero upstreams on
                IPv6-only LLMs (e.g. a bridge with only a ULA address)
                and `.local` queries fall through to public recursion.
              '';
              homepage = "https://github.com/pymumu/smartdns";
              maintainers = [ ];
              license = pkgs.lib.licenses.gpl3Plus;
              platforms = pkgs.lib.platforms.linux;
              mainProgram = "smartdns";
            };
          });
        });

      checks = forAllSystems (system: {
        default = self.packages.${system}.default;
      });

      # Drop-in overlay module: importing this into a NixOS config
      # replaces `pkgs.smartdns` with the patched build, so the stock
      # `services.smartdns` module (from nixpkgs) automatically picks
      # up the fix without per-option overrides.
      #
      # Usage:
      #   inputs.smartdns.url = "github:<you>/smartdns";  (or path:./pkgs/smartdns)
      #   modules = [ inputs.smartdns.nixosModules.default ];
      nixosModules.default = { config, lib, pkgs, ... }:
        {
          nixpkgs.overlays = [
            (final: prev:
              {
                smartdns = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
              })
          ];
        };
    };
}
