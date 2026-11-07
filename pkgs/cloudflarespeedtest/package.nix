# XIU2/CloudflareSpeedTest, taken as the upstream prebuilt release binary.
#
# It is a static Go binary (no DT_NEEDED, no ELF interpreter), so no patchelf
# is needed. The release tarball also carries the two IP-range lists the tool
# samples from — `ip.txt` (Cloudflare IPv4, one random address per /24) and
# `ipv6.txt` (curated IPv6 prefixes) — and those lists are the part this repo
# actually depends on, so the release is used as-is instead of rebuilding it
# from source.
#
# To bump: change `version`, then
#   nix-prefetch-url --type sha256 \
#     https://github.com/XIU2/CloudflareSpeedTest/releases/download/v<version>/cfst_linux_amd64.tar.gz
#   nix hash convert --hash-algo sha256 --to sri <hash>
{
  lib,
  stdenvNoCC,
  fetchurl,
}:

stdenvNoCC.mkDerivation (finalAttrs: {
  pname = "cloudflarespeedtest";
  version = "2.3.5";

  src = fetchurl {
    url = "https://github.com/XIU2/CloudflareSpeedTest/releases/download/v${finalAttrs.version}/cfst_linux_amd64.tar.gz";
    hash = "sha256-xMj8drThvyvbXO2LdllW2C3ae8TrWd9cBAU/D325jZA=";
  };

  dontConfigure = true;
  dontBuild = true;

  installPhase = ''
    runHook preInstall
    install -Dm755 cfst $out/bin/cfst
    install -Dm644 ip.txt $out/share/cfst/ip.txt
    install -Dm644 ipv6.txt $out/share/cfst/ipv6.txt
    runHook postInstall
  '';

  meta = {
    description = "Cloudflare CDN latency/throughput tester that picks the fastest IPs (优选 IP)";
    homepage = "https://github.com/XIU2/CloudflareSpeedTest";
    license = lib.licenses.gpl3Only;
    sourceProvenance = [ lib.sourceTypes.binaryNativeCode ];
    platforms = [ "x86_64-linux" ];
    mainProgram = "cfst";
  };
})
