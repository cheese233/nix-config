{ lib
, rustPlatform
, pkg-config
, curl
, openssl
, enableH3 ? false
}:

rustPlatform.buildRustPackage {
  pname = "microdoh";
  version = "0.1.0";

  src = lib.cleanSource ./.;

  cargoHash = lib.fakeHash; # FIXME: fill after first build

  nativeBuildInputs = [ pkg-config ];
  buildInputs = [
    (if enableH3 then (curl.override { http3Support = true; }) else curl)
    openssl
  ];

  meta = with lib; {
    description = "Minimal DNS-over-HTTPS proxy (Rust, libcurl, RFC 8484 GET, 0-RTT, Bearer auth)";
    license = licenses.mit;
    mainProgram = "microdoh";
    platforms = platforms.linux;
  };
}
