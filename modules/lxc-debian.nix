{ config, lib, pkgs, ... }:

# A minimal Debian system container.
#
# NixOS' own `containers.*` can only boot NixOS guests, so a Debian userland
# needs a real container manager.  This uses Incus (the LXD successor, shipped
# as `virtualisation.incus`), which gives us cgroup-backed CPU/memory limits and
# a hard rootfs quota without any hand-written cgroup plumbing.
#
#   incus list                  # state + limits
#   incus exec debian -- bash   # shell
#   incus config show debian    # effective config (profile merged in)
#
# The pool, the profile and the instance are created on the first boot after a
# rebuild and then left alone: after that everything is ordinary Incus state in
# /var/lib/incus.  Re-running `systemctl restart incus-debian` is safe.
let
  # ------------------------------------------------------------------ knobs
  name = "debian";
  profile = "debian";
  pool = "default";

  # Debian 13 "trixie", `default` (= minimal) variant, not `cloud`.
  image = "images:debian/13";

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
  # the firewall's `lan` zone and honk's tproxy scope.  br-lan carries no IPv4
  # and has no DHCPv4 server, so the container gets no IPv4 at all: IPv4-only
  # destinations are reached through DNS64 + NAT64 (unbound synthesises
  # 64:ff9b::/96 answers, tayga translates them).
  iface = "eth0";

  # Static rather than left to SLAAC because unbound's access-control only
  # admits sources inside fdea:d:beef::/48 — a SLAAC address could come from the
  # delegated prefix instead and have its DNS REFUSED.  Global IPv6 and the
  # default route still arrive by SLAAC/RA on top of this.
  ipv6 = "fdea:d:beef::50";

  # The host's unbound, which also does DNS64 for the NAT64 path.
  dns = "fdea:d:beef::1";

  # ------------------------------------------------------------------ setup
  # Pushed into the instance and executed there once.  Plain /bin/sh and only
  # tools a minimal Debian ships: the Nix store does not exist inside the
  # container, so this must not be a writeShellScript.
  #
  # The `default` image variant ships systemd-networkd with a DHCP config for
  # eth0, and nothing hands out addresses on br-lan, so that file is replaced
  # with an IPv6-only static one.
  netSetup = pkgs.writeText "debian-net-setup" ''
    #!/bin/sh
    # Idempotent network configuration for the Debian container.
    set -eu

    iface=${iface}
    ipv6=${ipv6}/64
    dns=${dns}

    networkd_conf() {
      echo '[Match]'
      echo "Name=$iface"
      echo
      echo '[Network]'
      echo "Address=$ipv6"
      echo "DNS=$dns"
    }

    ifupdown_conf() {
      echo 'auto lo'
      echo 'iface lo inet loopback'
      echo
      echo "auto $iface"
      echo "iface $iface inet6 static"
      echo "    address $ipv6"
    }

    # 1. Persistent configuration.  `10-eth0.network` sorts first and is the
    #    one networkd applies; `eth0.network` is the image's own file, which is
    #    overwritten too so the DHCP stanza cannot come back.
    if [ -d /etc/systemd/network ]; then
      networkd_conf > /etc/systemd/network/10-eth0.network
      if [ -e /etc/systemd/network/eth0.network ]; then
        networkd_conf > /etc/systemd/network/eth0.network
      fi
    fi

    if [ -e /etc/network/interfaces ]; then
      ifupdown_conf > /etc/network/interfaces
    fi

    # 2. Activate.  Restarting networkd (rather than poking addresses in with
    #    `ip`) keeps networkd's own view of the link consistent with the files.
    if systemctl is-enabled --quiet systemd-networkd 2>/dev/null; then
      systemctl restart systemd-networkd
    elif [ -e /etc/network/interfaces ]; then
      ifup -a 2>/dev/null || true
    else
      ip link set "$iface" up
      ip -6 addr replace "$ipv6" dev "$iface"
    fi

    # 3. DNS.  The image's /etc/resolv.conf is a symlink into systemd-resolved,
    #    which would resolve against a DHCP lease that never arrives.  A plain
    #    file that points straight at the host's unbound is deterministic; the
    #    `DNS=` in the networkd config above is kept for resolved's own use.
    rm -f /etc/resolv.conf
    printf 'nameserver %s\nsearch local\n' "$dns" > /etc/resolv.conf
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
        # The limits for this one instance, kept separate so that a second
        # container does not silently inherit them.
        {
          name = profile;
          description = "${cpuLimit} vCPU, ${memoryLimit} RAM + ${swapLimit} swap";
          config = {
            "limits.cpu" = cpuLimit;
            "limits.memory" = memoryLimit;
            "limits.memory.swap" = swapLimit;
            "boot.autostart" = "true";
          };
        }
      ];
    };
  };

  # --------------------------------------------------------------- instance
  systemd.services.incus-debian = {
    description = "Debian container (incus): create, then push network config";
    wantedBy = [ "multi-user.target" ];
    # honk is in `after` because the very first start has to pull the image
    # from images.linuxcontainers.org, which is a foreign destination here.
    after = [ "incus.service" "incus-preseed.service" "honk.service" "network-online.target" ];
    wants = [ "network-online.target" ];
    requires = [ "incus.service" ];
    path = [ config.virtualisation.incus.clientPackage pkgs.coreutils pkgs.bash ];
    # Re-push the setup script when it changes (e.g. a new static address).
    restartTriggers = [ netSetup ];
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
      fi

      # `launch` returns once the instance is running, but a cold boot may
      # still be bringing systemd up; the networkd restart below needs a live
      # systemd, so wait for the boot to finish rather than for `exec` to work.
      incus start "${name}" 2>/dev/null || true
      i=0
      while ! incus exec "${name}" -- systemctl is-active --quiet multi-user.target >/dev/null 2>&1; do
        i=$((i + 1))
        if [ "$i" -ge 90 ]; then
          echo "${name} did not reach multi-user.target" >&2
          exit 1
        fi
        sleep 2
      done

      incus file push --mode=0755 --create-dirs ${netSetup} "${name}/usr/local/bin/debian-net-setup"
      incus exec "${name}" -- /usr/local/bin/debian-net-setup
      incus exec "${name}" -- ip -br addr show "${iface}"
    '';
  };

  # ---------------------------------------------------------------- firewall
  # NNF drops traffic *within* a zone by default (only mDNS is allowed between
  # LAN hosts), so a container bridged onto br-lan is unreachable in both
  # directions until it is named here.
  networking.nftables.firewall.rules.lan-to-lan-debian = {
    from = [ "lan" ];
    to = [ "lan" ];
    extraLines = [
      "ip6 daddr ${ipv6} accept comment \"debian lxc\""
      "ip6 saddr ${ipv6} accept comment \"debian lxc\""
    ];
  };
}
