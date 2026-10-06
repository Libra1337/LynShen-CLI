// lynshen-relay forwards opaque bytes between paired clients and a daemon
// (docs/relay-protocol.md §3). It holds no keys, sees no content and stores
// nothing: every client stream is a Noise session the relay cannot read.
package main

import (
	"context"
	"crypto/ed25519"
	"crypto/rand"
	"crypto/sha256"
	"encoding/base64"
	"encoding/binary"
	"encoding/json"
	"errors"
	"flag"
	"log"
	"net"
	"net/http"
	"strings"
	"sync"
	"time"

	"github.com/coder/websocket"
)

const (
	kindOpen  = 1
	kindData  = 2
	kindClose = 3

	maxMessage        = 1 << 20
	streamsPerHost    = 32
	clientsPerMinute  = 20
	hostAuthPerMinute = 10
	handshakeTimeout  = 10 * time.Second
	idleStream        = 10 * time.Minute
	pingEvery         = 30 * time.Second
)

var b64 = base64.RawURLEncoding

type stream struct {
	conn     *websocket.Conn
	lastSeen time.Time
	cancel   context.CancelFunc
}

type host struct {
	id      string
	conn    *websocket.Conn
	writeMu sync.Mutex
	mu      sync.Mutex
	streams map[uint32]*stream
	nextID  uint32
	done    chan struct{}
}

func (h *host) send(ctx context.Context, kind byte, sid uint32, payload []byte) error {
	frame := make([]byte, 5+len(payload))
	frame[0] = kind
	binary.BigEndian.PutUint32(frame[1:5], sid)
	copy(frame[5:], payload)
	h.writeMu.Lock()
	defer h.writeMu.Unlock()
	return h.conn.Write(ctx, websocket.MessageBinary, frame)
}

type relay struct {
	mu    sync.Mutex
	hosts map[string]*host
	rate  *limiter
}

// limiter counts events per key in a sliding one-minute window.
type limiter struct {
	mu     sync.Mutex
	events map[string][]time.Time
}

func (l *limiter) allow(key string, perMinute int) bool {
	l.mu.Lock()
	defer l.mu.Unlock()
	now := time.Now()
	kept := l.events[key][:0]
	for _, t := range l.events[key] {
		if now.Sub(t) < time.Minute {
			kept = append(kept, t)
		}
	}
	if len(kept) >= perMinute {
		l.events[key] = kept
		return false
	}
	l.events[key] = append(kept, now)
	return true
}

func clientIP(r *http.Request) string {
	// Caddy terminates TLS on the same machine; the relay listens on loopback.
	if forwarded := r.Header.Get("X-Forwarded-For"); forwarded != "" {
		return strings.TrimSpace(strings.Split(forwarded, ",")[0])
	}
	ip, _, _ := net.SplitHostPort(r.RemoteAddr)
	return ip
}

// hostID is base64url(SHA-256(public key)[:16]), as the daemon derives it.
func hostID(public []byte) string {
	sum := sha256.Sum256(public)
	return b64.EncodeToString(sum[:16])
}

func (rl *relay) serveHost(w http.ResponseWriter, r *http.Request) {
	if !rl.rate.allow("auth:"+clientIP(r), hostAuthPerMinute) {
		http.Error(w, "too many attempts", http.StatusTooManyRequests)
		return
	}
	conn, err := websocket.Accept(w, r, nil)
	if err != nil {
		return
	}
	conn.SetReadLimit(maxMessage + 5)
	ctx := r.Context()

	nonce := make([]byte, 32)
	if _, err := rand.Read(nonce); err != nil {
		conn.Close(websocket.StatusInternalError, "rng")
		return
	}
	challenge, _ := json.Marshal(map[string]string{"t": "challenge", "nonce": b64.EncodeToString(nonce)})
	authCtx, cancel := context.WithTimeout(ctx, handshakeTimeout)
	defer cancel()
	if err := conn.Write(authCtx, websocket.MessageText, challenge); err != nil {
		return
	}
	kind, body, err := conn.Read(authCtx)
	if err != nil || kind != websocket.MessageText {
		conn.Close(4401, "auth failed")
		return
	}
	var auth struct {
		T   string `json:"t"`
		Pub string `json:"pub"`
		Sig string `json:"sig"`
		V   string `json:"v"`
	}
	if json.Unmarshal(body, &auth) != nil {
		conn.Close(4401, "auth failed")
		return
	}
	public, errPub := b64.DecodeString(auth.Pub)
	signature, errSig := b64.DecodeString(auth.Sig)
	message := append([]byte("lynshen-relay-v1:"), nonce...)
	if auth.T != "auth" || errPub != nil || errSig != nil || len(public) != ed25519.PublicKeySize ||
		!ed25519.Verify(public, message, signature) {
		conn.Close(4401, "auth failed")
		return
	}
	h := &host{id: hostID(public), conn: conn, streams: map[uint32]*stream{}, done: make(chan struct{})}
	ready, _ := json.Marshal(map[string]string{"t": "ready", "host": h.id})
	if err := conn.Write(ctx, websocket.MessageText, ready); err != nil {
		return
	}

	rl.mu.Lock()
	previous := rl.hosts[h.id]
	rl.hosts[h.id] = h
	rl.mu.Unlock()
	if previous != nil {
		previous.conn.Close(4409, "replaced")
	}
	log.Printf("host %s connected (daemon %s)", h.id, auth.V)

	go func() {
		ticker := time.NewTicker(pingEvery)
		defer ticker.Stop()
		for {
			select {
			case <-h.done:
				return
			case <-ticker.C:
				pingCtx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
				err := conn.Ping(pingCtx)
				cancel()
				if err != nil {
					conn.Close(websocket.StatusGoingAway, "ping timeout")
					return
				}
			}
		}
	}()

	defer func() {
		close(h.done)
		rl.mu.Lock()
		if rl.hosts[h.id] == h {
			delete(rl.hosts, h.id)
		}
		rl.mu.Unlock()
		h.mu.Lock()
		for _, s := range h.streams {
			s.conn.Close(4404, "host offline")
			s.cancel()
		}
		h.streams = map[uint32]*stream{}
		h.mu.Unlock()
		log.Printf("host %s disconnected", h.id)
	}()

	for {
		kind, frame, err := conn.Read(context.Background())
		if err != nil {
			return
		}
		if kind != websocket.MessageBinary || len(frame) < 5 {
			continue
		}
		sid := binary.BigEndian.Uint32(frame[1:5])
		h.mu.Lock()
		s := h.streams[sid]
		h.mu.Unlock()
		if s == nil {
			continue
		}
		switch frame[0] {
		case kindData:
			s.lastSeen = time.Now()
			writeCtx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
			err := s.conn.Write(writeCtx, websocket.MessageBinary, frame[5:])
			cancel()
			if err != nil {
				s.cancel()
			}
		case kindClose:
			s.conn.Close(4410, "host closed")
			s.cancel()
		}
	}
}

