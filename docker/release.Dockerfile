FROM node:24-alpine AS web
WORKDIR /src/travelagent/web
COPY travelagent/web/package.json travelagent/web/package-lock.json ./
RUN npm ci
COPY travelagent/web/ ./
COPY tools/precompress.mjs /src/tools/precompress.mjs
RUN npm run build

FROM rust:alpine AS build
ARG RUST_TARGET
RUN apk add --no-cache clang cmake make musl-dev perl file
RUN rustup target add "${RUST_TARGET}"
WORKDIR /src
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates/ crates/
COPY server/ server/
COPY relay/ relay/
COPY homeagent/ homeagent/
COPY travelagent/ travelagent/
COPY issuer/ issuer/
COPY foobar/ foobar/
COPY tests/fixtures/echo/ tests/fixtures/echo/
COPY --from=web /src/travelagent/web/dist/ travelagent/web/dist/
RUN cargo build --locked --release --target "${RUST_TARGET}" \
    -p flowsplice-server \
    -p flowsplice-relay \
    -p flowsplice-homeagent \
    -p flowsplice-travelagent \
    -p flowsplice-issuer \
    -p flowsplice-foobar
RUN mkdir /out && \
    cp "target/${RUST_TARGET}/release/flowsplice-server" /out/ && \
    cp "target/${RUST_TARGET}/release/flowsplice-relay" /out/ && \
    cp "target/${RUST_TARGET}/release/flowsplice-homeagent" /out/ && \
    cp "target/${RUST_TARGET}/release/flowsplice-travelagent" /out/ && \
    cp "target/${RUST_TARGET}/release/flowsplice-issuer" /out/ && \
    cp "target/${RUST_TARGET}/release/flowsplice-foobar" /out/ && \
    file /out/*

FROM scratch
COPY --from=build /out/ /
