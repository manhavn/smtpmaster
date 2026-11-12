#!/usr/bin/env bash
# Run the test suite against a throwaway MongoDB instance.
set -Eeuo pipefail

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$ROOT"

PORT="${SMTPMASTER_TEST_MONGO_PORT:-27999}"
DBPATH="${SMTPMASTER_TEST_MONGO_DBPATH:-$(mktemp -d -t smtpmaster-mongo-XXXXXX)}"
LOG="$DBPATH/mongod.log"
PIDFILE="$DBPATH/mongod.pid"
STARTED=0

mongod_cmd() {
    if command -v mongod >/dev/null 2>&1; then
        command -v mongod
        return
    fi
    if command -v mise >/dev/null 2>&1; then
        MISE_YES=1 MISE_TRUSTED_CONFIG_PATHS="$ROOT" mise install mongodb@8 >/dev/null
        mise which mongod 2>/dev/null || true
    fi
}

cleanup() {
    if [[ "$STARTED" == 1 && -f "$PIDFILE" ]]; then
        kill "$(cat "$PIDFILE")" >/dev/null 2>&1 || true
    fi
}
trap cleanup EXIT

if [[ -n "${SMTPMASTER_TEST_MONGO_URI:-}" ]]; then
    echo "==> using external MongoDB: $SMTPMASTER_TEST_MONGO_URI"
else
    MONGOD=$(mongod_cmd)
    if [[ -z "$MONGOD" ]]; then
        echo "error: mongod not found (install MongoDB or mise)" >&2
        exit 1
    fi
    mkdir -p "$DBPATH"
    echo "==> starting mongod on 127.0.0.1:$PORT"
    "$MONGOD" --dbpath "$DBPATH" --port "$PORT" --bind_ip 127.0.0.1 \
        --logpath "$LOG" --pidfilepath "$PIDFILE" --fork >/dev/null
    STARTED=1
    export SMTPMASTER_TEST_MONGO_URI="mongodb://127.0.0.1:$PORT"
fi

echo "==> cargo test"
cargo test --all-targets

echo "==> cargo clippy"
cargo clippy --all-targets -- -D warnings
