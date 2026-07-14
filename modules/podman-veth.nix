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
    path = with pkgs; [ iproute2 coreutils ];
    script = ''
      set -e

      install -d -m 755 /run/netns

      # 1. Create persistent netns
      ip netns add ${name} 2>/dev/null || true

      # 2. Create veth pair
      ip link add ${hostIf} type veth peer name ${nsIf}

      # 3. Host side → bridge
      ip link set ${hostIf} master ${bridge} up

      # 4. Netns side → rename to eth0, set MAC, bring up
      ip link set ${nsIf} netns ${name}
      ip netns exec ${name} ip link set ${nsIf} name eth0
      ip netns exec ${name} ip link set eth0 address ${mac} up

      # 5. Accept RAs (SLAAC + RDNSS)
      ip netns exec ${name} sysctl -w net.ipv6.conf.all.accept_ra=2

      # ── Wait for SLAAC to deliver a global IPv6 address ──
      # ip monitor subscribes to netlink events; grep -m1 exits after
      # the first match, closing the pipe and causing ip monitor to
      # receive SIGPIPE → clean exit.  No polling, no sleep loop.
      #
      # If the RA never arrives within raTimeout, we exit non-zero so
      # systemd retries the whole unit.  No fallback routes.
      echo "Waiting up to ${toString raTimeout}s for SLAAC address on eth0..."
      timeout ${toString raTimeout} sh -c '
        ip netns exec ${name} ip -6 monitor address dev eth0 2>/dev/null |
        grep -m1 "scope global"
      '

      echo "Network ready for container ${name}."
    '';
    preStop = ''
      ${pkgs.iproute2}/bin/ip netns del ${name} 2>/dev/null || true
      ${pkgs.iproute2}/bin/ip link del ${hostIf} 2>/dev/null || true
    '';
  };
}
