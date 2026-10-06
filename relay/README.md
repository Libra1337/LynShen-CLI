# lynshen-relay

The relay of `docs/relay-protocol.md` §3: forwards opaque Noise traffic between
paired clients and a daemon. It holds no keys, reads no content and stores
nothing on disk.

```sh
go build -o lynshen-relay .
./lynshen-relay -listen 127.0.0.1:18095
```

Put it behind TLS at `https://app.lynshen.org/relay/v1/*` (the daemon default).
Implemented: host auth, client streams, limits of §3.3, `healthz`. Not
implemented: Web Push and Getui (§3.3, the endpoints are not served).

Interop check against a real daemon:
`cargo run -p lynshen-daemon --example relay_phone -- ws://127.0.0.1:18095/relay/v1 '<pair link>'`
