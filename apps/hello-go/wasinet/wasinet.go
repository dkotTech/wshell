// Package wasinet is a hand-written TinyGo network driver on top of wasi:sockets (WASI 0.2).
//
// TinyGo's net package works through a pluggable driver (netdev), and there is no
// driver for wasip2: net.Listen returns an error. This is a minimal TCP driver for
// servers (bind/listen/accept/recv/send), enabled by importing it:
//
//	import _ "wshell.example/hello-go/wasinet"
//
// Experiment limitations: IPv4 and incoming connections only. TinyGo goroutines
// are cooperative and single-threaded, so a single reactor goroutine waits on
// all sockets at once (wasi:io/poll.poll) and wakes those that are ready: a slow or
// silent client does not block the others.
package wasinet

import (
	"errors"
	"io"
	"net/netip"
	"runtime"
	"time"
	_ "unsafe" // go:linkname

	"go.bytecodealliance.org/cm"

	monotonicclock "wshell.example/hello-go/internal/wasi/clocks/monotonic-clock"
	"wshell.example/hello-go/internal/wasi/io/poll"
	"wshell.example/hello-go/internal/wasi/io/streams"
	instancenetwork "wshell.example/hello-go/internal/wasi/sockets/instance-network"
	"wshell.example/hello-go/internal/wasi/sockets/network"
	"wshell.example/hello-go/internal/wasi/sockets/tcp"
	tcpcreatesocket "wshell.example/hello-go/internal/wasi/sockets/tcp-create-socket"
)

// netdever mirrors the driver interface from TinyGo's net package.
type netdever interface {
	GetHostByName(name string) (netip.Addr, error)
	Addr() (netip.Addr, error)
	Socket(domain int, stype int, protocol int) (int, error)
	Bind(sockfd int, ip netip.AddrPort) error
	Connect(sockfd int, host string, ip netip.AddrPort) error
	Listen(sockfd int, backlog int) error
	Accept(sockfd int) (int, netip.AddrPort, error)
	Send(sockfd int, buf []byte, flags int, deadline time.Time) (int, error)
	Recv(sockfd int, buf []byte, flags int, deadline time.Time) (int, error)
	Close(sockfd int) error
	SetSockOpt(sockfd int, level int, opt int, value interface{}) error
}

//go:linkname useNetdev net.useNetdev
func useNetdev(dev netdever)

func init() {
	go reactor()
	useNetdev(&driver{sockets: map[int]*socket{}, next: 3})
}

type socket struct {
	tcp tcp.TCPSocket
	in  streams.InputStream
	out streams.OutputStream
	// Only accepted connections have streams.
	connected bool
}

type driver struct {
	sockets map[int]*socket
	next    int
}

var (
	errUnsupported = errors.New("wasinet: not supported")
	errBadFd       = errors.New("wasinet: unknown socket")
)

func wasiErr(op string, code network.ErrorCode) error {
	return errors.New("wasinet: " + op + ": " + code.String())
}

func (d *driver) get(fd int) (*socket, error) {
	s, ok := d.sockets[fd]
	if !ok {
		return nil, errBadFd
	}
	return s, nil
}

// Waiting on the network. A goroutine registers its pollable and goes to sleep; the reactor waits
// on all registered ones at once and wakes the ready ones. While the reactor is in poll, the process sleeps,
// so no CPU is spent while idle.
type waiter struct {
	p    poll.Pollable
	done chan struct{}
}

var register = make(chan waiter, 64)

func wait(p poll.Pollable) {
	defer p.ResourceDrop()
	if p.Ready() {
		return
	}
	w := waiter{p: p, done: make(chan struct{})}
	register <- w
	<-w.done
}

// settleGap is a safety poll timeout right after waking goroutines: if one of them
// did not manage to register its wait before the reactor went into poll, it will wake
// no later than this. When idle (nobody to wake) there is no timeout.
const settleGap = 20 * time.Millisecond

func reactor() {
	var pending []waiter
	drain := func() {
		for {
			select {
			case w := <-register:
				pending = append(pending, w)
			default:
				return
			}
		}
	}
	woke := false
	for {
		if woke {
			// Woken goroutines (and those they spawned, e.g. a handler for a new
			// connection) must get a chance to run and start waiting before poll.
			for quiet := 0; quiet < 3; {
				runtime.Gosched()
				if len(register) > 0 {
					drain()
					quiet = 0
				} else {
					quiet++
				}
			}
		}
		if len(pending) == 0 {
			pending = append(pending, <-register) // nothing to wait for: sleep until the first registration
		}
		drain()

		list := make([]poll.Pollable, len(pending), len(pending)+1)
		for i, w := range pending {
			list[i] = w.p
		}
		if woke {
			list = append(list, monotonicclock.SubscribeDuration(monotonicclock.Duration(settleGap)))
		}
		ready := map[uint32]bool{}
		for _, i := range poll.Poll(cm.ToList(list)).Slice() {
			ready[i] = true
		}
		if woke {
			list[len(list)-1].ResourceDrop()
		}

		woke = false
		rest := pending[:0]
		for i, w := range pending {
			if ready[uint32(i)] {
				close(w.done)
				woke = true
			} else {
				rest = append(rest, w)
			}
		}
		pending = rest
	}
}

