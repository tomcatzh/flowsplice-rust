#!/usr/bin/env bash
# Standalone real-tmux regressions; never joins or changes the E2E topology.
set -euo pipefail
repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)"
log_root="${FLOWSPLICE_PTY_TEST_LOG_DIR:-/tmp/flowsplice-pty-platform-tests-$(date +%Y%m%d-%H%M%S)-$$}"
mkdir -p "$log_root"
export RUST_MIRROR_URL=off
status=0
for architecture in arm64 amd64; do
    image="flowsplice-pty-tests-${architecture}:local-$$"
    printf 'platform=linux/%s\n' "$architecture" | tee "$log_root/$architecture-tests.log"
    if docker build --platform "linux/$architecture" --progress plain --build-arg RUST_MIRROR_URL=off \
        -f "$repo_root/docker/pty-tests.Dockerfile" -t "$image" "$repo_root" > "$log_root/$architecture-build.log" 2>&1; then
    if ! docker run --rm --network none --platform "linux/$architecture" "$image" 2>&1 | tee -a "$log_root/$architecture-tests.log"; then status=1; fi
    else status=1; fi
done
printf 'PTY platform logs: %s\n' "$log_root"
exit "$status"
