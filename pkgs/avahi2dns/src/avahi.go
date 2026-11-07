package main

import (
	"bytes"
	"fmt"
	"net"
	"sort"
	"strings"
	"sync"
	"time"

	"github.com/godbus/dbus/v5"
	"github.com/miekg/dns"
)

const (
	avahiName         = "org.freedesktop.Avahi"
	avahiServerPath   = "/"
	avahiServer2Iface = "org.freedesktop.Avahi.Server2"
	avahiBrowserIface = "org.freedesktop.Avahi.RecordBrowser"

	avahiProtoUnspec   = -1
	avahiInterfaceAny  = -1

	// browseSettle is how long a browse waits for one more record once the
	// last one arrived, browseTimeout bounds a browse nothing answers for.
	browseSettle      = 100 * time.Millisecond
	browseTimeout     = 2 * time.Second
	browseQueueLength = 128
)

// resolver is what the DNS side needs from avahi: every address it knows for a
// name, for one address family.
type resolver interface {
	addresses(qtype uint16, name string) ([]net.IP, error)
}

// avahiClient browses avahi's mDNS records over D-Bus.
//
// avahi offers two ways to resolve a host name: ResolveHostName answers with a
// single address, picked from whichever record it saw first, while a record
// browser reports every matching record as a separate ItemNew signal. Only the
// latter can answer a multi-homed host correctly, so that is all this uses.
type avahiClient struct {
	conn     *dbus.Conn
	signals  chan *dbus.Signal
	mu       sync.Mutex
	browsers map[dbus.ObjectPath]chan []byte
}

func newAvahiClient() (*avahiClient, error) {
	conn, err := dbus.SystemBus()
	if err != nil {
		return nil, fmt.Errorf("connect to the system bus: %w", err)
	}

	c := &avahiClient{
		conn:     conn,
		signals:  make(chan *dbus.Signal, 256),
		browsers: make(map[dbus.ObjectPath]chan []byte),
	}
	conn.Signal(c.signals)
	if err := conn.AddMatchSignal(dbus.WithMatchInterface(avahiBrowserIface)); err != nil {
		return nil, fmt.Errorf("watch %s signals: %w", avahiBrowserIface, err)
	}

	go c.dispatch()
	return c, nil
}

func (c *avahiClient) Close() error {
	return c.conn.Close()
}

// dispatch routes the ItemNew signals of every running browse to the goroutine
// that owns that browse.
func (c *avahiClient) dispatch() {
	for signal := range c.signals {
		if signal.Name != avahiBrowserIface+".ItemNew" {
			continue
		}

		var (
			iface, protocol int32
			name            string
			class, rtype    uint16
			rdata           []byte
			flags           uint32
		)
		if err := dbus.Store(signal.Body, &iface, &protocol, &name, &class, &rtype, &rdata, &flags); err != nil {
			continue
		}

		c.mu.Lock()
		queue := c.browsers[signal.Path]
		c.mu.Unlock()
		if queue == nil {
			continue
		}

		select {
		case queue <- rdata:
		default: // a browse that cannot keep up is not worth blocking the bus for
		}
	}
}

func (c *avahiClient) addresses(qtype uint16, name string) ([]net.IP, error) {
	switch qtype {
	case dns.TypeA, dns.TypeAAAA:
	default:
		return nil, fmt.Errorf("unsupported query type %d", qtype)
	}

	records, err := c.browse(strings.ToLower(strings.TrimSuffix(name, ".")), qtype)
	if err != nil {
		return nil, err
	}

	addresses := make([]net.IP, 0, len(records))
	for _, rdata := range records {
		address := net.IP(rdata)
		switch {
		case qtype == dns.TypeA && address.To4() != nil:
		case qtype == dns.TypeAAAA && address.To4() == nil && address.To16() != nil:
		default:
			continue
		}
		addresses = append(addresses, address)
	}

	// Sorted so that a host with several addresses answers in the same order
	// every time.
	sort.Slice(addresses, func(i, j int) bool {
		return bytes.Compare(addresses[i], addresses[j]) < 0
	})
	return addresses, nil
}

// browse collects every record of one type that avahi reports for name.
//
// Records arrive as signals, with no marker for "and that was all", so
// collection stops once the browser has been quiet for browseSettle, or after
// browseTimeout for a name nothing answers for.
func (c *avahiClient) browse(name string, qtype uint16) ([][]byte, error) {
	path, err := c.prepare(name, qtype)
	if err != nil {
		return nil, err
	}
	defer func() {
		// Releasing the browser object is best effort: it also disappears when
		// the bus connection goes away.
		_ = c.call(path, "Free")
	}()

	queue := make(chan []byte, browseQueueLength)
	c.mu.Lock()
	c.browsers[path] = queue
	c.mu.Unlock()
	defer func() {
		c.mu.Lock()
		delete(c.browsers, path)
		c.mu.Unlock()
	}()

	if err := c.call(path, "Start"); err != nil {
		return nil, err
	}

	var (
		records [][]byte
		seen    = make(map[string]bool)
		settle  *time.Timer
		settleC <-chan time.Time
	)
	timeout := time.NewTimer(browseTimeout)
	defer timeout.Stop()

	for {
		select {
		case rdata := <-queue:
			if seen[string(rdata)] {
				continue
			}
			seen[string(rdata)] = true
			records = append(records, rdata)

			if settle == nil {
				settle = time.NewTimer(browseSettle)
				settleC = settle.C
				defer settle.Stop()
			} else {
				settle.Reset(browseSettle)
			}

		case <-settleC:
			return records, nil

		case <-timeout.C:
			return records, nil
		}
	}
}

// prepare creates a record browser without starting it, so that its signals
// cannot arrive before the browse that owns them is registered. The
// convenience method Server.RecordBrowserNew starts the browser inside the
// call, which races with the registration of its own object path.
//
// The interface protocol is unspecified on purpose: responders announce A and
// AAAA records over both transports, so an A record can be reachable only
// through an IPv6-only interface. The record type alone selects the family.
func (c *avahiClient) prepare(name string, qtype uint16) (dbus.ObjectPath, error) {
	var path dbus.ObjectPath
	err := c.conn.Object(avahiName, avahiServerPath).Call(
		avahiServer2Iface+".RecordBrowserPrepare", 0,
		int32(avahiInterfaceAny),
		int32(avahiProtoUnspec),
		name,
		uint16(dns.ClassINET),
		qtype,
		uint32(0),
	).Store(&path)
	if err != nil {
		return "", fmt.Errorf("prepare a record browser for %s: %w", name, err)
	}
	return path, nil
}

func (c *avahiClient) call(path dbus.ObjectPath, method string) error {
	return c.conn.Object(avahiName, path).Call(avahiBrowserIface+"."+method, 0).Err
}
