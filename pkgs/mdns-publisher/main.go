// Command mdns-publisher is a minimal mDNS hostname responder.
//
// It listens on the given network interface and answers mDNS A/AAAA
// queries for "<hostname>.local" with the interface's global unicast
// addresses. Pair it with smartdns's `mdns-lookup yes` (or any other
// mDNS-aware DNS resolver) to expose this host under a .local name on
// the LAN without running a full Zeroconf stack (avahi/bonjour).
//
// Intentionally tiny: no service discovery, no reflection, no caching
// beyond what hashicorp/mdns provides.
package main

import (
	"flag"
	"log"
	"net"
	"os"
	"os/signal"
	"strings"
	"syscall"
	"time"

	"github.com/hashicorp/mdns"
	"github.com/miekg/dns"
)

func main() {
	ifaceName := flag.String("iface", "", "network interface to bind mDNS to (required)")
	hostnameOverride := flag.String("hostname", "", "hostname (without .local) to advertise; empty = use system hostname")
	ttl := flag.Uint("ttl", 120, "TTL in seconds for the published A/AAAA records")
	refresh := flag.Duration("refresh", 60*time.Second, "interval between health log lines")
	flag.Parse()

	if *ifaceName == "" {
		log.Fatalf("--iface is required")
	}

	hostname := *hostnameOverride
	if hostname == "" {
		h, err := os.Hostname()
		if err != nil {
			log.Fatalf("hostname: %v", err)
		}
		hostname = h
	}
	if i := strings.IndexByte(hostname, '.'); i >= 0 {
		hostname = hostname[:i] // strip any domain component
	}

	iface, err := net.InterfaceByName(*ifaceName)
	if err != nil {
		log.Fatalf("interface %s: %v", *ifaceName, err)
	}

	addrs, err := iface.Addrs()
	if err != nil {
		log.Fatalf("addrs: %v", err)
	}

	fqdn := hostname + ".local."
	uintTTL := uint32(*ttl)

	var records []dns.RR
	for _, a := range addrs {
		var ip net.IP
		switch v := a.(type) {
		case *net.IPNet:
			ip = v.IP
		case *net.IPAddr:
			ip = v.IP
		}
		if ip == nil || ip.IsLinkLocalUnicast() || ip.IsLoopback() {
			continue
		}
		if v4 := ip.To4(); v4 != nil {
			records = append(records, &dns.A{
				Hdr: dns.RR_Header{
					Name:   fqdn,
					Rrtype: dns.TypeA,
					Class:  dns.ClassINET,
					Ttl:    uintTTL,
				},
				A: v4,
			})
		} else {
			records = append(records, &dns.AAAA{
				Hdr: dns.RR_Header{
					Name:   fqdn,
					Rrtype: dns.TypeAAAA,
					Class:  dns.ClassINET,
					Ttl:    uintTTL,
				},
				AAAA: ip,
			})
		}
	}

	if len(records) == 0 {
		log.Fatalf("no usable global unicast addresses on %s", *ifaceName)
	}

	log.Printf("publishing %s on %s with %d record(s):", fqdn, *ifaceName, len(records))
	for _, r := range records {
		log.Printf("  %s", r.String())
	}

	zone, err := mdns.NewMDNSZone()
	if err != nil {
		log.Fatalf("zone: %v", err)
	}
	if err := zone.Insert(records...); err != nil {
		log.Fatalf("insert: %v", err)
	}

	server, err := mdns.NewServer(&mdns.Config{
		Zone: zone,
		Iface: iface,
	})
	if err != nil {
		log.Fatalf("server: %v", err)
	}
	defer server.Shutdown()

	// periodically log liveness + the published record count so the
	// daemon is observable in `journalctl -u mdns-publisher`.
	go func() {
		t := time.NewTicker(*refresh)
		defer t.Stop()
		for range t.C {
			log.Printf("alive (%d record(s) for %s)", len(records), fqdn)
		}
	}()

	// shut down cleanly on SIGTERM/SIGINT so systemd Restart= works
	sigCh := make(chan os.Signal, 1)
	signal.Notify(sigCh, syscall.SIGINT, syscall.SIGTERM)
	<-sigCh
	log.Printf("shutting down")
}