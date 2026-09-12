# syntax=docker/dockerfile:1@sha256:ecfaec9ed6d810b56388c508f4121597bfbba70d41a6dfeee4d8cad5f295fc32
FROM rust:1.97-alpine@sha256:3c38f3f82c2f3d73da3b38e18d279393a04cb43ddded0e35088a8c3324d40900 AS build
ENV RUSTUP_TOOLCHAIN=1.97.1
ARG TARGETARCH
ARG RUST_MIRROR_URL=off
RUN test "$RUST_MIRROR_URL" = off && apk add --no-cache clang cmake make musl-dev perl python3 g++
WORKDIR /src
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates/ crates/
COPY internal/ internal/
COPY pty/ pty/
COPY travel-android/rust/ travel-android/rust/
COPY travel-apple/rust/ travel-apple/rust/
COPY server/ server/
COPY relay/ relay/
COPY homeagent/ homeagent/
COPY travelagent/ travelagent/
COPY foobar/ foobar/
COPY tests/fixtures/echo/ tests/fixtures/echo/
RUN --mount=type=cache,id=flowsplice-pty-tests-registry-${TARGETARCH},target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=flowsplice-pty-tests-target-${TARGETARCH},target=/src/target,sharing=locked \
    (cargo test --locked --no-run --message-format=json -p flowsplice-pty-codec -p flowsplice-pty-home -p flowsplice-pty-client -p flowsplice-pty-protocol -p flowsplice-transport > /tmp/test-artifacts.json || \
      { python3 -c 'import json; rows=[json.loads(line) for line in open("/tmp/test-artifacts.json")]; [print(row["message"].get("rendered", "")) for row in rows if row.get("reason")=="compiler-message"]'; exit 1; }) && \
    mkdir /tests && \
    python3 -c 'import json,shutil; rows=[json.loads(line) for line in open("/tmp/test-artifacts.json")]; artifacts=[row for row in rows if row.get("reason")=="compiler-artifact" and row.get("profile",{}).get("test") and row.get("executable")]; assert artifacts; [shutil.copy2(row["executable"], "/tests/"+row["target"]["name"]+"-"+row["target"]["kind"][0]) for row in artifacts]'
FROM alpine:3.23@sha256:fd791d74b68913cbb027c6546007b3f0d3bc45125f797758156952bc2d6daf40
RUN apk add --no-cache tmux libgcc && addgroup -S tests && adduser -S -s /bin/sh -G tests tests
COPY --from=build /tests /tests
USER tests
CMD ["sh", "-ec", "uname -m; tmux -V; status=0; for test in /tests/*; do echo TEST_EXECUTABLE=$test; if timeout 120 \"$test\" --test-threads=1 --nocapture; then :; else status=1; fi; done; exit $status"]
