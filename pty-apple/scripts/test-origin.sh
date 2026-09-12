#!/usr/bin/env bash
# Exercise the real shared Swift sources without building or launching an app.
set -euo pipefail
root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
build_dir="$(mktemp -d "${TMPDIR:-/tmp}/flowsplice-origin-tests.XXXXXX")"
trap 'rm -rf -- "${build_dir}"' EXIT
# These tests exercise pure helpers in the real host source. Any accidental native
# call must fail loudly; no installation, credentials, or network are initialized.
cat > "${build_dir}/native-stubs.c" <<'EOF'
#include <stdlib.h>
#include "Bridge.h"
char *flowsplice_pty_open(const char *options) { abort(); }
char *flowsplice_pty_send(uint64_t handle, const char *action) { abort(); }
char *flowsplice_pty_poll(uint64_t handle) { abort(); }
char *flowsplice_pty_close(uint64_t handle) { abort(); }
void flowsplice_pty_string_free(char *value) { abort(); }
EOF
xcrun clang -I "${root}/Sources" -c "${build_dir}/native-stubs.c" -o "${build_dir}/native-stubs.o"
for optimization in Onone O; do
  xcrun swiftc "-${optimization}" -module-cache-path "${build_dir}/module-cache" \
    "${root}/Sources/LocalOrigin.swift" "${root}/Sources/WorkspaceStore.swift" \
    "${root}/Tests/OriginTests.swift" -o "${build_dir}/origin-tests-${optimization}"
  "${build_dir}/origin-tests-${optimization}"
  xcrun swiftc "-${optimization}" -module-cache-path "${build_dir}/module-cache" \
    -import-objc-header "${root}/Sources/Bridge.h" \
    -framework Foundation -framework WebKit -framework Security -framework Combine -framework AppKit \
    "${root}/Sources/TerminalHost.swift" "${root}/Sources/LocalOrigin.swift" \
    "${root}/Sources/WorkspaceStore.swift" "${root}/Tests/NativeLifecycleTests.swift" \
    "${build_dir}/native-stubs.o" -o "${build_dir}/native-lifecycle-tests-${optimization}"
  "${build_dir}/native-lifecycle-tests-${optimization}"
done
