#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cache=""
if [ "${WISP_NO_SCCACHE:-0}" != "1" ]; then
    if [ -x "$root/.tools/sccache" ]; then
        cache="$root/.tools/sccache"
    elif command -v sccache >/dev/null 2>&1; then
        cache=$(command -v sccache)
    fi
fi

if [ -n "$cache" ]; then
    mkdir -p "$root/.cache"
    : "${SCCACHE_DIR:=$root/.cache/sccache}"
    : "${SCCACHE_CACHE_SIZE:=1G}"
    : "${SCCACHE_CONF:=$root/.cargo/sccache.toml}"
    : "${SCCACHE_SERVER_UDS:=$root/.cache/sccache.sock}"
    : "${SCCACHE_IGNORE_SERVER_IO_ERROR:=1}"
    export SCCACHE_DIR SCCACHE_CACHE_SIZE SCCACHE_CONF SCCACHE_SERVER_UDS SCCACHE_IGNORE_SERVER_IO_ERROR
    exec "$cache" "$@"
fi

case "${1:-}" in
    ""|-*)
        echo "sccache unavailable or disabled; install it with python3 scripts/setup-sccache.py" >&2
        exit 1
        ;;
esac
# Cargo supplies the real compiler as the first argument. Missing sccache must
# not break a fresh checkout, and incremental Rust builds remain enabled.
exec "$@"
