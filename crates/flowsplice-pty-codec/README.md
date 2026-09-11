# PTY Snappy codec

Official Google Snappy 1.2.2, explicitly Level 1 when `encode` is enabled.
Default builds expose decompression only. This is a raw block codec, not a
transport or framing policy. Snappy has no checksum: syntactically valid literal
corruption requires the application's authenticated transport to detect it.

`vendor/PROVENANCE.json` records the verified upstream archive and individual
unaltered file hashes; `vendor/snappy/COPYING` is the original BSD license.
Only upstream library build sources are vendored. No build downloads occur.

Build prerequisites: CMake, C/C++ toolchains, Rust. Upstream CMake probes use the
same target flags as compilation, with no host-specific ISA overrides. Upstream
and bridge disable exceptions and RTTI; code/data sections permit dead stripping.

- Apple: set `DEVELOPER_DIR` to full Xcode for iOS. Rust consumers of the final
  static library must link system `libc++` (`-lc++`). Both device and simulator
  use the SDK/architecture selected by the Rust target and cc/cmake crates.
- Android: set target-qualified `CC`, `CXX`, and `AR` to NDK wrappers, e.g.
  `CC_aarch64_linux_android=.../aarch64-linux-android34-clang`, the corresponding
  `CXX_aarch64_linux_android=.../aarch64-linux-android34-clang++`, and `llvm-ar`.
  CMake derives NDK root and API from that compiler; probes and build share API.
  Static libc++ and libc++abi follow Snappy in link order. Only those archives
  are exposed to the linker, preserving Android's shared system libc. The app
  has a single Rust JNI shared library, whose build rejects undefined symbols.
- Linux: provide target C/C++ compilers (including g++ in container builders).
  musl targets link static libstdc++; no `-march=native` or AVX requirement.

Run `cargo test -p flowsplice-pty-codec --no-default-features` and
`cargo test -p flowsplice-pty-codec --release --features encode`.
The decoder checks the header's declared length before allocating, enforces the
caller cap, and rejects malformed or trailing data. Allocation failure becomes
an error; compression capacity arithmetic is checked before allocating.
