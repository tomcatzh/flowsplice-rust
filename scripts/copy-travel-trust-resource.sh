#!/usr/bin/env bash
set -euo pipefail
repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
: "${TARGET_BUILD_DIR:?Xcode target build directory is required}"
: "${UNLOCALIZED_RESOURCES_FOLDER_PATH:?Xcode resource directory is required}"
resource="${TARGET_BUILD_DIR}/${UNLOCALIZED_RESOURCES_FOLDER_PATH}/bootstrap/deployment-root.pub"
if [[ -z "${FLOWSPLICE_PRIVATE_TRUST_FILE:-}" ]]; then
  # Always run, including private A -> private B -> deployment-neutral rebuilds.
  if [[ -L "$(dirname -- "${resource}")" || -L "${resource}" ]]; then
    printf 'Refusing a symbolic link in generated deployment trust resources.\n' >&2
    exit 1
  fi
  rm -f -- "${resource}"
  exit 0
fi
python3 "${repo_root}/scripts/private-travel-trust.py" copy-root \
  --source "${FLOWSPLICE_PRIVATE_TRUST_FILE}" --destination "${resource}"
