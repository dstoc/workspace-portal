#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: scripts/fuse-e2e.sh [-- [extra-test-args...]]

Runs the ignored FUSE end-to-end suite directly on the host. Requires a real
/dev/fuse and fusermount3 (Debian/Ubuntu: apt-get install fuse3). This is the
shared entry point: CI and local host runs call it directly, and
scripts/fuse-e2e-podman.sh calls it inside the container.

Default cargo invocation:
  cargo test --locked --test fuse_e2e -- --ignored --test-threads=1

Set CARGO_NET_OFFLINE=true to run without network access (used by the Podman
harness, which pre-fetches dependencies at image build time).
EOF
}

if [[ ${1:-} == "-h" || ${1:-} == "--help" ]]; then
  usage
  exit 0
fi

extra_test_args=()
if (($#)); then
  if [[ ${1:-} != "--" ]]; then
    echo "error: unexpected arguments" >&2
    usage
    exit 2
  fi
  shift
  extra_test_args=("$@")
fi

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "$script_dir/.." && pwd)"

if [[ ! -c /dev/fuse ]]; then
  echo "error: /dev/fuse is missing or not a character device; FUSE mounts are unavailable" >&2
  exit 1
fi

if ! command -v fusermount3 >/dev/null 2>&1; then
  echo "error: fusermount3 not found in PATH (Debian/Ubuntu: apt-get install fuse3)" >&2
  exit 1
fi

cd -- "$repo_root"
exec cargo test --locked --test fuse_e2e -- --ignored --test-threads=1 "${extra_test_args[@]}"
