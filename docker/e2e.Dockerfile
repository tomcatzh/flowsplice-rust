# syntax=docker/dockerfile:1@sha256:ecfaec9ed6d810b56388c508f4121597bfbba70d41a6dfeee4d8cad5f295fc32
FROM node:24-alpine@sha256:d32cdf619f63fe0471182d08996dd516c6275bb5fd31ae06e55a570bd9e1ad43 AS travel-web
WORKDIR /src/travelagent/web
COPY travelagent/web/package.json travelagent/web/package-lock.json ./
RUN --mount=type=cache,id=flowsplice-e2e-travel-npm,target=/root/.npm npm ci
COPY travelagent/web/ ./
COPY tools/precompress.mjs /src/tools/precompress.mjs
RUN npm run build

FROM node:24-alpine@sha256:d32cdf619f63fe0471182d08996dd516c6275bb5fd31ae06e55a570bd9e1ad43 AS home-web
WORKDIR /src/homeagent/web
COPY homeagent/web/package.json homeagent/web/package-lock.json ./
RUN --mount=type=cache,id=flowsplice-e2e-home-npm,target=/root/.npm npm ci
COPY homeagent/web/ ./
COPY tools/precompress.mjs /src/tools/precompress.mjs
RUN npm run build

FROM rust:1.97-alpine@sha256:3c38f3f82c2f3d73da3b38e18d279393a04cb43ddded0e35088a8c3324d40900 AS build
ENV RUSTUP_TOOLCHAIN=1.97.1
RUN apk add --no-cache clang cmake make musl-dev perl
WORKDIR /src
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates/ crates/
COPY server/ server/
COPY relay/ relay/
COPY homeagent/ homeagent/
COPY travelagent/ travelagent/
COPY foobar/ foobar/
COPY tests/fixtures/echo/ tests/fixtures/echo/
COPY --from=travel-web /src/travelagent/web/dist/ travelagent/web/dist/
COPY --from=home-web /src/homeagent/web/dist/ homeagent/web/dist/
RUN --mount=type=cache,id=flowsplice-e2e-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=flowsplice-e2e-cargo-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=flowsplice-e2e-cargo-target,target=/src/target,sharing=locked \
    cargo build --locked --release --bins \
    --features flowsplice-homeagent/e2e-remote-ui,flowsplice-travelagent/e2e-remote-ui \
    -p flowsplice-server \
    -p flowsplice-relay \
    -p flowsplice-homeagent \
    -p flowsplice-travelagent \
    -p flowsplice-echo && \
    mkdir /out && \
    cp target/release/flowsplice-server /out/ && \
    cp target/release/flowsplice-relay /out/ && \
    cp target/release/flowsplice-homeagent /out/ && \
    cp target/release/flowsplice-travelagent /out/ && \
    cp target/release/flowsplice-echo /out/ && \
    cp target/release/travel-login-probe /out/

FROM alpine:3.23@sha256:fd791d74b68913cbb027c6546007b3f0d3bc45125f797758156952bc2d6daf40
RUN addgroup -S flowsplice && adduser -S -G flowsplice flowsplice
COPY --from=build /out/flowsplice-server /usr/local/bin/
COPY --from=build /out/flowsplice-relay /usr/local/bin/
COPY --from=build /out/flowsplice-homeagent /usr/local/bin/
COPY --from=build /out/flowsplice-travelagent /usr/local/bin/
COPY --from=build /out/flowsplice-echo /usr/local/bin/
COPY --from=build /out/travel-login-probe /usr/local/bin/flowsplice-travel-login-probe
USER flowsplice
