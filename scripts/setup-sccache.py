#!/usr/bin/env python3
"""Install a pinned, checksum-verified official sccache binary in this project."""
import hashlib
import io
from pathlib import Path
import platform
import subprocess
import tarfile
import tempfile
import os
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
VERSION = "0.19.0"
ARCHIVE = f"sccache-v{VERSION}-x86_64-unknown-linux-musl"
URL = f"https://github.com/mozilla/sccache/releases/download/v{VERSION}/{ARCHIVE}.tar.gz"
# SHA-256 published on the official mozilla/sccache GitHub release.
SHA256 = "aa73c7270c997894038c0d8f0d297865f57569e1687883322bc431713d4bb9dd"


def main():
    destination = ROOT / ".tools/sccache"
    if destination.exists() or destination.is_symlink():
        print(f"Leaving existing file untouched: {destination}")
        return
    if platform.system() != "Linux" or platform.machine() not in ("x86_64", "AMD64"):
        raise SystemExit("This installer supports Linux x86-64; install sccache with your package manager instead.")
    request = urllib.request.Request(URL, headers={"User-Agent": "Wisp-build-setup"})
    with urllib.request.urlopen(request, timeout=60) as response:
        data = response.read(20 * 1024 * 1024 + 1)
    if len(data) > 20 * 1024 * 1024 or hashlib.sha256(data).hexdigest() != SHA256:
        raise SystemExit("Archive checksum mismatch; nothing was installed.")
    # Never extract archive paths to the filesystem: copy just the known binary.
    with tarfile.open(fileobj=io.BytesIO(data), mode="r:gz") as archive:
        member = archive.getmember(f"{ARCHIVE}/sccache")
        if not member.isfile() or member.size > 100 * 1024 * 1024:
            raise SystemExit("Unexpected sccache archive member")
        with archive.extractfile(member) as source:
            binary = source.read()
    destination.parent.mkdir(exist_ok=True)
    with tempfile.NamedTemporaryFile(dir=destination.parent, prefix="sccache-") as temporary:
        temporary.write(binary)
        temporary.flush()
        os.chmod(temporary.name, 0o755)
        # Atomic, and unlike replace(), refuses to overwrite an existing file.
        os.link(temporary.name, destination)
    subprocess.run([str(destination), "--version"], check=True)
    print(f"Installed {destination}; Cargo's project-local wrapper will use it automatically.")


if __name__ == "__main__":
    main()
