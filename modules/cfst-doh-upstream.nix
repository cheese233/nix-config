# Cloudflare 优选 IP (`优选 IP`) for the microdoh3 DoH upstream.
#
# microdoh3 has no address option: its candidate set is *exactly* what the DoH
# hostname resolves to through `--bootstrap-dns` (unbound on 127.0.0.1), and it
# re-resolves on the answer's TTL. So the way to make it dial 优选 IPs is not a
# microdoh3 knob — it is to change what unbound answers for that name.
#
# One run is three stages, deliberately narrow:
#
#   1. ICMP prefilter — one random address per /24, `samplePerCidr` draws per
#      published CIDR (~120 addresses), so the pool is sampled without sweeping
#      anyone's address space over TCP.
#   2. TCP rank — CFST is pointed at the ~16 lowest-RTT responders only, so it
#      measures a shortlist instead of Cloudflare's whole footprint.
#   3. DoH probe — each ranked address must answer a real DoH GET for this
#      endpoint; whatever survives is published as unbound `local-data`.
#
# No unbound restart, no cache flush, no seed list in microdoh3.
#
# How it composes with the rest of the resolver:
#
#   * unbound answers local zones in `daemon/worker.c` — before the module
#     stack (hence before DNS64) and before any forward-zone lookup for the
#     same name. The pinned addresses therefore reach microdoh3 verbatim, and
#     no DNS64-synthesised twin of each address shows up in the pool.
#   * a name that has local data is answered authoritatively for *every* type:
#     a transparent local zone only falls through when it has no data for the
#     name at all. With `ipv6 = false` (default) only A records are published,
#     so AAAA is an authoritative NODATA and the candidate pool is exactly the
#     selected IPv4 set. `ipv6 = true` additionally tests Cloudflare's IPv6
#     ranges and pins native AAAA, which is what brings back the two-family
#     comparison microdoh3's scoring is designed for.
#   * the `$DOMAIN` forward-zone in the host config stays as the fallback for
#     the window before the first successful run (and if this service never
#     succeeds). Local data always wins over it once published.
#   * CFST and the probe must egress *directly*: honk routes an unlisted process
#     to the subscription, and then both would measure the proxy path instead of
#     the one microdoh3 uses. The host config therefore carries
#     `pname(cfst) -> direct(must)` and `pname(dohprobe) -> direct(must)` — the
#     `(must)` makes the decision terminal in the fast path; a plain `direct` is
#     punted to userspace, which resolves the process name asynchronously and
#     misses the sub-100ms sockets a scan is made of. The probe is a copy of curl
#     under its own name because honk matches the process name, not the argv[0] a
#     wrapper could change.
#
# Failure behaviour: a round that yields nothing, or nothing that passes the DoH
# probe, leaves the published file untouched, so the DoH keeps working off the
# previous selection (or off the forward-zone before the first one).
{ config, lib, pkgs, inputs, ... }:

