#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
dist_dir="${repo_root}/dist"
mkdir -p "${dist_dir}/macos-arm64"

(cd "${repo_root}/travelagent/web" && npm ci && npm run build)
(cd "${repo_root}" && cargo build --locked --release \
  -p flowsplice-server -p flowsplice-relay -p flowsplice-homeagent -p flowsplice-travelagent \
  -p flowsplice-foobar)
for binary in flowsplice-server flowsplice-relay flowsplice-homeagent flowsplice-travelagent flowsplice-foobar; do
  cp "${repo_root}/target/release/${binary}" "${dist_dir}/macos-arm64/${binary}"
done

for spec in "amd64:x86_64-unknown-linux-musl" "arm64:aarch64-unknown-linux-musl"; do
  arch="${spec%%:*}"
  target="${spec#*:}"
  mkdir -p "${dist_dir}/linux-${arch}"
  docker buildx build \
    --platform "linux/${arch}" \
    --build-arg "RUST_TARGET=${target}" \
    --file "${repo_root}/docker/release.Dockerfile" \
    --output "type=local,dest=${dist_dir}/linux-${arch}" \
    "${repo_root}"
done

printf 'Release artifacts written below %s\n' "${dist_dir}"
