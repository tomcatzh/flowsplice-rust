#!/usr/bin/env bash
set -euo pipefail

repository_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
stage="$(mktemp -d "${TMPDIR:-/private/tmp}/flowsplice-review-fixtures.XXXXXX")"
trap 'rm -rf -- "$stage"' EXIT

for fixture in legit rogue; do
  fixture_root="${stage}/${fixture}"
  mkdir -p "${fixture_root}"
  cp "${repository_root}/tests/e2e/generate-certs.sh" "${fixture_root}/"
  cp "${repository_root}/tests/e2e/authority-public-key.py" "${fixture_root}/"
  cp "${repository_root}/tests/e2e/generate-deployment-trust.py" "${fixture_root}/"
  cp -R "${repository_root}/tests/e2e/config" "${fixture_root}/config"
  bash "${fixture_root}/generate-certs.sh"
done

export FLOWSPLICE_REVIEW_FIXTURES="${stage}"
if [[ -z "${CARGO_TARGET_DIR:-}" ]]; then
  export CARGO_TARGET_DIR="${stage}/cargo-target"
fi
cargo test -p flowsplice-travel-core review_regressions -- --ignored --test-threads=1
