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
supervisor (sole bootstrap-DNS resolver, fork × N, waitpid,
            restart w/ backoff, keeps resolution fresh in shm)
 │   memfd + MAP_SHARED page (seqlock) — children map PROT_READ
 └── child i: pinned to physical CPU core i
      ├── SO_REUSEPORT DNS socket (kernel load-balancing)
      ├── own QUIC connection upstream (persistent, keep-alive)
      └── one epoll set: [dns_sock, quic_sock, timerfd, signal pipe]
```

Everything in the request path runs on one thread per core — no channels,
no context switches. Upstream addresses live in a single shared-memory
page written only by the supervisor (a seqlock-guarded memfd, mapped
read-only by children). All bootstrap DNS — startup resolution and TTL
refresh — happens in the supervisor (cold path): children never do
blocking DNS; they pick up refreshed addresses with a single atomic load
per housekeeping pass.

## Latency techniques

- **Prefork shard-per-core** — one child per physical core (sysfs topology),
  `sched_setaffinity`, kernel-side query dispatch via `SO_REUSEPORT`.
- **QUIC 0-RTT** — TLS 1.3 early data with an in-memory session cache;
  reconnects (e.g. after GOAWAY) carry requests in the first flight.
- **Persistent connection** — PING keep-alives + proactive reconnect keep
  handshakes out of the request path.
- **GRO/GSO batching** — `UDP_GRO`/`UDP_SEGMENT` + `recvmmsg`/`sendmmsg`
  via noq-udp.
- **Zero-parse hot path** — DNS wire bytes pass through untouched;
  validation is a 3-word header check.
- **Single timerfd** armed to the nearest deadline (QUIC timers, request
  timeouts, keep-alive, TTL refresh).
- **Fast-fail SERVFAIL** — pending clients get an immediate SERVFAIL (with
  echoed question) on upstream timeout or connection loss, so local
  resolvers retry instantly instead of waiting out their own timeout.
- Optional: `--busy-poll` (SO_BUSY_POLL), `--spin` (epoll pre-sweep),
  `--mlockall`.

## Features

- RFC 8484 GET (base64url, ID zeroed) + POST fallback for queries > 1400 B
- HTTP/3 with hand-rolled QPACK (static table, literal encoder, Huffman decoder)
- QUIC 0-RTT resume (in-memory; see note below)
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
```

## Notes

- **0-RTT replay**: early data can be replayed by a network attacker; DNS
  queries are idempotent, so this is acceptable here. The in-memory ticket
  cache means the first connection after process start is 1-RTT, all
  subsequent reconnects are 0-RTT.
- The HTTP/3 layer implements the client subset needed for DoH: control
  stream + SETTINGS (QPACK dynamic table disabled), one request stream per
  query, GOAWAY handling. No server push, no trailers semantics.
- IPv6 upstreams are preferred (works well on IPv6-only/NAT64 networks).
