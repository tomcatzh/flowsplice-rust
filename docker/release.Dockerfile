FROM node:24-alpine AS travel-web
WORKDIR /src/travelagent/web
COPY travelagent/web/package.json travelagent/web/package-lock.json ./
RUN npm ci
COPY travelagent/web/ ./
COPY tools/precompress.mjs /src/tools/precompress.mjs
RUN npm run build

FROM node:24-alpine AS home-web
WORKDIR /src/homeagent/web
COPY homeagent/web/package.json homeagent/web/package-lock.json ./
RUN npm ci
COPY homeagent/web/ ./
COPY tools/precompress.mjs /src/tools/precompress.mjs
RUN npm run build

FROM rust:alpine AS build
ARG RUST_TARGET
ARG FLOWSPLICE_DEPLOYMENT_ROOT_PUBLIC_KEY
ENV FLOWSPLICE_DEPLOYMENT_ROOT_PUBLIC_KEY=${FLOWSPLICE_DEPLOYMENT_ROOT_PUBLIC_KEY}
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
RUN cargo build --locked --release --target "${RUST_TARGET}" \
    -p flowsplice-server \
    -p flowsplice-relay \
    -p flowsplice-homeagent \
    -p flowsplice-travelagent \
    -p flowsplice-foobar
RUN mkdir /out && \
    cp "target/${RUST_TARGET}/release/flowsplice-server" /out/ && \
    cp "target/${RUST_TARGET}/release/flowsplice-relay" /out/ && \
    cp "target/${RUST_TARGET}/release/flowsplice-homeagent" /out/ && \
    cp "target/${RUST_TARGET}/release/flowsplice-travelagent" /out/ && \
    cp "target/${RUST_TARGET}/release/flowsplice-foobar" /out/ && \
    file /out/*

FROM scratch
COPY --from=build /out/ /
