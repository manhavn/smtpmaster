#!/usr/bin/env bash
# Build statically linked musl binaries (amd64 + arm64) and package them into
# ./outputs for a manual GitHub release upload. No CI involved.
#
# Toolchain priority: mise -> container (podman/docker) -> system.
# Missing build tools are installed automatically when mise is available.
set -Eeuo pipefail

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$ROOT"

if [[ -t 1 ]]; then
    C_DIM=$'\033[2m' C_RED=$'\033[31m' C_GREEN=$'\033[32m' C_YELLOW=$'\033[33m' C_RESET=$'\033[0m'
else
    C_DIM='' C_RED='' C_GREEN='' C_YELLOW='' C_RESET=''
fi
info() { printf '%s==>%s %s\n' "$C_GREEN" "$C_RESET" "$*"; }
warn() { printf '%s!!%s %s\n' "$C_YELLOW" "$C_RESET" "$*"; }
die() {
    printf '%serror:%s %s\n' "$C_RED" "$C_RESET" "$*" >&2
    exit 1
}

usage() {
    cat <<'USAGE'
Usage: scripts/build-release.sh [options]

Builds static musl release binaries and packages ./outputs.

Options:
  --version <ver>        Override the version from Cargo.toml
  --targets "<list>"     Space or comma separated Rust targets
                         (default: x86_64-unknown-linux-musl aarch64-unknown-linux-musl)
  --provider <name>      auto | mise | container | system   (default: auto)
  --output <dir>         Output directory                    (default: ./outputs)
  --no-strict            Do not turn rustc warnings into errors
  --keep-stage           Keep the temporary packaging directory
  -h, --help             Show this help

Environment:
  VERSION, TARGETS, PROVIDER, OUTPUT_DIR, JOBS, REPO_URL, STRICT
USAGE
}

VERSION="${VERSION:-}"
OUTPUT_DIR="${OUTPUT_DIR:-$ROOT/outputs}"
TARGETS_SPEC="${TARGETS:-x86_64-unknown-linux-musl aarch64-unknown-linux-musl}"
PROVIDER="${PROVIDER:-auto}"
STRICT="${STRICT:-1}"
JOBS="${JOBS:-$(nproc 2>/dev/null || echo 4)}"
KEEP_STAGE=0

while [[ $# -gt 0 ]]; do
    case "$1" in
        --version) [[ $# -ge 2 ]] || die "--version needs a value"; VERSION=$2; shift 2 ;;
        --targets) [[ $# -ge 2 ]] || die "--targets needs a value"; TARGETS_SPEC=$2; shift 2 ;;
        --provider) [[ $# -ge 2 ]] || die "--provider needs a value"; PROVIDER=$2; shift 2 ;;
        --output) [[ $# -ge 2 ]] || die "--output needs a value"; OUTPUT_DIR=$2; shift 2 ;;
        --no-strict) STRICT=0; shift ;;
        --keep-stage) KEEP_STAGE=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) die "unknown argument: $1 (try --help)" ;;
    esac
done

if [[ -z "$VERSION" ]]; then
    VERSION=$(sed -n 's/^version[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' Cargo.toml | head -n 1)
    [[ -n "$VERSION" ]] || die "cannot read version from Cargo.toml"
fi
VERSION="${VERSION#v}"

read -r -a TARGETS <<< "${TARGETS_SPEC//,/ }"
[[ ${#TARGETS[@]} -gt 0 ]] || die "no targets selected"

# Strict warnings: the release build must be warning-free. The linker_messages
# lint is exempt from `-D warnings`; allow it explicitly because zig's lld emits
# a harmless deprecation notice on every musl link.
if [[ "$STRICT" == 1 ]]; then
    export RUSTFLAGS="${RUSTFLAGS:-} -D warnings -A linker_messages"
fi
export RUSTFLAGS="${RUSTFLAGS# }"
export CARGO_TERM_COLOR="${CARGO_TERM_COLOR:-always}"

arch_for() {
    case "$1" in
        x86_64-unknown-linux-musl) echo amd64 ;;
        aarch64-unknown-linux-musl) echo arm64 ;;
        *) die "unsupported target '$1' (supported: x86_64-unknown-linux-musl, aarch64-unknown-linux-musl)" ;;
    esac
}

for target in "${TARGETS[@]}"; do
    arch_for "$target" >/dev/null
done

detect_provider() {
    local requested=$1
    case "$requested" in
        mise|container|system) echo "$requested" ;;
        auto)
            if command -v mise >/dev/null 2>&1 && [[ -f "$ROOT/.mise.toml" ]]; then
                echo mise
            elif command -v podman >/dev/null 2>&1 || command -v docker >/dev/null 2>&1; then
                echo container
            elif command -v cargo >/dev/null 2>&1 && command -v rustc >/dev/null 2>&1; then
                echo system
            else
                die "no usable build provider: install mise (https://mise.jdx.dev), podman/docker, or a Rust toolchain"
            fi
            ;;
        *) die "unknown --provider '$requested' (auto, mise, container, system)" ;;
    esac
}
PROVIDER=$(detect_provider "$PROVIDER")
info "version $VERSION, provider $PROVIDER, targets: ${TARGETS[*]}"

