{ config, lib, pkgs, ... }:

# A minimal Ubuntu system container.
#
# NixOS' own `containers.*` can only boot NixOS guests, so a foreign userland
# needs a real container manager: this uses Incus (the LXD successor, shipped
# as `virtualisation.incus`), which gives us cgroup-backed CPU/memory limits and
# a hard rootfs quota without any hand-written cgroup plumbing.
#
#   incus list                                    # state + limits
#   incus exec ubuntu -- bash                     # shell
#   incus exec ubuntu -- cloud-init status --long # what cloud-init did
#   incus config show ubuntu                      # effective config
#
# Networking needs no host-side scripting: the `cloud` image variant ships
# cloud-init, and `cloud-init.network-config` hands it a netplan document, which
# it renders to /etc/netplan/50-cloud-init.yaml on the first boot and applies.
# The key therefore lives in the *profile* -- it has to be in place before
# `incus launch` starts the instance, because cloud-init runs exactly once.
# Consequence: editing the document below needs
#   incus exec ubuntu -- cloud-init clean && incus restart ubuntu
# to take effect (a plain reboot re-runs nothing).
let
  # ------------------------------------------------------------------ knobs
  name = "ubuntu";
  profile = "ubuntu";
  pool = "default";

  # Ubuntu 26.04 LTS "resolute".  The `/cloud` suffix is the variant with
  # cloud-init; the `default` variant is ~30 MiB smaller but has neither
  # cloud-init nor a way to inject netplan without touching the rootfs.
  image = "images:ubuntu/26.04/cloud";

  cpuLimit = "2";
  memoryLimit = "5GiB";
  # Incus semantics: this is swap *in addition to* limits.memory.
  swapLimit = "4GiB";

  # Rootfs quota.  The pool itself is a sparse loop file, so this is also the
  # hard ceiling on how much the container can ever write.
  rootSize = "32GiB";
  poolSize = "32GiB"; # -> /var/lib/incus/disks/default.img

  # ------------------------------------------------------------------ network
  # Bridged onto br-lan like every other container on this host, so it lands in
  # the firewall's `lan` zone and honk's tproxy scope.  br-lan carries no IPv4,
  # so the instance gets none either: IPv4-only destinations are reached
  # through DNS64 + NAT64 (unbound synthesises 64:ff9b::/96, tayga translates).
  iface = "eth0";
  ipv6 = "fdea:d:beef::50";
  dns = "fdea:d:beef::1";

  # cloud-init -> netplan -> systemd-networkd.  The ULA address is static rather
  # than SLAAC-only because unbound's access-control only admits sources inside
  # fdea:d:beef::/48; global IPv6 and the default route still arrive by RA on
  # top of it.  DNS goes through systemd-resolved, which the image already
  # points /etc/resolv.conf at, so no resolv.conf surgery is needed.
  networkConfig = ''
    network:
      version: 2
      ethernets:
        ${iface}:
          addresses: [ ${ipv6}/64 ]
          dhcp4: false
          dhcp6: false
          accept-ra: true
          nameservers:
            addresses: [ ${dns} ]
            search: [ local ]
  '';
in
{
  virtualisation.incus = {
    enable = true;

    # Applied by `incus admin init --preseed` (idempotent: existing entities are
    # updated, nothing is ever removed).
    preseed = {
      storage_pools = [
        {
          name = pool;
          driver = "btrfs";
          # No `source`: Incus creates a sparse loop file of this size at
          # /var/lib/incus/disks/<pool>.img and puts btrfs on it.  Note this
          # must be a raw file — Incus formats the loop device itself, so a
          # qcow2 image would not work here.
          config = { size = poolSize; };
        }
      ];

      profiles = [
        # Container shape shared by anything launched on this host.
        {
          name = "default";
          description = "root on the btrfs loop pool, bridged onto br-lan";
          devices = {
            root = {
              type = "disk";
              path = "/";
              pool = pool;
              size = rootSize;
            };
            eth0 = {
              type = "nic";
              name = iface;
              nictype = "bridged";
              parent = "br-lan";
            };
          };
        }
        # This instance's limits and its first-boot network configuration, kept
        # out of `default` so a second container does not inherit either.
        {
          name = profile;
          description = "${cpuLimit} vCPU, ${memoryLimit} RAM + ${swapLimit} swap";
          config = {
            "limits.cpu" = cpuLimit;
            "limits.memory" = memoryLimit;
            "limits.memory.swap" = swapLimit;
            "boot.autostart" = "true";
            "cloud-init.network-config" = networkConfig;
          };
        }
      ];
    };
  };

  # --------------------------------------------------------------- instance
  # Preseed cannot create instances, so this unit does it once.  After that the
  # instance is ordinary Incus state and the unit is a no-op.
  systemd.services.incus-ubuntu = {
    description = "Ubuntu container (incus): create if missing";
    wantedBy = [ "multi-user.target" ];
    # honk is in `after` because the very first start has to pull the image
    # from images.linuxcontainers.org, which is a foreign destination here.
    after = [ "incus.service" "incus-preseed.service" "honk.service" "network-online.target" ];
    wants = [ "network-online.target" ];
    requires = [ "incus.service" ];
    path = [ config.virtualisation.incus.clientPackage ];
    serviceConfig = {
      Type = "oneshot";
      RemainAfterExit = true;
      # First boot downloads and unpacks the image.
      TimeoutStartSec = "900";
    };
    script = ''
      set -eu

      if ! incus info "${name}" >/dev/null 2>&1; then
        echo "creating ${name} from ${image}"
        incus launch "${image}" "${name}" --profile default --profile "${profile}" --storage "${pool}"

        # Informational: cloud-init renders and applies the netplan document
        # during this same boot, so the rebuild log says whether the address
        # actually landed.  Neither command may fail the unit.
        incus exec "${name}" -- cloud-init status --wait || true
        incus exec "${name}" -- ip -br addr show "${iface}" || true
      fi
    '';
  };

  # ---------------------------------------------------------------- firewall
  # NNF drops traffic *within* a zone by default (only mDNS is allowed between
  # LAN hosts), so a container bridged onto br-lan is unreachable in both
  # directions until it is named here.
  networking.nftables.firewall.rules.lan-to-lan-ubuntu = {
    from = [ "lan" ];
    to = [ "lan" ];
    extraLines = [
      "ip6 daddr ${ipv6} accept comment \"ubuntu lxc\""
      "ip6 saddr ${ipv6} accept comment \"ubuntu lxc\""
    ];
  };
}
