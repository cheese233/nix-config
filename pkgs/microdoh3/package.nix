{ lib
, rustPlatform
}:

rustPlatform.buildRustPackage {
  pname = "microdoh3";
  version = "0.1.0";

  src = lib.cleanSource ./.;

  cargoHash = "sha256-0llRP1lo0GojwwUvqMqwC416XBJsgaxnoh4g1JcUD7Q=";

  meta = with lib; {
    description = "Minimal DNS-over-HTTP/3 proxy (Rust, noq QUIC, prefork per-core, 0-RTT, no async runtime)";
    license = licenses.mit;
    mainProgram = "microdoh3";
    platforms = platforms.linux;
  };
}
