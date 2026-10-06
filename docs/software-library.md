# LynShen software library

`https://software.lynshen.org` serves LynShen releases as static files. The
CLI updates from it; nothing else is needed on the server.

```
/cli/latest.json            signed manifest of the newest release (never cached)
/cli/<version>/<file>       release files (immutable)
```

## Manifest

```json
{
  "release": "{\"product\":\"cli\",\"version\":\"0.4.9\",\"files\":[{\"name\":\"lynshen-aarch64-apple-darwin\",\"sha256\":\"…\",\"size\":123}]}",
  "signature": "<base64 Ed25519 signature over the exact bytes of release>",
  "key": "<base64 public key, informational>"
}
```

`release` is a string so the signed bytes are exactly the served bytes. File
names are `lynshen-<target>` (`.exe` on Windows), as `update::binary_name`
expects.

## How the CLI uses it

- At startup the CLI fetches `/cli/latest.json` in the background and verifies
  the signature with the public key built into it
  (`MANIFEST_PUBLIC_KEY` in `crates/agent-core/src/update.rs`). A manifest
  signed by any other key, or with a file name that could leave its version
  directory, is refused.
- When the release is newer, a release binary (not npm, not the copy Desktop
  manages, not a debug build) downloads its file, checks the sha256 from the
  manifest, runs it with `--version`, and replaces itself. The new version
  runs from the next start. `auto_update: false` in `config.json` turns this
  off; the CLI then only prints a notice.
- `lynshen update` does the same in the foreground.

## Keys

- `software-release keygen <file>` writes a new private key (mode 0600) and
  prints its public key. The private key never goes into a repository or CI
  log. The current key lives on the release machine in
  `~/.lynshen-release/software-signing.key`.
- Rotating the key: put the new public key in `update.rs` and
  `receive-software.py`, ship one release signed with the old key that
  contains the new public key, then sign with the new key.

## Publishing

```sh
SOFTWARE_SIGNING_KEY=~/.lynshen-release/software-signing.key \
SOFTWARE_SSH=software@40.160.141.21 \
deploy/software-library/publish.sh cli 0.4.9 ./release-files
```

The release workflows build the binaries (GitHub Release of the tag). Once
both have finished, put the four `lynshen-<target>` files in one directory
and run `publish.sh` on the machine that holds the key. CI never sees the
private key, and each version is published once with every platform.

`publish.sh` signs the files with `software-release sign` and streams
`latest.json` plus `<version>/<files>` to the server. The server's
`receive-software.py` checks the signature again, every sha256, that the
archive holds exactly the listed files, and that the version is not older
than the published one. Published versions cannot change. Then it switches
`latest.json` atomically.

## Server setup (40.160.141.21)

1. DNS: `software.lynshen.org` A record → `40.160.141.21`.
2. `apt install python3 python3-cryptography` (Python 3.11 or newer).
3. Create `/srv/software`, owned by a `software` user, readable by Caddy.
4. Install `deploy/software-library/receive-software.py` root-owned at
   `/usr/local/lib/lynshen/receive-software.py`.
5. In `~software/.ssh/authorized_keys`, add the publisher's key with:
   `restrict,command="/usr/bin/python3 /usr/local/lib/lynshen/receive-software.py /srv/software" ssh-ed25519 …`
6. Add `deploy/software-library/Caddyfile` to Caddy and reload it. Caddy gets
   the certificate itself once DNS points at the server.
