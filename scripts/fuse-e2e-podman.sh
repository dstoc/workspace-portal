#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: scripts/fuse-e2e-podman.sh [-- [extra-test-args...]]

Builds the Podman image in containers/fuse-e2e/ and runs the ignored FUSE
end-to-end suite inside it.

Default cargo invocation:
  cargo test --offline --locked --test fuse_e2e -- --ignored --test-threads=1
EOF
}

if [[ ${1:-} == "-h" || ${1:-} == "--help" ]]; then
  usage
  exit 0
fi

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "$script_dir/.." && pwd)"
containerfile="$repo_root/containers/fuse-e2e/Containerfile"
image_name="${FUSE_E2E_IMAGE:-workspace-portal-fuse-e2e}"
podman="${PODMAN:-podman}"

if ! command -v "$podman" >/dev/null 2>&1; then
  echo "error: $podman is required to run the FUSE E2E harness" >&2
  exit 127
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

pass_args=()
if ((${#extra_test_args[@]})); then
  pass_args=(-- "${extra_test_args[@]}")
fi

"$podman" build -f "$containerfile" -t "$image_name" "$repo_root"

echo "==> Running FUSE E2E suite in Podman"
# Network is disabled in the container, so cargo must run offline against the
# dependencies fetched at image build time. The shared script drives the actual
# cargo invocation; CARGO_NET_OFFLINE replaces the previous hard-coded --offline.
"$podman" run --rm \
  --device /dev/fuse \
  --cap-add SYS_ADMIN \
  --network none \
  -e CARGO_NET_OFFLINE=true \
  -w /workspace \
  "$image_name" \
  bash scripts/fuse-e2e.sh "${pass_args[@]}"
