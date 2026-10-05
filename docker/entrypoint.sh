#!/bin/sh
# First start seeds config.json:
# - the LynShen gateway and a model it serves (LYNSHEN_MODEL, default
#   gpt-6-sol); fields left out fall back to non-LynShen defaults;
# - the sandbox off: the container is the isolation boundary
#   (gVisor/Firecracker in production, see docs/cloud-agent.md), and the
#   engine's own sandbox needs user namespaces a container lacks.
set -e
mkdir -p "$HOME/.lynshen"
if [ ! -f "$HOME/.lynshen/config.json" ]; then
    printf '{"provider":"lynshen","model":"%s","sandbox":"full-access"}\n' \
        "${LYNSHEN_MODEL:-gpt-6-sol}" > "$HOME/.lynshen/config.json"
fi
# The relay is the only way in.
lynshen daemon relay on >/dev/null
exec lynshen daemon --listen 127.0.0.1:7788 "$@"
