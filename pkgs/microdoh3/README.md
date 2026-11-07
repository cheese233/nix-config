# microdoh3

Minimal DNS-over-HTTP/3 (DoH/3) proxy written in Rust — **no async runtime**.

Listens on a local UDP port, forwards DNS queries to a DoH upstream over
HTTP/3 (RFC 9114 + RFC 8484, GET with POST fallback for large messages),
and returns responses over UDP.

Successor to `microdoh` (libcurl/tokio): rewritten from scratch on the
sans-io [`noq`](https://github.com/n0-computer/noq) QUIC stack
(`noq-proto` + `noq-udp`) with a hand-rolled epoll event loop. No tokio,
no libcurl, no hickory.

## Architecture

```
supervisor (sole bootstrap-DNS resolver, fork × N, waitpid, restart w/ backoff;
            resolves the upstream, aggregates worker scores into per-address
            weights)
 │   memfd #1 "resolve" (seqlock): candidate addrs + weights  → children RO
 │   memfd #2 "scores"  (seqlock): one slot per worker        ← children RW
 └── child i: pinned to CPU
      ├── SO_REUSEPORT DNS socket, bound only while it has use for it
      ├── exactly ONE QUIC connection upstream (persistent, keep-alive)
      ├── writes its measured RTT/loss into score slot i
      └── one epoll set: [dns_sock, quic_sock, timerfd, signal pipe]
```

`SO_REUSEPORT` picks the socket by hashing the client's flow, and a resolver
retrying a failed query reuses that flow — so a worker that kept its socket
while it was reconnecting would answer every retry itself, for as long as the
reconnect lasts. Instead a worker binds its DNS socket only while it can
actually open a request stream (`quic.has_conn() && h3_ready`): while it is
reconnecting or still handshaking it is *out* of the kernel's reuseport group,
and the kernel rehashes its share of the traffic onto peers that can serve it.
That socket is also what replies are sent from, so a worker fails what is in
flight (SERVFAIL with the echoed question) before it leaves.

Everything in the request path runs on one thread per core — no channels,
no context switches. All bootstrap DNS — startup resolution and TTL
refresh — happens in the supervisor (cold path): children never do
blocking DNS; they pick up refreshed addresses with a single atomic load
per housekeeping pass.

### Multi-address selection without touching the hot path

The upstream is a *set* of candidate addresses, each with a weight. A worker
owns exactly one connection (so requests never pay for connection switching),
and its address comes from a deterministic weighted slot assignment:
`(idx + 0.5) / workers` of the cumulative weights. Because unbound sends every
query from a random source port, `SO_REUSEPORT` spreads queries evenly across
workers — so **the weights come true as a fleet-wide traffic split**. Six
workers at weights 3:2:1 really do place 3/2/1 workers on the addresses; no
per-query machinery, no eBPF, no socket weighting.

Scores are measured where it matters — dispatch → response, per worker — and
published every 2s. The score is just the inverse of the measured RTT, and it
is computed by the worker, so the supervisor needs no quality model:

* each worker publishes the score of the address it is sitting on (0 = it has
  not completed a request yet, or is not connected);
* the supervisor keeps the best score seen per address and gives every address
  *without* a score a small floor — an unmeasured or currently-dead candidate
  gets tried rather than dropping out;
* one rotating address gets a full extra share each round, so exploration
  cycles through the pool instead of pinning whichever candidate was probed
  first.

Loss needs no term of its own: a lossy path shows up as slow *successful*
requests (retransmits), which is exactly what the RTT measures.

A worker migrates only when the newly assigned address measures materially
better (150% of the current quality), at most once every 3 minutes, staggered
by worker index, and immediately when its current address stops failing or is
dropped from the set. Reconnects are otherwise just reconnects: a DNS TTL
refresh updates the weights and nothing else.

## Latency techniques

- **Prefork shard-per-core** — one child per physical core (sysfs topology),
  `sched_setaffinity`, kernel-side query dispatch via `SO_REUSEPORT`.
- **Measured multi-address selection** — one connection per worker, weighted
  by measured RTT/loss, so a bad anycast address loses its workers instead of
  stalling them (see above).
- **Persistent connection** — PING keep-alives + proactive reconnect keep
  handshakes out of the request path.
- **QUIC 0-RTT** — wired up and used when the endpoint allows it; see the
  0-RTT note below for why that is a server-side property.
- **GRO/GSO batching** — `UDP_GRO`/`UDP_SEGMENT` + `recvmmsg`/`sendmmsg`
  via noq-udp.
- **Zero-parse hot path** — DNS wire bytes pass through untouched;
  validation is a 3-word header check.
- **Single timerfd** armed to the nearest deadline (QUIC timers, request
  timeouts, keep-alive, TTL refresh).
- **Fast-fail SERVFAIL** — pending clients get an immediate SERVFAIL (with
  echoed question) on upstream timeout or connection loss, so local
  resolvers retry instantly instead of waiting out their own timeout. A worker
  that is reconnecting or still handshaking is out of the reuseport group
  entirely, so those retries reach a peer instead of coming back to the same
  socket; a GOAWAY drain is bounded (see the notes), so it cannot become a
  long-lived version of the same trap.
- Optional: `--busy-poll` (SO_BUSY_POLL), `--spin` (epoll pre-sweep),
  `--mlockall`.

## Features

- RFC 8484 GET (base64url, ID zeroed) + POST fallback for queries > 1400 B
- HTTP/3 with hand-rolled QPACK (static table, literal encoder, Huffman decoder)
- QUIC 0-RTT resume (in-memory; only if the server's tickets allow it)
- Weighted upstream selection from a candidate set, with exploration
- EDNS0 padding (RFC 8467), optional
- Bearer auth via `$MICRODOH_TOKEN`, `--token`, or `--token-file`
- Bootstrap DNS with TTL-aware cache (stale-while-revalidate)
- Graceful shutdown; supervisor restarts crashed workers with backoff

## Usage

```bash
# Basic (one worker per physical core)
microdoh3 --upstream https://dns.google/dns-query

# Explicit workers/cores + auth
microdoh3 -l '[::1]:5443' --workers 2 --cpus 0,2 \
  --upstream https://dns.nextdns.io/abc123 --token "$TOKEN"

# Bootstrap via local resolver, low-latency options
microdoh3 --bootstrap-dns 127.0.0.1 --busy-poll --mlockall \
  --upstream https://cloudflare-dns.com/dns-query

# Verbose
microdoh3 --verbose --upstream https://dns.google/dns-query

# Multi-address selection: the candidate set is whatever the upstream hostname
# resolves to, and the weights spread the workers across it by measured quality.
# Widen the pool in DNS (serve more addresses for the name).
microdoh3 --bootstrap-dns 127.0.0.1 --prefer-ipv4 \
  --upstream https://doh.example/dns-query --token "$TOKEN"

# Derive workers from NIC queue XPS maps (one worker per queue CPU).
# Only useful when the workers' sockets actually meet a multi-queue NIC.
microdoh3 --xps-cpus --xps-interface enp2s0f1 --upstream https://dns.google/dns-query
```

## Notes

- **0-RTT is a server-side property, not a client one.** rustls reuses
  resumption tickets from its in-memory cache (256 entries, shared across
  `ClientConfig::clone()`), but it only *offers* early data when the server's
  `NewSessionTicket` carries the `early_data` extension
  (`max_early_data_size > 0`); rustls defaults that to 0 when the extension is
  absent. Verify a candidate endpoint with:

  ```bash
  openssl s_client -quic -connect <host>:443 -alpn h3 \
    -sess_out /tmp/s.pem -ign_eof </dev/null >/dev/null 2>&1
  openssl sess_id -in /tmp/s.pem -text -noout | grep 'Max Early Data'
  # 4294967295 ⇒ advertised; 0 ⇒ not advertised for this hostname
  ```

  On Cloudflare this is **per hostname, not per zone**. The zone-level
  `0rtt` setting ("0-RTT Connection Resumption") and `tls_1_3 = zrt` both have
  to be on, but a hostname attached to a **Pages or Workers custom domain** gets
  no `early_data` at all however those are set — the same zone's plain proxied
  hostnames do get it. A DoH endpoint fronted by a Worker/Pages function
  therefore cannot do 0-RTT on that hostname; serve it from a plain proxied
  record instead (a Snippets rule that matches on path + `Authorization`, not
  hostname, moves with it for free).

  The two outcomes are reported separately (`0-rtt offered` vs `0-rtt
  accepted`, plus `FLAG_ZERO_RTT`/`FLAG_ZERO_RTT_OK` in the score page) so a
  server policy is not mistaken for a client bug.
- **0-RTT replay**: early data can be replayed by a network attacker; DNS
  queries are idempotent, so this is acceptable here.
- **GOAWAY drains are bounded** (1s). While a worker drains it still holds the
  listen socket, because that is how the in-flight replies are sent — so the
  kernel keeps routing new queries to it and each is answered SERVFAIL, whose
  retries hash back to the same socket. A drain is worth about one slow round
  trip and no more: after `GOAWAY_DRAIN_MAX` whatever is still in flight is
  failed, and the worker reconnects and rejoins the group.
- Known limitation: when a server *does* advertise early data and then rejects
  it, noq resets the 0-RTT streams and microdoh3 answers SERVFAIL for those
  queries instead of re-dispatching them over the 1-RTT handshake. unbound
  retries, so nothing is lost, but one RTT is wasted. This is now reachable: a
  ticket is offered for every reconnect until it expires (~48 h on Cloudflare),
  and a reconnect that lands after expiry is answered by the server with 1-RTT
  only.
- The HTTP/3 layer implements the client subset needed for DoH: control
  stream + SETTINGS (QPACK dynamic table disabled), one request stream per
  query, GOAWAY handling. No server push, no trailers semantics.
- The candidate set may span **both address families**. Each worker owns one
  dual-stack UDP socket (`IPV6_V6ONLY` cleared before `bind`), so the weighted
  selection can move it between an IPv4 and an IPv6 upstream, and the two can be
  compared on measured quality like any other pair of addresses. IPv4 peers
  arrive on that socket in their v4-mapped form, which `quic::poll_socket` maps
  back before matching the datagram against its connection. `--prefer-ipv4`
  only changes the listing order (which family the exploration probes first);
  it no longer restricts what the fleet may use.
- `--xps-cpus` aligns workers with NIC TX queues. It only helps when the
  workers' sockets actually traverse those queues: traffic that enters or
  leaves through a TUN, a bridge or a single-queue PPP device has no queue to
  align with, and the queue count can exceed the physical core count.
