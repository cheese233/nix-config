{
  description = "honk, an eBPF transparent proxy engine";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";

  outputs = { self, nixpkgs }:
    let
      supportedSystems = [ "x86_64-linux" ];
      forAllSystems = nixpkgs.lib.genAttrs supportedSystems;
      version = "0.0.1-alpha";
      rev = "d66c702e871c9cde78ceec7c374129ac48c239b3";
      sourceHash = "sha256-3d9Vip1fPzr5m3uqx9/EySYK3/lU0Txjyxvvh1k9SMM=";
    in
    {
      packages = forAllSystems (system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          bpf-linker = pkgs.stdenv.mkDerivation {
            pname = "bpf-linker";
            version = "0.10.3";
            src = pkgs.fetchurl {
              url = "https://github.com/aya-rs/bpf-linker/releases/download/v0.10.3/bpf-linker-x86_64-unknown-linux-musl.tar.gz";
              hash = "sha256-D6RkXS37tcr+YjGwqp+tTxQwvQhx471zGegtgnv2Jiw=";
            };
            dontConfigure = true;
            dontBuild = true;
            dontUnpack = true;
            installPhase = ''
              tar -xzf "$src"
              install -Dm755 bpf-linker $out/bin/bpf-linker
            '';
          };
          src = pkgs.fetchFromGitHub {
            owner = "daeuniverse";
            repo = "honk";
            inherit rev;
            hash = sourceHash;
          };

          honk-ebpf = pkgs.rustPlatform.buildRustPackage {
            pname = "honk-ebpf";
            inherit version src;
            cargoRoot = "crates/honk-ebpf";
            cargoLock.lockFile = "${src}/crates/honk-ebpf/Cargo.lock";

            nativeBuildInputs = [ bpf-linker ];
            env = {
              RUSTC_BOOTSTRAP = "1";
              RUST_SRC_PATH = "${pkgs.rustPlatform.rustcSrc}";
            };

            postPatch = ''
              substituteInPlace crates/honk-ebpf/.cargo/config.toml \
                --replace-fail /root/.cargo/bin/bpf-linker-wrapper \
                ${bpf-linker}/bin/bpf-linker
              substituteInPlace crates/honk-ebpf/.cargo/config.toml \
                --replace-fail 'bpf-stack-size=4096' 'bpf-stack-size=512'
            '';

            buildPhase = ''
              runHook preBuild
              vendor_dir=$(find "$NIX_BUILD_TOP" -type d -name cargo-vendor-dir -print -quit)
              for crate in ${pkgs.rustPlatform.rustVendorSrc}/*; do
                crate_name=$(basename "$crate")
                if [ ! -e "$vendor_dir/$crate_name" ]; then
                  ln -s "$crate" "$vendor_dir/$crate_name"
                fi
              done
              rustc_sysroot=$(rustc --print sysroot)
              build_sysroot="$TMPDIR/honk-rust-sysroot"
              mkdir -p "$build_sysroot/lib/rustlib/src"
              cp -rs "$rustc_sysroot/lib/rustlib/." "$build_sysroot/lib/rustlib/"
              ln -s ${pkgs.rustPlatform.rustcSrc} "$build_sysroot/lib/rustlib/src/rust"
              real_rustc=$(command -v rustc)
              cat > "$TMPDIR/honk-rustc" <<EOF
              #!/bin/sh
              exec "$real_rustc" --sysroot "$build_sysroot" "\$@"
              EOF
              chmod +x "$TMPDIR/honk-rustc"
              export RUSTC="$TMPDIR/honk-rustc"
              cd crates/honk-ebpf
              cargo build --release --offline -Zbuild-std=core --target bpfel-unknown-none
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
          honk = pkgs.rustPlatform.buildRustPackage {
            pname = "honk";
            inherit version src;
            cargoLock = {
              lockFile = "${src}/Cargo.lock";
              outputHashes."boring-sys-5.1.0" = "sha256-Tvf9qpUC6IO3ikkHO7BG0lp+ZGtu4DiS0HKKFdmjwjY=";
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
              homepage = "https://github.com/daeuniverse/honk";
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
