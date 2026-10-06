"""Restricted SSH receiver for the LynShen software library.

Install root-owned at /usr/local/lib/lynshen/receive-software.py and bind it to
the publisher's key in authorized_keys:

  restrict,command="/usr/bin/python3 /usr/local/lib/lynshen/receive-software.py /srv/software" ssh-ed25519 AAAA... software-publisher

The client sends `tar -czf - -C <out> .` where <out> is what
`software-release sign` wrote: latest.json plus <version>/<files>. The
receiver checks the signature against the key in this file, the sha256 of
every file and the version order, then switches latest.json atomically.
Published versions are immutable.
"""
import base64
import fcntl
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import sys
import tarfile
import tempfile

# Same key as MANIFEST_PUBLIC_KEY in crates/agent-core/src/update.rs.
PUBLIC_KEY = 'rNaDD3lDFhpKdEGDOZdTHGBW9uEoQJ5jaQeTYrhz0tQ='
PRODUCTS = ('cli',)
NAME = re.compile(r'[A-Za-z0-9][A-Za-z0-9._-]*')
VERSION = re.compile(r'\d+\.\d+\.\d+')
MAX_BYTES = 1024**3


def verify(manifest):
    """Ed25519 check with the `cryptography` package (python3-cryptography)."""
    from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PublicKey
    key = Ed25519PublicKey.from_public_bytes(base64.b64decode(PUBLIC_KEY))
    key.verify(base64.b64decode(manifest['signature']), manifest['release'].encode())
    return json.loads(manifest['release'])


def version_key(version):
    return tuple(int(part) for part in version.split('.'))


def receive(stream, root, product):
    if product not in PRODUCTS:
        raise ValueError('Unknown product')
    root = Path(root).resolve() / product
    root.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='.incoming-', dir=root) as temporary:
        stage = Path(temporary)
        total = 0
        names = set()
        with tarfile.open(fileobj=stream, mode='r|gz') as archive:
            for member in archive:
                name = member.name.removeprefix('./').rstrip('/')
                if member.isdir() and name in ('', '.'):
                    continue
                parts = PurePosixPath(name).parts
                if (not parts or len(parts) > 2 or PurePosixPath(name).is_absolute()
                        or any(not NAME.fullmatch(part) for part in parts)
                        or name in names or len(names) >= 64):
                    raise ValueError('Invalid archive path or entry count')
                names.add(name)
                if member.isdir():
                    if len(parts) != 1 or not VERSION.fullmatch(name):
                        raise ValueError('Invalid release directory')
                    (stage / name).mkdir(exist_ok=True)
                    continue
                total += member.size
                if not member.isfile() or member.size < 0 or total > MAX_BYTES:
                    raise ValueError('Invalid release entry or size')
                target = stage / name
                target.parent.mkdir(exist_ok=True)
                source = archive.extractfile(member)
                with target.open('xb') as output:
                    while chunk := source.read(1024 * 1024):
                        output.write(chunk)
        manifest = json.loads((stage / 'latest.json').read_text())
        release = verify(manifest)
        version = release['version']
        if release.get('product') != product or not VERSION.fullmatch(version):
            raise ValueError('Manifest is not for this product or version')
        expected = {'latest.json', version}
        for entry in release['files']:
            name = entry['name']
            if not NAME.fullmatch(name):
                raise ValueError('Invalid file name')
            path = stage / version / name
            with path.open('rb') as source:
                if hashlib.file_digest(source, 'sha256').hexdigest() != entry['sha256']:
                    raise ValueError(f'Checksum mismatch: {name}')
            expected.add(f'{version}/{name}')
        if names != expected:
            raise ValueError('Archive files differ from the manifest')
        publish(stage, root, version)


def publish(stage, root, version):
    with (root / '.publish.lock').open('a') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        current = root / 'latest.json'
        if current.exists():
            published = verify(json.loads(current.read_text()))['version']
            if version_key(published) > version_key(version):
                raise ValueError('Refusing to publish an older release')
        destination = root / version
        if destination.exists():
            for path in (stage / version).iterdir():
                existing = destination / path.name
                if not existing.is_file() or existing.read_bytes() != path.read_bytes():
                    raise ValueError('Published version files are immutable')
        else:
            (stage / version).chmod(0o755)
            for path in (stage / version).iterdir():
                path.chmod(0o644)
            shutil.move(str(stage / version), destination)
        fd, filename = tempfile.mkstemp(prefix='.manifest-', dir=root)
        try:
            with os.fdopen(fd, 'wb') as output:
                output.write((stage / 'latest.json').read_bytes())
                output.flush()
                os.fsync(output.fileno())
            os.chmod(filename, 0o644)
            os.replace(filename, current)
        finally:
            if os.path.exists(filename):
                os.unlink(filename)
    print(f'Published {root.name} {version}')


if __name__ == '__main__':
    command = os.environ.get('SSH_ORIGINAL_COMMAND', '').split()
    if len(command) != 2 or command[0] != 'publish-software':
        raise SystemExit('Only `publish-software <product>` is allowed')
    receive(sys.stdin.buffer, sys.argv[1], command[1])