let
  cfg = config.services.cfstDohUpstream;

  # Cloudflare's published edge ranges — the same pinned file inputs
  # modules/microvm-traefik.nix uses, refreshed with
  # `nix flake update cloudflareIpsV4 cloudflareIpsV6`. CFST samples one random
  # address per /24 from whatever file it is given, so this is ~4k candidates.
  ipv4Ranges = inputs.cloudflareIpsV4;

  # The IPv6 side uses CFST's own curated list instead: Cloudflare's published
  # IPv6 ranges are /32s, and a random address inside one is usually not an
  # edge, so sampling them yields nothing. `ipv6.txt` is the prefixes that were
  # actually observed answering.
  ipv6Ranges = "${cfg.package}/share/cfst/ipv6.txt";

  # `--resolve` pins the address while SNI/Host stay the DoH hostname, i.e. the
  # probe exercises exactly what microdoh3 would dial. This is a *copy* of curl
  # rather than a symlink or a wrapper so that the process name is `dohprobe`
  # and honk's direct rule matches it.
  probeCurl = pkgs.runCommandLocal "cfst-doh-probe" { } ''
    install -Dm755 ${lib.getExe pkgs.curl} $out/bin/dohprobe
  '';

  unbound = config.services.unbound.package;

  # `example.com. A IN` (29 bytes, ID 0, RD=1) base64url-encoded per RFC 8484.
  # The answer is irrelevant — a 200 proves this address serves this endpoint.
  dnsQuery = "AAABAAABAAAAAAAAB2V4YW1wbGUDY29tAAABAAE";

  ipv6Block = lib.optionalString cfg.ipv6 ''
    publish ipv6 AAAA ${ipv6Ranges} || true
  '';

  script = pkgs.writeShellScript "cfst-doh-upstream" ''
    set -eu

    # Same agenix env file microdoh3's ExecStart sources: DOMAIN, URI_PATH, TOKEN.
    set -a
    . ${cfg.envFile}
    set +a

    DOMAIN=''${DOMAIN%.}
    URI_PATH=''${URI_PATH:-/dns-query}
    TOKEN=''${TOKEN:-}

    WORK=${cfg.stateDir}
    FINAL=${cfg.includeFile}
    PICK=$WORK/published.conf
    STAGE=$WORK/stage
    mkdir -p "$STAGE"

    log() { printf 'cfst-doh-upstream: %s\n' "$*" >&2; }

    case "$URI_PATH" in
      *\?*) DOH_URL="https://$DOMAIN:${toString cfg.port}$URI_PATH&dns=${dnsQuery}" ;;
      *)    DOH_URL="https://$DOMAIN:${toString cfg.port}$URI_PATH?dns=${dnsQuery}" ;;
    esac

    # ── 1. ICMP prefilter ────────────────────────────────────────────────
    # One random address per /24, `samplePerCidr` draws per published CIDR:
    # ~100 addresses instead of the whole Cloudflare footprint. ICMP is cheap
    # enough that this reads as a monitoring sweep rather than a port scan, and
    # nothing here touches TCP.
    ${pkgs.gawk}/bin/awk -v K=${toString cfg.samplePerCidr} '
      function u32(s, p) { split(s, p, "."); return ((p[1] * 256 + p[2]) * 256 + p[3]) * 256 + p[4] }
      BEGIN { srand() }
      /^[0-9]/ {
        split($1, c, "/"); pre = c[2] + 0
        base = u32(c[1]); n24 = 2 ^ (24 - pre)
        for (i = 0; i < K; i++) {
          idx = int(rand() * n24)
          ip = (base / 256 + idx) * 256 + int(rand() * 254) + 1
          printf "%d.%d.%d.%d\n", int(ip / 16777216) % 256, int(ip / 65536) % 256, \
            int(ip / 256) % 256, ip % 256
        }
      }' ${ipv4Ranges} | sort -u >"$STAGE/candidates.txt"

    # ── 2. Ping them, keep the responders ranked by average RTT ──────────
    xargs -P ${toString cfg.pingParallel} -I{} \
      bash -c 'r=$(ping -c ${toString cfg.pingCount} -i 0.3 -W 1 -q {} 2>/dev/null | tail -1); [ -n "$r" ] && echo "{} $r"' \
      <"$STAGE/candidates.txt" >"$STAGE/pings.txt" || true
    ${pkgs.gawk}/bin/awk '
      { for (i = 1; i <= NF; i++) if ($i == "=") { split($(i + 1), a, "/"); print a[2], $1 } }' \
      "$STAGE/pings.txt" | sort -n | head -n ${toString cfg.candidates} | awk '{print $2}' \
      >"$STAGE/short.txt"

    log "sampled $(wc -l <"$STAGE/candidates.txt") address(es): $(wc -l <"$STAGE/pings.txt") answered ICMP, TCP-probing the best $(wc -l <"$STAGE/short.txt")"
    if [ ! -s "$STAGE/short.txt" ]; then
      log "no ICMP responder this round; keeping the current selection"
      exit 0
    fi

    # ── 3. Rank that handful by TCP handshake ────────────────────────────
    # CFST is still the tool, but on a ~16-address file instead of Cloudflare's
    # published ranges: `-dd` sorts by average latency (no download test), and
    # one random sample per /24 keeps roughly one per edge population.
    pick() {
      fam=$1
      list=$2
      out=$WORK/result-$fam.csv
      rm -f "$out"
      ${cfg.package}/bin/cfst -f "$list" -tp ${toString cfg.port} -dd -p 0 \
        -o "$out" ${lib.escapeShellArgs cfg.cfstArgs} \
        >"$WORK/cfst-$fam.log" 2>&1 || true
      [ -s "$out" ] || return 1
      # `tr -d ' \r'`, not `tr -d '[:space:]'`: the latter also eats the
      # newlines and returns one giant concatenated "address".
      tail -n +2 "$out" | cut -d, -f1 | tr -d ' \r' \
        | head -n ${toString cfg.candidates}
    }

    # A candidate is published only if it answers a real DoH GET with this
    # hostname and token: TCP reachability (all CFST measures) does not prove
    # that the address serves this zone over TLS.
    probe() {
      ip=$1
      case "$ip" in
        *:*) host="[$ip]" ;;
        *)   host=$ip ;;
      esac
      ${probeCurl}/bin/dohprobe --fail --silent --show-error --max-time 5 \
        --resolve "$DOMAIN:${toString cfg.port}:$host" \
        --header "accept: application/dns-message" \
        --header "authorization: Bearer $TOKEN" \
        --output /dev/null "$DOH_URL"
    }

    publish() {
      fam=$1
      rrtype=$2
      list=$3
      n=0
      for ip in $(pick "$fam" "$list"); do
        [ -n "$ip" ] || continue
        if probe "$ip"; then
          printf '    local-data: "%s. ${toString cfg.ttl} IN %s %s"\n' \
            "$DOMAIN" "$rrtype" "$ip" >>"$PICK"
          log "publish $rrtype $ip"
          n=$((n + 1))
          [ "$n" -ge ${toString cfg.count} ] && break
        else
          log "reject $rrtype $ip (no DoH answer through it)"
        fi
      done
      [ "$n" -gt 0 ]
    }

    # `local-data` is a *server* option: in an include file it has to sit inside
    # a `server:` clause, otherwise unbound rejects the line with a bare
    # "syntax error" (and the next boot would not start at all).
    {
      printf '# generated by cfst-doh-upstream — do not edit\n'
      printf 'server:\n'
    } >"$PICK"
    publish ipv4 A "$STAGE/short.txt" || true
    # IPv6 still goes through CFST's curated prefix list in full; it is off by
    # default and its ranges are curated enough that sampling them blindly does
    # not work (see ipv6Ranges).
    ${ipv6Block}

    published=$(grep -c 'local-data:' "$PICK" || true)
    if [ "$published" -eq 0 ]; then
      log "no address passed this round; keeping the current selection"
      exit 0
    fi

    if cmp -s "$PICK" "$FINAL"; then
      log "selection unchanged"
      exit 0
    fi

    # Publish atomically (same filesystem), then prove the whole config still
    # parses before unbound ever sees it — a broken local-data line would
    # otherwise only surface at the next unbound start.
    cp -f "$FINAL" "$FINAL.prev" 2>/dev/null || true
    cp -f "$PICK" "$FINAL.new"
    mv -f "$FINAL.new" "$FINAL"

    if ! ${unbound}/bin/unbound-checkconf /etc/unbound/unbound.conf >/dev/null; then
      log "unbound-checkconf rejected the new local-data; rolling back"
      if [ -e "$FINAL.prev" ]; then mv -f "$FINAL.prev" "$FINAL"; else : >"$FINAL"; fi
      exit 1
    fi

    # reload_keep_cache: re-read the config (so the include is re-parsed) without
    # flushing the 512 MiB message cache. The plain reload is only a fallback for
    # an unbound too old to know the command.
    ${unbound}/bin/unbound-control -c /etc/unbound/unbound.conf reload_keep_cache \
      || ${unbound}/bin/unbound-control -c /etc/unbound/unbound.conf reload

    log "published $published address(es); unbound reloaded"
  '';
