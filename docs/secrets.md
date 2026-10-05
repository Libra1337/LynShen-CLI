# Encrypted credentials

LynShen Desktop can encrypt provider API keys and LynShen OAuth tokens in
`~/.lynshen/auth.json` when `"encrypt_secrets": true` is set in
`~/.lynshen/config.json`. The CLI accepts both plaintext values and Desktop's
encrypted envelope:

```text
jcenc1:<base64(nonce || ciphertext || tag)>
```

The envelope uses ChaCha20-Poly1305 with a 32-byte key and a fresh 12-byte
nonce. Provider values and `lynshen.access_token` / `lynshen.refresh_token` are
encrypted; expiry metadata remains plaintext. An invalid envelope, missing
key, wrong key, or malformed key is a load error. The CLI does not substitute
an empty credential or overwrite the unreadable auth file.

## Key lookup

The CLI tries these locations in order:

1. `LYNSHEN_SECRET_KEY_PATH`, when set.
2. `~/.lynshen/secret.key`, a shared CLI/Desktop location.
3. LynShen Desktop's Tauri app config location:
   - macOS: `~/Library/Application Support/com.lynshen.desktop/secret.key`
   - Linux: `$XDG_CONFIG_HOME/com.lynshen.desktop/secret.key`, or
     `~/.config/com.lynshen.desktop/secret.key`
   - Windows: `%APPDATA%\com.lynshen.desktop\secret.key`

The file must contain exactly 32 bytes. When encrypted credentials were read,
subsequent CLI auth writes use the same key and remain encrypted. If encryption
is enabled before Desktop has created a key, the CLI remains compatible with
plaintext until a key appears.

The key lives on the same machine as the encrypted file. This protects secrets
from casual disclosure in backups, screen shares, and support bundles; it does
not protect against software already running as the user.
