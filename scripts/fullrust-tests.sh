#!/usr/bin/env bash
# Build dldota's tests as fully static, libc-free binaries with fullrust
# (https://github.com/KarpelesLab/fullrust), then run the same binaries against
# each kind of system loader: the host's own, and Alpine's (musl) in Docker.
#
# Uses a local `fullrust-$FULLRUST_VERSION` rustup toolchain when installed
# (unless FULLRUST_DOCKER=1), otherwise the fullrust Docker image. Arguments are passed to the test
# binaries, e.g. `scripts/fullrust-tests.sh --include-ignored`.
set -euo pipefail
cd "$(dirname "$0")/.."

FULLRUST_VERSION="${FULLRUST_VERSION:-1.99}"
ALPINE_IMAGE="${ALPINE_IMAGE:-alpine:3.22}"
# Doc-tests are skipped: the fullrust toolchain ships no rustdoc.
build_args=(test --no-run --lib --tests --message-format=json-render-diagnostics)

if [ "${FULLRUST_DOCKER:-0}" != 1 ] && rustup toolchain list 2>/dev/null | grep -q "^fullrust-$FULLRUST_VERSION"; then
    json=$(cargo "+fullrust-$FULLRUST_VERSION" "${build_args[@]}" --target x86_64-unknown-linux-fullrust)
    target_dir=$(cargo metadata --format-version 1 --no-deps | jq -r .target_directory)
    map=(cat)
else
    # Build as the calling user so target/ stays ours; the image's CARGO_HOME
    # (which carries the ecosystem [patch]) is only read.
    json=$(docker run --rm --user "$(id -u):$(id -g)" -v "$PWD":/src -w /src \
        -e CARGO_TARGET_DIR=/src/target \
        "ghcr.io/karpeleslab/fullrust:$FULLRUST_VERSION" "${build_args[@]}")
    target_dir="$PWD/target"
    map=(sed "s|^/src/target|$target_dir|")
fi

mapfile -t bins < <(jq -r 'select(.reason == "compiler-artifact" and .profile.test) | .executable // empty' <<<"$json" | "${map[@]}")
[ ${#bins[@]} -gt 0 ] || { echo "no test binaries built" >&2; exit 1; }

status=0
for bin in "${bins[@]}"; do
    echo "=== $(basename "$bin") on host ($(sed -n 's/^PRETTY_NAME=//p' /etc/os-release))"
    "$bin" "$@" || status=1
    echo "=== $(basename "$bin") on $ALPINE_IMAGE"
    docker run --rm -v "$target_dir":"$target_dir":ro "$ALPINE_IMAGE" "$bin" "$@" || status=1
done
exit $status