in
{
  options.services.cfstDohUpstream = {
    enable = lib.mkEnableOption ''
      CloudflareSpeedTest-selected 优选 IPs for the microdoh3 DoH upstream,
      published as unbound local-data on a timer
    '';

    package = lib.mkOption {
      type = lib.types.package;
      default = pkgs.callPackage ../pkgs/cloudflarespeedtest/package.nix { };
      defaultText = lib.literalExpression "pkgs.callPackage ../pkgs/cloudflarespeedtest/package.nix { }";
      description = ''
        CloudflareSpeedTest package; it must provide `bin/cfst` and the
        `share/cfst/ipv6.txt` range list.
      '';
    };

    envFile = lib.mkOption {
      type = lib.types.str;
      default = "/run/agenix/doh-env";
      description = ''
        Environment file providing the DoH endpoint's `DOMAIN`, `URI_PATH` and
        `TOKEN` — the same file microdoh3's ExecStart sources.
      '';
    };

    ipv6 = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = ''
        Also test Cloudflare's IPv6 ranges and publish native AAAA records.

        Off by default because XIU2's 优选 workflow is IPv4-centric and the
        native IPv6 path to the edge measured 30-60% loss on this line. On:
        microdoh3's weighted selection gets both families back and can compare
        the direct paths, as its scoring is designed to. Off: AAAA for the name
        is an authoritative NODATA, so the pool is the pinned IPv4 set only.
      '';
    };

    count = lib.mkOption {
      type = lib.types.ints.positive;
      default = 4;
      description = ''
        Addresses published per family. More than one so microdoh3's weighted
        slot assignment still has something to choose between; six workers
        spread well over four addresses.
      '';
    };

    samplePerCidr = lib.mkOption {
      type = lib.types.ints.positive;
      default = 8;
      description = ''
        ICMP candidates drawn per published Cloudflare CIDR (one random
        address per /24). With the 15 ranges in `ips-v4` that is ~120 addresses
        per run — a monitoring sweep, not a scan.
      '';
    };

    pingCount = lib.mkOption {
      type = lib.types.ints.positive;
      default = 3;
      description = "ICMP echo requests per candidate.";
    };

    pingParallel = lib.mkOption {
      type = lib.types.ints.positive;
      default = 32;
      description = "Concurrent ICMP probes (the whole sweep finishes in seconds).";
    };

    candidates = lib.mkOption {
      type = lib.types.ints.positive;
      default = 16;
      description = ''
        How many of the lowest-RTT ICMP responders get a TCP handshake, i.e.
        the size of the `-f` file CFST ranks. Needs headroom: on this line most
        of the fastest addresses fail one layer up (TLS reset, or a handshake
        that then times out), so a top-16 window still yields only a few usable
        addresses.
      '';
    };

    cfstArgs = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [
        "-n" "8"
        "-t" "2"
      ];
      description = ''
        Extra CloudflareSpeedTest arguments. Defaults: 8 latency-test threads
        and 2 probes per address, which is plenty for a ~16-address shortlist.
        `-f`, `-tp`, `-dd`, `-p` and `-o` are supplied by the service and must
        not be set here.
      '';
    };

    port = lib.mkOption {
      type = lib.types.port;
      default = 443;
      description = "Port tested and probed; also the DoH TLS port.";
    };

    ttl = lib.mkOption {
      type = lib.types.ints.positive;
      default = 300;
      description = ''
        TTL of the published records. microdoh3 clamps to a 30 s floor and
        re-resolves when it expires, so this is the upper bound on how long a
        fresh selection takes to reach the workers.
      '';
    };

    enableTimer = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = ''
        Install the periodic run.

        One run is now ~120 ICMP echoes plus ~16 TCP handshakes over a few
        seconds, so a daily tick is safe. Keep it off until the pinning is
        actually wanted: while off, the only way to run it is
        `systemctl start cfst-doh-upstream`.
      '';
    };

    calendar = lib.mkOption {
      type = lib.types.str;
      default = "*-*-* 04:23:00";
      description = ''
        systemd `OnCalendar=` for the periodic run. The selection is measured
        from this line's own path, so once a day is enough; microdoh3 re-scores
        whatever it is given anyway.
      '';
    };

    stateDir = lib.mkOption {
      type = lib.types.str;
      default = "/var/lib/cfst-doh";
      description = ''
        Where CFST's per-family result CSVs and logs are kept. Must be under
        `/var/lib`, so that `StateDirectory` can be derived from it.
      '';
    };

    includeFile = lib.mkOption {
      type = lib.types.str;
      default = "/var/lib/unbound/cfst-upstream.conf";
      description = ''
        Generated unbound include holding the `local-data:` records. Must live
        in unbound's state directory, which is the only place the service is
        allowed to write besides `stateDir`.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    # unbound must be able to read the include on *every* start, including the
    # very first one before any selection exists — an empty include is valid.
    systemd.services.unbound.preStart = lib.mkAfter ''
      touch ${cfg.includeFile}
    '';

    # The host config keeps `include` a list so this merges instead of replacing.
    services.unbound.settings.include = lib.mkAfter [ cfg.includeFile ];

    systemd.services.cfst-doh-upstream = {
      description = "Cloudflare 优选 IP for the DoH upstream (CloudflareSpeedTest → unbound local-data)";
      # Ordered like the resolver it feeds. Not `network-online.target`: a run
      # that starts before ppp0 is up just measures nothing and leaves the
      # previous selection alone, so there is nothing to block on.
      wants = [ "unbound.service" ];
      after = [
        "network.target"
        "unbound.service"
        "agenix.service"
      ];

      # The script is a plain writeShellScript (no PATH of its own): give it
      # exactly what it shells out to — coreutils, diffutils for `cmp`, gawk for
      # the candidate/ranking arithmetic, iputils for `ping`, and bash because
      # `xargs` re-executes the ping one-liner by name.
      # This is a unit-level option, not a [Service] directive.
      path = [
        pkgs.bash
        pkgs.coreutils
        pkgs.diffutils
        pkgs.gawk
        pkgs.iputils
      ];

      serviceConfig = {
        Type = "oneshot";
        ExecStart = script;
        StateDirectory = lib.removePrefix "/var/lib/" cfg.stateDir;
        StandardInput = "null";
        TimeoutStartSec = "15min";

        # Same shape as the resolver it feeds. Write access is limited to the
        # two directories the service actually touches.
        ProtectSystem = "strict";
        ProtectHome = true;
        PrivateTmp = true;
        PrivateDevices = true;
        NoNewPrivileges = true;
        # AF_NETLINK is not optional: `unbound-checkconf` resolves the
        # `interface:` names through getifaddrs(3), which is a netlink socket.
        # Without it the gate fails with "Address family not supported by
        # protocol" and every round is rolled back.
        RestrictAddressFamilies = [
          "AF_INET"
          "AF_INET6"
          "AF_NETLINK"
          "AF_UNIX"
        ];
        ReadWritePaths = [ (builtins.dirOf cfg.includeFile) ];
      };
    };

    systemd.timers.cfst-doh-upstream = lib.mkIf cfg.enableTimer {
      description = "Periodic CloudflareSpeedTest run for the DoH upstream";
      wantedBy = [ "timers.target" ];
      timerConfig = {
        Unit = "cfst-doh-upstream.service";
        # Exactly one run per calendar slot: no `OnBootSec`, and `Persistent`
        # off, so a reboot or a missed slot cannot add a second sweep that day.
        # (XIU2's own advice is to re-measure when the line changes, not on a
        # schedule; the day is only there so a stale set cannot live forever.)
        OnCalendar = cfg.calendar;
        RandomizedDelaySec = "30min";
        AccuracySec = "1min";
        Persistent = false;
      };
    };
  };
}
