{ lib
, rustPlatform
}:

rustPlatform.buildRustPackage {
  pname = "microdoh3";
  version = "0.1.0";

  src = lib.cleanSource ./.;

  cargoHash = "sha256-ZWpKHrr8RfOGOTuVcWCdT27qD/Ivj2V+LGj/j+yFdWo=";

  meta = with lib; {
    description = "Minimal DNS-over-HTTP/3 proxy (Rust, noq QUIC, prefork per-core, 0-RTT, no async runtime)";
    license = licenses.mit;
    mainProgram = "microdoh3";
    platforms = platforms.linux;
  };
}
