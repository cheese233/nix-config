{ config, lib, pkgs, inputs, ... }:

let
  mkPodmanVeth = import ../modules/podman-veth.nix { inherit pkgs lib inputs; };

  veth = mkPodmanVeth {
    name   = "jellyfin";
    bridge = "br-lan";
    mac    = "02:00:00:00:00:03";
    mdns   = true;
  };

  # Official Jellyfin image
  jellyfinImage = pkgs.dockerTools.pullImage {
    imageName = "ghcr.io/jellyfin/jellyfin";
    imageDigest = "sha256:ddf59965ae63fccc66dfe72384495df355c67ea5abd9cc04eb65988f9a6010d1";
    sha256 = "sha256-Vf8xcu7tiIXeoIwmLio7q8Dwih/luD9RsXKkYZhLZqo=";
    finalImageTag = "latest";
  };

  # Folder *inside the container* holding the web client's fallback fonts.
  # Jellyfin keeps this path in <config>/encoding.xml (there is no environment
  # variable or NixOS option for it), so it is selected once under
  # Dashboard -> Playback -> Transcoding -> "Fallback font folder path".
  fallbackFontFolder = "/usr/share/jellyfin/fallback-fonts";

  # WenQuanYi Micro Hei is shipped by nixpkgs as a TrueType collection (.ttc),
  # and Jellyfin only serves .woff/.woff2/.ttf/.otf files from the fallback font
  # folder, so convert the collection to woff2 at build time. See the "Fonts"
  # section of https://jellyfin.org/docs/general/administration/configuration/#fonts
  # woff2 is the format recommended there: the whole folder is preloaded by the
  # web client, so it must stay below 20 MB.
  wqyMicroheiFallbackFonts = pkgs.runCommand "wqy-microhei-webfonts" {
    nativeBuildInputs = [
      (pkgs.python3.withPackages (ps: [ ps.fonttools ps.brotli ]))
    ];
  } ''
    python3 ${./jellyfin/wqy-microhei-to-woff2.py} \
      ${pkgs.wqy_microhei}/share/fonts/truetype/wqy-microhei.ttc $out
  '';
in
{
  systemd.services = veth.services // {
    "${config.virtualisation.oci-containers.containers.jellyfin.serviceName}" = {
      serviceConfig.StateDirectory = "jellyfin";
      after = [ "podman-veth-jellyfin.service" ];
      requires = [ "podman-veth-jellyfin.service" ];
    };
  };

  systemd.tmpfiles.rules = [
    "d /var/lib/jellyfin/config 0755 root root -"
    "d /var/lib/jellyfin/cache 0755 root root -"
  ];

  virtualisation.oci-containers.containers.jellyfin = {
      image = "ghcr.io/jellyfin/jellyfin:latest";
      imageFile = jellyfinImage;
      autoStart = true;

      volumes = [
        "/var/lib/jellyfin/config:/config"
        "/var/lib/jellyfin/cache:/cache"
        "/mnt/HDD:/media"
        # Fallback fonts for the web client's ASS/SSA subtitle renderer ...
        "${wqyMicroheiFallbackFonts}:${fallbackFontFolder}:ro"
        # ... and wqy-microhei as a system font, so CJK glyphs also render when
        # the server burns subtitles into the video or draws cover images.
        "${pkgs.wqy_microhei}/share/fonts/truetype:/usr/share/fonts/truetype/wqy-microhei:ro"
      ];

      environment = {
        TZ = "Asia/Shanghai";
      };

      extraOptions = [
        "--network=${veth.arg}"
        "--hostname=jellyfin"
        "--tmpfs=/tmp"
        "--cap-drop=ALL"
        "--security-opt=no-new-privileges:true"
        "--device=/dev/dri/renderD128:/dev/dri/renderD128"
        "--dns=fdea:d:beef::1"
      ];
    };
}
