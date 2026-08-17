FROM node:24-alpine AS web
WORKDIR /src/travelagent/web
COPY travelagent/web/package.json travelagent/web/package-lock.json ./
RUN npm ci
COPY travelagent/web/ ./
COPY tools/precompress.mjs /src/tools/precompress.mjs
RUN npm run build

FROM rust:alpine AS build
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
COPY --from=web /src/travelagent/web/dist/ travelagent/web/dist/
RUN cargo build --release \
    -p flowsplice-server \
    -p flowsplice-relay \
    -p flowsplice-homeagent \
    -p flowsplice-travelagent \
    -p flowsplice-echo

FROM alpine:3.23
RUN addgroup -S flowsplice && adduser -S -G flowsplice flowsplice
COPY --from=build /src/target/release/flowsplice-server /usr/local/bin/
COPY --from=build /src/target/release/flowsplice-relay /usr/local/bin/
COPY --from=build /src/target/release/flowsplice-homeagent /usr/local/bin/
COPY --from=build /src/target/release/flowsplice-travelagent /usr/local/bin/
COPY --from=build /src/target/release/flowsplice-echo /usr/local/bin/
USER flowsplice
