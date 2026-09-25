{
  description = "honk, an eBPF transparent proxy engine";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
  inputs.rust-overlay = {
    url = "github:oxalica/rust-overlay";
    inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs = { self, nixpkgs, rust-overlay }:
    let
      supportedSystems = [ "x86_64-linux" ];
      forAllSystems = nixpkgs.lib.genAttrs supportedSystems;
      version = "0.0.1-alpha";
      rev = "c773b8679037825e63d92d802f2c0c21a512c9d1";
      sourceHash = "sha256-Aa21DbNArI8yu9P6lpyRvIH1uwfSes8xyHGT5y6V/ys=";
    in
    {
      packages = forAllSystems (system:
        let
          pkgs = import nixpkgs {
            inherit system;
            overlays = [ (import rust-overlay) ];
          };
          # crates/honk-ebpf/rust-toolchain.toml pins this channel for the object.
          rustNightly = pkgs.rust-bin.nightly."2026-07-20".default.override {
            extensions = [ "rust-src" ];
          };
          # rust-toolchain.toml pins stable 1.98.1 for the userspace crates;
          # the locked rust-overlay carries 1.98.0 (same patch level).
          rustStable = pkgs.rust-bin.stable."1.98.0".default;
          honkRustPlatform = pkgs.makeRustPlatform {
            cargo = rustStable;
            rustc = rustStable;
          };
          bpf-linker = pkgs.stdenv.mkDerivation {
            pname = "bpf-linker";
            version = "0.11.0";
            src = pkgs.fetchurl {
              url = "https://github.com/aya-rs/bpf-linker/releases/download/v0.11.0/bpf-linker-x86_64-unknown-linux-musl.tar.zst";
              hash = "sha256-EPYrqat+VE1Tg3BVJmDvy08aGRU9V1K78Pa1HzutpFA=";
            };
            nativeBuildInputs = [ pkgs.zstd ];
            dontConfigure = true;
            dontBuild = true;
            dontUnpack = true;
            installPhase = ''
              zstd -dc "$src" | tar -xf -
              install -Dm755 bpf-linker $out/bin/bpf-linker
            '';
          };
          src = pkgs.fetchFromGitHub {
            owner = "cheese233";
            repo = "honk";
            inherit rev;
            hash = sourceHash;
          };

          honk-ebpf = pkgs.rustPlatform.buildRustPackage {
            pname = "honk-ebpf";
            inherit version src;
            cargoRoot = "crates/honk-ebpf";
            cargoLock.lockFile = "${src}/crates/honk-ebpf/Cargo.lock";

            nativeBuildInputs = [ bpf-linker rustNightly ];
            env = {
              RUST_SRC_PATH = "${rustNightly}/lib/rustlib/src/rust";
            };

            # Upstream now sets `linker=bpf-linker` directly, so the linker is
            # resolved from PATH (bpf-linker is a nativeBuildInput); only the
            # stack-size cap still needs narrowing.
            postPatch = ''
              substituteInPlace crates/honk-ebpf/.cargo/config.toml \
                --replace-fail 'bpf-stack-size=4096' 'bpf-stack-size=512'
            '';

            buildPhase = ''
              runHook preBuild
              vendor_dir=$(find "$NIX_BUILD_TOP" -type d -name cargo-vendor-dir -print -quit)
              rustc_sysroot=$(${rustNightly}/bin/rustc --print sysroot)
              for crate in ${pkgs.rustPlatform.rustVendorSrc}/*; do
                crate_name=$(basename "$crate")
                if [ ! -e "$vendor_dir/$crate_name" ]; then
                  ln -s "$crate" "$vendor_dir/$crate_name"
                fi
              done
              for crate in "$rustc_sysroot/lib/rustlib/src/rust/library/vendor"/*; do
                crate_name=$(basename "$crate")
                if [ ! -e "$vendor_dir/$crate_name" ]; then
                  ln -s "$crate" "$vendor_dir/$crate_name"
                fi
              done
              build_sysroot="$TMPDIR/honk-rust-sysroot"
              mkdir -p "$build_sysroot/lib/rustlib"
              for entry in "$rustc_sysroot/lib/rustlib"/*; do
                [ "$(basename "$entry")" = src ] || cp -rs "$entry" "$build_sysroot/lib/rustlib/"
              done
              mkdir -p "$build_sysroot/lib/rustlib/src"
              ln -s "$rustc_sysroot/lib/rustlib/src/rust" "$build_sysroot/lib/rustlib/src/rust"
              real_rustc=${rustNightly}/bin/rustc
              cat > "$TMPDIR/honk-rustc" <<EOF
              #!/bin/sh
              exec "$real_rustc" --sysroot "$build_sysroot" "\$@"
              EOF
              chmod +x "$TMPDIR/honk-rustc"
              export RUSTC="$TMPDIR/honk-rustc"
              cd crates/honk-ebpf
              ${rustNightly}/bin/cargo build --release --offline -Zbuild-std=core --target bpfel-unknown-none
              runHook postBuild
            '';

            doCheck = false;
            dontCargoInstall = true;
            installPhase = ''
              runHook preInstall
              object=$(find . -path '*/target/bpfel-unknown-none/release/honk-ebpf' -type f -print -quit)
              test -n "$object"
              install -Dm644 "$object" $out/lib/honk-ebpf.o
              runHook postInstall
            '';
          };

          honk-core-build = ./core-build.rs;
          honk = honkRustPlatform.buildRustPackage {
            pname = "honk";
            inherit version src;
            cargoLock = {
              lockFile = "${src}/Cargo.lock";
              outputHashes = {
                "boring-sys-5.2.0" = "sha256-VG0POjZdA2JazFt1jFe4UfdO01Pw7E1EC/7u8fOmfBs=";
                "quinn-0.11.11" = "sha256-G2isJHacUgeSe852A3a8WH/Amh0i5h0V1ziho1jV3Rs=";
                "quinn-proto-0.11.17" = "sha256-G2isJHacUgeSe852A3a8WH/Amh0i5h0V1ziho1jV3Rs=";
                "quinn-udp-0.5.15" = "sha256-G2isJHacUgeSe852A3a8WH/Amh0i5h0V1ziho1jV3Rs=";
              };
            };
            buildFeatures = [ "ebpf" ];
            cargoBuildFlags = [ "-p" "honk-core" ];

            nativeBuildInputs = with pkgs; [
              cmake
              gitMinimal
              pkg-config
              llvmPackages.libclang
            ];
            env = {
              HONK_EBPF_OBJECT = "${honk-ebpf}/lib/honk-ebpf.o";
              BINDGEN_EXTRA_CLANG_ARGS = "-isystem ${pkgs.glibc.dev}/include -isystem ${pkgs.stdenv.cc.cc}/include";
              LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
            };

            postPatch = ''
              cp ${honk-core-build} crates/honk-core/build.rs
            '';

            doCheck = false;
            meta = with pkgs.lib; {
              description = "An eBPF-based transparent proxy engine inspired by dae and sing-box";
              homepage = "https://github.com/cheese233/honk";
              license = licenses.gpl3Only;
              mainProgram = "honk-core";
              platforms = platforms.linux;
            };
          };
        in
        {
          default = honk;
          inherit honk honk-ebpf;
        });

      nixosModules.default = { config, lib, pkgs, ... }:
        let
          cfg = config.network.honk;
          genAssetsDrv = paths: pkgs.symlinkJoin {
            name = "honk-assets";
            inherit paths;
          };
          configPath = if cfg.config != null then "/etc/honk/config.dae" else cfg.configFile;
        in
        {
          options.network.honk = {
            enable = lib.mkEnableOption "honk, an eBPF transparent proxy engine";

            package = lib.mkOption {
              type = lib.types.package;
              default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
              defaultText = lib.literalExpression "inputs.honk.packages.\${pkgs.system}.default";
              description = "honk package to use.";
            };

            assets = lib.mkOption {
              type = lib.types.listOf lib.types.path;
              default = with pkgs; [ v2ray-geoip v2ray-domain-list-community ];
              description = "Packages containing honk's geoip.dat and geosite.dat assets.";
            };

            assetsPath = lib.mkOption {
              type = lib.types.pathInStore;
              default = "${genAssetsDrv cfg.assets}/share/v2ray";
              description = "Directory containing geoip.dat and geosite.dat.";
            };

            config = lib.mkOption {
              type = lib.types.nullOr lib.types.lines;
              default = null;
              description = "Inline honk configuration. It is written to /etc/honk/config.dae.";
            };

            configFile = lib.mkOption {
              type = lib.types.str;
              default = "/etc/honk/config.dae";
              description = "Absolute path to the honk configuration file.";
            };

            openFirewall = lib.mkOption {
              type = lib.types.submodule {
                options = {
                  enable = lib.mkEnableOption "opening the honk transparent proxy port";
                  port = lib.mkOption {
                    type = lib.types.port;
                    default = 12345;
                    description = "The TCP and UDP transparent proxy port.";
                  };
                };
              };
              default = { enable = true; port = 12345; };
            };
          };

          config = lib.mkIf cfg.enable {
            environment.etc = lib.mkIf (cfg.config != null) {
              "honk/config.dae" = {
                mode = "0400";
                text = cfg.config;
              };
            };

            networking.firewall = lib.mkIf cfg.openFirewall.enable {
              allowedTCPPorts = [ cfg.openFirewall.port ];
              allowedUDPPorts = [ cfg.openFirewall.port ];
            };

            systemd.services.honk = {
              description = "honk transparent proxy";
              after = [ "network-online.target" ];
              wants = [ "network-online.target" ];
              wantedBy = [ "multi-user.target" ];
              serviceConfig = {
                Type = "simple";
                ExecStart = lib.escapeShellArgs [
                  "${cfg.package}/bin/honk-core"
                  "--config"
                  configPath
                ];
                Environment = "DAE_LOCATION_ASSET=${cfg.assetsPath}";
                StateDirectory = "honk";
                WorkingDirectory = "/var/lib/honk";
                Restart = "on-failure";
                RestartSec = 5;
                TimeoutStartSec = 120;
              };
            };
          };
        };
    };
}