func (d *driver) GetHostByName(name string) (netip.Addr, error) {
	// A server does not need name resolution: only addresses are accepted.
	return netip.ParseAddr(name)
}

func (d *driver) Addr() (netip.Addr, error) { return netip.IPv4Unspecified(), nil }

func (d *driver) Socket(domain int, stype int, protocol int) (int, error) {
	const afInet, sockStream = 2, 1
	if domain != afInet || stype != sockStream {
		return -1, errUnsupported
	}
	res := tcpcreatesocket.CreateTCPSocket(network.IPAddressFamilyIPv4)
	if err := res.Err(); err != nil {
		return -1, wasiErr("socket", *err)
	}
	fd := d.next
	d.next++
	d.sockets[fd] = &socket{tcp: *res.OK()}
	return fd, nil
}

func (d *driver) Bind(fd int, ip netip.AddrPort) error {
	s, err := d.get(fd)
	if err != nil {
		return err
	}
	host := ip.Addr().Unmap()
	if !host.IsValid() {
		host = netip.IPv4Unspecified() // ":8482" means all interfaces
	}
	addr := network.IPSocketAddressIPv4(network.IPv4SocketAddress{
		Port:    ip.Port(),
		Address: network.IPv4Address(host.As4()),
	})
	if res := s.tcp.StartBind(instancenetwork.InstanceNetwork(), addr); res.IsErr() {
		return wasiErr("bind", *res.Err())
	}
	wait(s.tcp.Subscribe())
	if res := s.tcp.FinishBind(); res.IsErr() {
		return wasiErr("bind", *res.Err())
	}
	return nil
}

func (d *driver) Connect(int, string, netip.AddrPort) error { return errUnsupported }

func (d *driver) Listen(fd int, backlog int) error {
	s, err := d.get(fd)
	if err != nil {
		return err
	}
	if res := s.tcp.StartListen(); res.IsErr() {
		return wasiErr("listen", *res.Err())
	}
	wait(s.tcp.Subscribe())
	if res := s.tcp.FinishListen(); res.IsErr() {
		return wasiErr("listen", *res.Err())
	}
	return nil
}

func (d *driver) Accept(fd int) (int, netip.AddrPort, error) {
	s, err := d.get(fd)
	if err != nil {
		return -1, netip.AddrPort{}, err
	}
	for {
		res := s.tcp.Accept()
		if code := res.Err(); code != nil {
			if *code != network.ErrorCodeWouldBlock {
				return -1, netip.AddrPort{}, wasiErr("accept", *code)
			}
			wait(s.tcp.Subscribe())
			continue
		}
		conn := res.OK()
		client := &socket{tcp: conn.F0, in: conn.F1, out: conn.F2, connected: true}
		cfd := d.next
		d.next++
		d.sockets[cfd] = client
		return cfd, remoteAddr(client.tcp), nil
	}
}

func remoteAddr(s tcp.TCPSocket) netip.AddrPort {
	res := s.RemoteAddress()
	if res.IsErr() {
		return netip.AddrPort{}
	}
	addr := res.OK()
	if v4 := addr.IPv4(); v4 != nil {
		return netip.AddrPortFrom(netip.AddrFrom4(v4.Address), v4.Port)
	}
	return netip.AddrPort{}
}

func (d *driver) Recv(fd int, buf []byte, flags int, deadline time.Time) (int, error) {
	s, err := d.get(fd)
	if err != nil || !s.connected {
		return -1, errBadFd
	}
	wait(s.in.Subscribe())
	res := s.in.Read(uint64(len(buf)))
	if e := res.Err(); e != nil {
		if e.Closed() {
			return 0, io.EOF
		}
		return -1, errors.New("wasinet: recv")
	}
	return copy(buf, res.OK().Slice()), nil
}

func (d *driver) Send(fd int, buf []byte, flags int, deadline time.Time) (int, error) {
	s, err := d.get(fd)
	if err != nil || !s.connected {
		return -1, errBadFd
	}
	// blocking-write-and-flush accepts at most 4096 bytes at a time.
	sent := 0
	for sent < len(buf) {
		chunk := buf[sent:min(sent+4096, len(buf))]
		if res := s.out.BlockingWriteAndFlush(cm.ToList(chunk)); res.IsErr() {
			return sent, errors.New("wasinet: send")
		}
		sent += len(chunk)
	}
	return sent, nil
}

func (d *driver) Close(fd int) error {
	s, err := d.get(fd)
	if err != nil {
		return err
	}
	delete(d.sockets, fd)
	if s.connected {
		s.in.ResourceDrop()
		s.out.ResourceDrop()
	}
	s.tcp.ResourceDrop()
	return nil
}

func (d *driver) SetSockOpt(int, int, int, interface{}) error { return nil }
