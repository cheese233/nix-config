# microdoh

Minimal DNS-over-HTTPS (DoH) proxy written in Rust, using libcurl + tokio.

Listens on a local UDP port, forwards DNS queries to a DoH upstream via
[RFC 8484](https://datatracker.ietf.org/doc/html/rfc8484) GET (with POST
fallback for large messages), and returns the response over UDP.

## Features

- **RFC 8484 compliant** — GET with base64url encoding, POST fallback for
  queries > 1400 bytes
- **HTTP/3 (QUIC) + 0-RTT** — via libcurl with ngtcp2 backend; graceful
  fallback to HTTP/2
- **EDNS0 padding** (RFC 8467) — optional, pads queries to 128-byte blocks
- **Bearer auth** — reads token from `$MICRODOH_TOKEN` env var, `--token`,
  or `--token-file`
- **Bootstrap DNS** — resolves DoH upstream hostname via a configurable
  bootstrap server (avoids circular dependency)
- **TTL-aware refresh** — caches bootstrap DNS results and refreshes on expiry
- **Graceful shutdown** — SIGINT/SIGTERM drains in-flight requests

## Usage

```bash
# Basic
microdoh --upstream https://dns.google/dns-query

# With auth token
microdoh --upstream https://dns.nextdns.io/abc123 --token "$TOKEN"

# Custom listen address and bootstrap DNS
microdoh -l '[::1]:5443' --bootstrap-dns 8.8.8.8 --upstream https://cloudflare-dns.com/dns-query

# Verbose mode (debug logs + libcurl protocol dump)
microdoh --verbose --upstream https://dns.google/dns-query
```