func (rl *relay) serveConnect(w http.ResponseWriter, r *http.Request) {
	if !rl.rate.allow("connect:"+clientIP(r), clientsPerMinute) {
		conn, err := websocket.Accept(w, r, nil)
		if err == nil {
			conn.Close(4429, "too many connections")
		}
		return
	}
	conn, err := websocket.Accept(w, r, &websocket.AcceptOptions{InsecureSkipVerify: true})
	if err != nil {
		return
	}
	conn.SetReadLimit(maxMessage)
	rl.mu.Lock()
	h := rl.hosts[r.URL.Query().Get("host")]
	rl.mu.Unlock()
	if h == nil {
		conn.Close(4404, "host offline")
		return
	}
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	h.mu.Lock()
	if len(h.streams) >= streamsPerHost {
		h.mu.Unlock()
		conn.Close(4429, "too many streams")
		return
	}
	h.nextID++
	if h.nextID == 0 {
		h.nextID = 1
	}
	sid := h.nextID
	s := &stream{conn: conn, lastSeen: time.Now(), cancel: cancel}
	h.streams[sid] = s
	h.mu.Unlock()
	defer func() {
		h.mu.Lock()
		_, live := h.streams[sid]
		delete(h.streams, sid)
		h.mu.Unlock()
		if live {
			closeCtx, done := context.WithTimeout(context.Background(), 5*time.Second)
			h.send(closeCtx, kindClose, sid, nil)
			done()
		}
		conn.Close(websocket.StatusNormalClosure, "")
	}()
	if err := h.send(ctx, kindOpen, sid, nil); err != nil {
		return
	}

	go func() {
		ticker := time.NewTicker(time.Minute)
		defer ticker.Stop()
		for {
			select {
			case <-ctx.Done():
				return
			case <-ticker.C:
				if time.Since(s.lastSeen) > idleStream {
					conn.Close(4408, "idle")
					cancel()
					return
				}
			}
		}
	}()

	for {
		kind, payload, err := conn.Read(ctx)
		if err != nil {
			return
		}
		if kind != websocket.MessageBinary {
			conn.Close(4400, "binary frames only")
			return
		}
		s.lastSeen = time.Now()
		if err := h.send(ctx, kindData, sid, payload); err != nil {
			return
		}
	}
}

func main() {
	listen := flag.String("listen", "127.0.0.1:18095", "address to listen on")
	flag.Parse()
	rl := &relay{hosts: map[string]*host{}, rate: &limiter{events: map[string][]time.Time{}}}
	mux := http.NewServeMux()
	mux.HandleFunc("/relay/v1/host", rl.serveHost)
	mux.HandleFunc("/relay/v1/connect", rl.serveConnect)
	mux.HandleFunc("/relay/v1/healthz", func(w http.ResponseWriter, _ *http.Request) {
		w.Write([]byte("ok"))
	})
	server := &http.Server{Addr: *listen, Handler: mux, ReadHeaderTimeout: 10 * time.Second}
	log.Printf("lynshen-relay listening on %s", *listen)
	if err := server.ListenAndServe(); err != nil && !errors.Is(err, http.ErrServerClosed) {
		log.Fatal(err)
	}
}
