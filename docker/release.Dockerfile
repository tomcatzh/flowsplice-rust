# syntax=docker/dockerfile:1@sha256:ecfaec9ed6d810b56388c508f4121597bfbba70d41a6dfeee4d8cad5f295fc32
FROM node:24-alpine@sha256:d32cdf619f63fe0471182d08996dd516c6275bb5fd31ae06e55a570bd9e1ad43 AS travel-web
WORKDIR /src/travelagent/web
COPY travelagent/web/package.json travelagent/web/package-lock.json ./
RUN --mount=type=cache,id=flowsplice-release-travel-npm,target=/root/.npm npm ci
COPY travelagent/web/ ./
COPY tools/precompress.mjs /src/tools/precompress.mjs
RUN npm run build

FROM node:24-alpine@sha256:d32cdf619f63fe0471182d08996dd516c6275bb5fd31ae06e55a570bd9e1ad43 AS home-web
WORKDIR /src/homeagent/web
COPY homeagent/web/package.json homeagent/web/package-lock.json ./
RUN --mount=type=cache,id=flowsplice-release-home-npm,target=/root/.npm npm ci
COPY homeagent/web/ ./
COPY tools/precompress.mjs /src/tools/precompress.mjs
RUN npm run build

FROM rust:1.97-alpine@sha256:3c38f3f82c2f3d73da3b38e18d279393a04cb43ddded0e35088a8c3324d40900 AS build
ARG RUST_TARGET
ENV RUSTUP_TOOLCHAIN=1.97.1
RUN apk add --no-cache clang cmake make musl-dev perl file
RUN rustup target add "${RUST_TARGET}"
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
ARG FLOWSPLICE_DEPLOYMENT_ROOT_PUBLIC_KEY
ARG FLOWSPLICE_MANAGEMENT_CA_CERTIFICATE_PEM
ARG FLOWSPLICE_BOOTSTRAP_RELAYS
ARG FLOWSPLICE_SERVER_ID
ARG FLOWSPLICE_SERVER_NAME
ARG FLOWSPLICE_SERVER_CONTROL_PORT
ARG FLOWSPLICE_HOME_UI_PORT
ENV FLOWSPLICE_DEPLOYMENT_ROOT_PUBLIC_KEY=${FLOWSPLICE_DEPLOYMENT_ROOT_PUBLIC_KEY}
ENV FLOWSPLICE_MANAGEMENT_CA_CERTIFICATE_PEM=${FLOWSPLICE_MANAGEMENT_CA_CERTIFICATE_PEM}
ENV FLOWSPLICE_BOOTSTRAP_RELAYS=${FLOWSPLICE_BOOTSTRAP_RELAYS}
ENV FLOWSPLICE_SERVER_ID=${FLOWSPLICE_SERVER_ID}
ENV FLOWSPLICE_SERVER_NAME=${FLOWSPLICE_SERVER_NAME}
ENV FLOWSPLICE_SERVER_CONTROL_PORT=${FLOWSPLICE_SERVER_CONTROL_PORT}
ENV FLOWSPLICE_HOME_UI_PORT=${FLOWSPLICE_HOME_UI_PORT}
RUN --mount=type=cache,id=flowsplice-release-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=flowsplice-release-cargo-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=flowsplice-release-cargo-target-${RUST_TARGET},target=/src/target,sharing=locked \
    cargo build --locked --release --target "${RUST_TARGET}" \
    -p flowsplice-server \
    -p flowsplice-relay \
    -p flowsplice-homeagent \
    -p flowsplice-travelagent \
    -p flowsplice-foobar && \
    mkdir /out && \
    cp "target/${RUST_TARGET}/release/flowsplice-server" /out/ && \
    cp "target/${RUST_TARGET}/release/flowsplice-relay" /out/ && \
    cp "target/${RUST_TARGET}/release/flowsplice-homeagent" /out/ && \
    cp "target/${RUST_TARGET}/release/flowsplice-travelagent" /out/ && \
    cp "target/${RUST_TARGET}/release/flowsplice-foobar" /out/ && \
    file /out/*

FROM scratch
COPY --from=build /out/ /
