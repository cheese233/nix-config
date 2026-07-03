# mdns-publisher

A tiny mDNS hostname responder. Listens on a single network interface
and answers mDNS A/AAAA queries for `<hostname>.local` with the
interface's global unicast IPv4/IPv6 addresses.

It is **not** a Zeroconf stack. There is no service browsing, no
reflection between interfaces, no dbus, no avahi. The whole point is to
make this host resolvable as `<hostname>.local` on the LAN with the
smallest daemon possible.

Pair it with smartdns's `mdns-lookup yes` so any LAN client (even ones
without their own mDNS stack) can resolve this host by querying the
router's DNS server instead of multicasting on their own.

## Build

Pure Go, no CGO. The only external Go modules are
[`github.com/hashicorp/mdns`](https://github.com/hashicorp/mdns) and
[`github.com/miekg/dns`](https://github.com/miekg/dns).

```sh
nix build .#default
```

On the first build, Nix prints the correct `vendorHash` SRI; paste it
into `flake.nix` (or use `nixpkgs.lib.fakeHash` to bootstrap).

## Use as a flake

```nix
# your flake.nix
inputs.mdns-publisher.url = "github:<you>/mdns-publisher";

# host module
modules = [ inputs.mdns-publisher.nixosModules.default ];

# configuration
services.mdns-publisher = {
  enable = true;
  interface = "eth0";          # required
  hostname = null;            # null = use system hostname
  ttl = 120;                  # seconds
  openFirewall = true;        # only affects the legacy iptables firewall
};
```

If you use `networking.nftables.firewall` (or any other firewall stack),
`openFirewall` does nothing — open UDP 5353 manually.

## Standalone binary

```sh
mdns-publisher -iface br-lan -hostname nixos -ttl 120
```

`-iface` is required. `-hostname` (default: system hostname, stripped
of any domain component) and `-ttl` (default: 120) are optional.