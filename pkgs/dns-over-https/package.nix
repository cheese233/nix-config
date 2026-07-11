{
  lib,
  fetchFromGitHub,
  buildGoModule,
  applyPatches,
  nix-update-script,
}:

buildGoModule (
  finalAttrs:
  let
    patches = [
      ./bearer-auth.patch
    ];
  in
  {
    pname = "dns-over-https";
    version = "2.3.10";

    src = applyPatches {
      src = fetchFromGitHub {
        owner = "m13253";
        repo = "dns-over-https";
        tag = "v${finalAttrs.version}";
        hash = "sha256-WQ6OyZfQMtW9nZcvlBjHk0R96NQr0Lc2mGB5taC0d6k=";
      };
      patches = patches;
    };

    vendorHash = "sha256-46BrN50G5IhdMwMVMU9Wdj/RFzUzIPoTRucCedMGu4g=";

    ldflags = [
      "-w"
      "-s"
    ];

    subPackages = [
      "doh-client"
      "doh-server"
    ];

    passthru.updateScript = nix-update-script { };

    meta = {
      homepage = "https://github.com/m13253/dns-over-https";
      changelog = "https://github.com/m13253/dns-over-https/releases/tag/v${finalAttrs.version}";
      description = "High performance DNS over HTTPS client & server (with bearer auth patch)";
      longDescription = ''
        Client and server software to query DNS over HTTPS, using Google
        DNS-over-HTTPS protocol and IETF DNS-over-HTTPS (RFC 8484).

        This build includes the bearer authentication patch which adds
        support for an Authorization: Bearer <token> header on upstream
        requests, configurable per-upstream or globally.
      '';
      license = lib.licenses.mit;
      maintainers = [ ];
      platforms = lib.platforms.all;
    };
  }
)