# ---------------------------------------------------------------------------
# mise provider
# ---------------------------------------------------------------------------
prepare_mise() {
    command -v mise >/dev/null 2>&1 || die "mise is not installed (see https://mise.jdx.dev)"
    export MISE_YES=1
    export MISE_TRUSTED_CONFIG_PATHS="$ROOT${MISE_TRUSTED_CONFIG_PATHS:+:$MISE_TRUSTED_CONFIG_PATHS}"
    ( cd "$ROOT" && mise trust >/dev/null 2>&1 || true )
    info "installing missing tools from .mise.toml"
    mise install || die "mise install failed"
    mise exec -- zig version >/dev/null 2>&1 || die "zig is missing after mise install"
    mise exec -- cargo-zigbuild --version >/dev/null 2>&1 || die "cargo-zigbuild is missing after mise install"
}

ensure_target_mise() {
    local target=$1
    if mise exec -- rustup --version >/dev/null 2>&1; then
        mise exec -- rustup target add "$target" >/dev/null 2>&1 || true
    fi
    local libdir
    libdir=$(mise exec -- rustc --print target-libdir --target "$target" 2>/dev/null || true)
    if [[ -z "$libdir" || ! -d "$libdir" ]]; then
        die "Rust std for $target is missing. Install it with: mise exec -- rustup target add $target"
    fi
}

build_mise() {
    local target=$1
    ensure_target_mise "$target"
    mise exec -- cargo zigbuild --release --target "$target" --locked --jobs "$JOBS"
}

# ---------------------------------------------------------------------------
# container provider (podman preferred, docker accepted)
# ---------------------------------------------------------------------------
ENGINE=""
prepare_container() {
    if command -v podman >/dev/null 2>&1; then
        ENGINE=podman
    elif command -v docker >/dev/null 2>&1; then
        ENGINE=docker
    else
        die "no container engine: install podman (preferred) or docker"
    fi
    "$ENGINE" info >/dev/null 2>&1 || die "$ENGINE daemon is not reachable"
}

build_container() {
    local target=$1 arch=$2
    local build_dir="$ROOT/.build/container-$arch"
    local cargo_home="$ROOT/.build/cargo-home"
    mkdir -p "$build_dir" "$cargo_home"
    info "building $target with $ENGINE (ghcr.io/rust-cross/cargo-zigbuild)"
    "$ENGINE" run --rm \
        --user "$(id -u):$(id -g)" \
        -e HOME=/tmp \
        -e CARGO_HOME=/cargo-home \
        -e CARGO_TARGET_DIR=/target \
        -e RUSTFLAGS="$RUSTFLAGS" \
        -v "$ROOT":/project \
        -v "$cargo_home":/cargo-home \
        -v "$build_dir":/target \
        -w /project \
        ghcr.io/rust-cross/cargo-zigbuild:latest \
        cargo zigbuild --release --target "$target" --locked --jobs "$JOBS" \
        || die "container build failed for $target"
}

