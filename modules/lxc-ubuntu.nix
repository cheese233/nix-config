{ config, lib, pkgs, ... }:

# A minimal Ubuntu system container.
#
# NixOS' own `containers.*` can only boot NixOS guests, so a foreign userland
# needs a real container manager: this uses Incus (the LXD successor, shipped
# as `virtualisation.incus`), which gives us cgroup-backed CPU/memory limits and
# a hard rootfs quota without any hand-written cgroup plumbing.
#
#   incus list                                    # state, limits, address
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
  #
  # The address is whatever radvd hands out over RA, as with every other
  # container here.  The MAC is pinned instead of the address (02: = locally
  # administered, matching the 02:00:00:00:00:0{3..7} used by the podman
  # containers), which keeps the SLAAC address stable across restarts and gives
  # the firewall something durable to match on now that the address is not
  # known at evaluation time.
  iface = "eth0";
  mac = "02:00:00:00:00:08";
  dns = "fdea:d:beef::1";

  # cloud-init -> netplan -> systemd-networkd: nothing but "accept RAs, no
  # DHCP", plus the resolver.  `accept-ra` is on by default, but it is written
  # out because it is the only thing that brings the interface up at all.  DNS
  # goes through systemd-resolved, which the image already points
  # /etc/resolv.conf at, so no resolv.conf surgery is needed.
  #
  # Caveat: unbound's access-control only admits sources inside
  # fdea:d:beef::/48, so this depends on radvd advertising the ULA /64 -- that
  # is the address the container sources its queries from.  If it only ever gets
  # a delegated-prefix address, DNS comes back REFUSED.
  networkConfig = ''
    network:
      version: 2
      ethernets:
        ${iface}:
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
        incus launch "${image}" "${name}" --profile default --profile "${profile}" --storage "${pool}" \
          --device "${iface},hwaddr=${mac}"

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
  # directions until it is named here.  The match is on the pinned MAC rather
  # than on an address, because the address now comes from RA and is not known
  # here; `ether` is available because br_netfilter hands the bridged frames to
  # the inet forward hook with their L2 header intact.
  networking.nftables.firewall.rules.lan-to-lan-ubuntu = {
    from = [ "lan" ];
    to = [ "lan" ];
    extraLines = [
      "ether daddr ${mac} accept comment \"ubuntu lxc\""
      "ether saddr ${mac} accept comment \"ubuntu lxc\""
    ];
  };
}
