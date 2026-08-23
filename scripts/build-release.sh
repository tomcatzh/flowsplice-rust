#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
dist_dir="${repo_root}/dist"
docker_pull="${FLOWSPLICE_DOCKER_PULL:-false}"
if [[ "${docker_pull}" != "false" && "${docker_pull}" != "true" ]]; then
  printf 'FLOWSPLICE_DOCKER_PULL must be true or false.\n' >&2
  exit 1
fi
rust_mirror_url="${RUST_MIRROR_URL-http://host.docker.internal:18787}"
rustup_dist_server=''
rustup_update_root=''
# Set RUST_MIRROR_URL=off (or an empty value) to use the official upstream servers.
if [[ -n "${rust_mirror_url}" && "${rust_mirror_url}" != "off" ]]; then
  rustup_dist_server="${rust_mirror_url%/}"
  rustup_update_root="${rustup_dist_server}/rustup"
fi
mkdir -p "${dist_dir}/macos-arm64"

(cd "${repo_root}/travelagent/web" && npm ci && npm run build)
(cd "${repo_root}/homeagent/web" && npm ci && npm run build)
(cd "${repo_root}" && cargo build --locked --release \
  -p flowsplice-server -p flowsplice-relay -p flowsplice-homeagent -p flowsplice-travelagent \
  -p flowsplice-foobar)
(cd "${repo_root}" && cargo build --locked --release \
  -p flowsplice-enrollment --bin flowsplice-trust)
bash "${repo_root}/tests/check-release-feature-gates.sh" \
  --home "${repo_root}/target/release/flowsplice-homeagent" \
  --travel "${repo_root}/target/release/flowsplice-travelagent"
for binary in flowsplice-server flowsplice-relay flowsplice-homeagent flowsplice-travelagent flowsplice-foobar flowsplice-trust; do
  cp "${repo_root}/target/release/${binary}" "${dist_dir}/macos-arm64/${binary}"
done

if ! command -v codesign >/dev/null 2>&1; then
  printf 'codesign is required to produce the macOS release artifacts.\n' >&2
  exit 1
fi
for binary in flowsplice-server flowsplice-relay flowsplice-homeagent flowsplice-travelagent flowsplice-foobar flowsplice-trust; do
  identifier="io.zxf.flowsplice.${binary#flowsplice-}"
  artifact="${dist_dir}/macos-arm64/${binary}"
  codesign \
    --force \
    --sign - \
    --identifier "${identifier}" \
    --options runtime \
    --timestamp=none \
    "${artifact}"
  codesign --verify --strict --verbose=2 "${artifact}"
done

for spec in "amd64:x86_64-unknown-linux-musl" "arm64:aarch64-unknown-linux-musl"; do
  arch="${spec%%:*}"
  target="${spec#*:}"
  mkdir -p "${dist_dir}/linux-${arch}"
  docker buildx build \
    --pull="${docker_pull}" \
    --platform "linux/${arch}" \
    --build-arg "RUST_TARGET=${target}" \
    --build-arg "RUST_MIRROR_URL=${rust_mirror_url}" \
    --build-arg "RUSTUP_DIST_SERVER=${rustup_dist_server}" \
    --build-arg "RUSTUP_UPDATE_ROOT=${rustup_update_root}" \
    --file "${repo_root}/docker/release.Dockerfile" \
    --output "type=local,dest=${dist_dir}/linux-${arch}" \
    "${repo_root}"
done

printf 'Release artifacts written below %s\n' "${dist_dir}"
printf 'macOS artifacts use ad-hoc signatures only; they are not Developer ID signed or notarized.\n'