# ---------------------------------------------------------------------------
# system provider
# ---------------------------------------------------------------------------
prepare_system() {
    command -v cargo >/dev/null 2>&1 || die "cargo is not installed"
    command -v rustc >/dev/null 2>&1 || die "rustc is not installed"
}

build_system() {
    local target=$1
    if command -v cargo-zigbuild >/dev/null 2>&1 && command -v zig >/dev/null 2>&1; then
        if command -v rustup >/dev/null 2>&1; then
            rustup target add "$target" >/dev/null 2>&1 || true
        fi
        cargo zigbuild --release --target "$target" --locked --jobs "$JOBS"
        return
    fi

    local linker="" compiler="" archiver=""
    local zig_bin=""
    if command -v zig >/dev/null 2>&1; then
        zig_bin=$(command -v zig)
    fi

    case "$target" in
        x86_64-unknown-linux-musl)
            if [[ -n "$zig_bin" ]]; then
                linker="$ROOT/.build/zigcc-amd64"
                mkdir -p "$ROOT/.build"
                printf '#!/bin/sh\nexec "%s" cc -target x86_64-linux-musl "$@"\n' "$zig_bin" > "$linker"
                chmod +x "$linker"
                compiler=$linker
                archiver="$ROOT/.build/zigar-amd64"
                printf '#!/bin/sh\nexec "%s" ar "$@"\n' "$zig_bin" > "$archiver"
                chmod +x "$archiver"
            elif command -v musl-gcc >/dev/null 2>&1; then
                linker=$(command -v musl-gcc)
                compiler=$linker
            else
                die "no linker for $target: install zig or musl-gcc, or use --provider container"
            fi
            ;;
        aarch64-unknown-linux-musl)
            if [[ -n "$zig_bin" ]]; then
                linker="$ROOT/.build/zigcc-arm64"
                mkdir -p "$ROOT/.build"
                printf '#!/bin/sh\nexec "%s" cc -target aarch64-linux-musl "$@"\n' "$zig_bin" > "$linker"
                chmod +x "$linker"
                compiler=$linker
                archiver="$ROOT/.build/zigar-arm64"
                printf '#!/bin/sh\nexec "%s" ar "$@"\n' "$zig_bin" > "$archiver"
                chmod +x "$archiver"
            elif command -v aarch64-linux-musl-gcc >/dev/null 2>&1; then
                linker=$(command -v aarch64-linux-musl-gcc)
                compiler=$linker
            else
                die "no linker for $target: install zig or aarch64-linux-musl-gcc, or use --provider container"
            fi
            ;;
    esac

    if command -v rustup >/dev/null 2>&1; then
        rustup target add "$target" >/dev/null 2>&1 || true
    fi

    local triple_upper="${target//-/_}"
    triple_upper="${triple_upper^^}"
    local env_args=(
        "CARGO_TARGET_${triple_upper}_LINKER=$linker"
        "CC_${target//-/_}=$compiler"
    )
    if [[ -n "$archiver" ]]; then
        env_args+=("AR_${target//-/_}=$archiver")
    fi
    env "${env_args[@]}" cargo build --release --target "$target" --locked --jobs "$JOBS"
}

# ---------------------------------------------------------------------------
# Build + verify
# ---------------------------------------------------------------------------
command -v file >/dev/null 2>&1 || die "the 'file' utility is required"

case "$PROVIDER" in
    mise) prepare_mise ;;
    container) prepare_container ;;
    system) prepare_system ;;
esac

declare -a BUILT_ARCHES=()
declare -a BUILT_BINS=()

for target in "${TARGETS[@]}"; do
    arch=$(arch_for "$target")
    bin="$ROOT/target/$target/release/smtpmaster"
    case "$PROVIDER" in
        mise) build_mise "$target" ;;
        container) build_container "$target" "$arch"; bin="$ROOT/.build/container-$arch/$target/release/smtpmaster" ;;
        system) build_system "$target" ;;
    esac
    [[ -f "$bin" ]] || die "expected binary not found: $bin"

    description=$(file -b "$bin")
    if [[ "$description" != *"statically linked"* ]]; then
        die "$target artifact is not statically linked: $description"
    fi
    size=$(du -h "$bin" | cut -f1)
    info "$target -> $size (static)"

    BUILT_ARCHES+=("$arch")
    BUILT_BINS+=("$bin")
