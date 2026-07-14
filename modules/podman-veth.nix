{ pkgs }:

{ name, bridge, mac, raTimeout ? 120 }:

let
  hostIf = "veth-${name}-h";
  nsIf   = "veth-${name}-c";
in
{
  arg = "ns:/run/netns/${name}";

  services."podman-veth-${name}" = {
    description = "veth pair + netns for podman container ${name} on ${bridge}";
    after = [ "network.target" ];
    before = [ "podman-${name}.service" ];
    requiredBy = [ "podman-${name}.service" ];
    serviceConfig = {
      Type = "oneshot";
      RemainAfterExit = true;
    };
    path = with pkgs; [ iproute2 coreutils procps bash ];
    script = ''
      set -e

      install -d -m 755 /run/netns

      # Clean up leftover veth host side from a previous failed run.
      ip link del ${hostIf} 2>/dev/null || true

      # 1. Create persistent netns
      ip netns add ${name} 2>/dev/null || true

      # 2. Create veth pair
      ip link add ${hostIf} type veth peer name ${nsIf}

      # 3. Move ns side into netns, rename, set MAC
      ip link set ${nsIf} netns ${name}
      ip netns exec ${name} ip link set ${nsIf} name eth0
      ip netns exec ${name} ip link set eth0 address ${mac}

      # 4. Start netlink monitor BEFORE bringing the interface up,
      #    so we don't miss the SLAAC event.
      slaacFile=/tmp/slaac-${name}
      ip netns exec ${name} \
        ip -6 monitor addr dev eth0 > "$slaacFile" 2>/dev/null &
      monitorPid=$!

      # 5. Bring the interface up and attach host side to bridge.
      ip netns exec ${name} ip link set eth0 up
      ip link set ${hostIf} master ${bridge} up

      # 6. Accept RAs (SLAAC + RDNSS)
      ip netns exec ${name} sysctl -w net.ipv6.conf.all.accept_ra=2

      # 7. Wait for a global IPv6 address to appear.
      #    tail -f uses inotify (no polling); grep -m1 closes the pipe.
      echo "Waiting up to ${toString raTimeout}s for SLAAC address on eth0..."
      timeout ${toString raTimeout} sh -c '
        tail -n+1 -f "$1" | grep -m1 "scope global"
      ' _ "$slaacFile"

      # 8. Tear down the monitor.
      kill "$monitorPid" 2>/dev/null || true
      wait "$monitorPid" 2>/dev/null || true
      rm -f "$slaacFile" || true

      echo "Network ready for container ${name}."
    '';
    preStop = ''
      ${pkgs.iproute2}/bin/ip netns del ${name} 2>/dev/null || true
      ${pkgs.iproute2}/bin/ip link del ${hostIf} 2>/dev/null || true
    '';
  };
}