done

# ---------------------------------------------------------------------------
# Package
# ---------------------------------------------------------------------------
info "packaging into $OUTPUT_DIR"
rm -rf "$OUTPUT_DIR"
mkdir -p "$OUTPUT_DIR"
STAGE="$OUTPUT_DIR/.stage"

for index in "${!BUILT_ARCHES[@]}"; do
    arch="${BUILT_ARCHES[$index]}"
    bin="${BUILT_BINS[$index]}"
    name="smtpmaster-${VERSION}-linux-musl-${arch}"
    mkdir -p "$STAGE/$name"
    cp "$bin" "$STAGE/$name/smtpmaster"
    cp README.md README.vi LICENSE config.example.toml "$STAGE/$name/"
    chmod +x "$STAGE/$name/smtpmaster"
    tar -czf "$OUTPUT_DIR/$name.tar.gz" -C "$STAGE/$name" \
        smtpmaster README.md README.vi LICENSE config.example.toml
    info "wrote outputs/$name.tar.gz"
done

if [[ "$KEEP_STAGE" == 1 ]]; then
    warn "keeping staging directory $STAGE"
else
    rm -rf "$STAGE"
fi

(
    cd "$OUTPUT_DIR"
    sha256sum ./*.tar.gz > SHA256SUMS
)

detect_repo_url() {
    if [[ -n "${REPO_URL:-}" ]]; then
        printf '%s\n' "${REPO_URL%/}"
        return
    fi
    local url=""
    if git rev-parse --git-dir >/dev/null 2>&1; then
        url=$(git remote get-url origin 2>/dev/null || true)
    fi
    if [[ -z "$url" ]]; then
        printf '%s\n' "https://github.com/OWNER/smtpmaster"
        return
    fi
    url="${url%.git}"
    case "$url" in
        git@github.com:*) url="https://github.com/${url#git@github.com:}" ;;
        ssh://git@github.com/*) url="https://github.com/${url#ssh://git@github.com/}" ;;
    esac
    printf '%s\n' "${url%/}"
}
REPO=$(detect_repo_url)

cat > "$OUTPUT_DIR/mise-tools.toml" <<EOF
# Install smtpmaster with mise (https://mise.jdx.dev). Copy this file to
# mise.toml in an empty directory, then run:
#
#   mise install
#   mise exec -- smtpmaster --version
#
# The URL template picks the matching amd64/arm64 musl artifact.
[tools]
"http:smtpmaster" = { version = "$VERSION", url = '$REPO/releases/download/v{{ version }}/smtpmaster-{{ version }}-linux-musl-{{ arch(x64="amd64") }}.tar.gz' }
EOF

cat > "$OUTPUT_DIR/release-notes.md" <<EOF
# smtpmaster v$VERSION

Static musl builds for Linux amd64 and arm64.

## Install

\`\`\`sh
mise use -g github:manhavn/smtpmaster
smtpmaster --version
\`\`\`

## Artifacts

$(cd "$OUTPUT_DIR" && sed 's/^/    /' SHA256SUMS)

## Install with mise

\`\`\`toml
[tools]
"http:smtpmaster" = { version = "$VERSION", url = '$REPO/releases/download/v{{ version }}/smtpmaster-{{ version }}-linux-musl-{{ arch(x64="amd64") }}.tar.gz' }
\`\`\`

## Manual upload

\`\`\`sh
gh release create v$VERSION \\
  ./*.tar.gz SHA256SUMS mise-tools.toml \\
  --title "smtpmaster v$VERSION" --notes-file release-notes.md
\`\`\`
EOF

info "done"
printf '\n'
ls -1 "$OUTPUT_DIR"
printf '\n%sNext:%s upload the files in %s to a GitHub release, e.g.\n' "$C_GREEN" "$C_RESET" "$OUTPUT_DIR"
printf '  gh release create v%s outputs/*.tar.gz outputs/SHA256SUMS outputs/mise-tools.toml \\\n' "$VERSION"
printf '    --title "smtpmaster v%s" --notes-file outputs/release-notes.md\n' "$VERSION"
