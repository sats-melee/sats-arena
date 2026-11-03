# syntax=docker/dockerfile:1
FROM node:22-bookworm-slim AS frontend
WORKDIR /build/frontend
COPY server/frontend/package.json server/frontend/package-lock.json ./
RUN npm ci
COPY server/frontend/ ./
RUN npm run build

FROM rust:1.88.0-bookworm AS backend
ENV CARGO_BUILD_JOBS=2 CARGO_PROFILE_RELEASE_STRIP=symbols
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY protocol/ ./protocol/
COPY core/ ./core/
COPY local/ ./local/
COPY server/Cargo.toml ./server/Cargo.toml
COPY server/src/ ./server/src/
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    cargo build --locked --release -p sats-arena-server \
    && cp target/release/sats-arena-server /usr/local/bin/sats-arena-server

FROM debian:bookworm-slim AS runtime
WORKDIR /app
COPY --from=backend /usr/local/bin/sats-arena-server /usr/local/bin/sats-arena-server
COPY --from=frontend /build/frontend/dist ./frontend
USER 10001:10001
EXPOSE 8080
STOPSIGNAL SIGINT
ENTRYPOINT ["/usr/local/bin/sats-arena-server", "rooms"]
CMD ["--bind", "0.0.0.0:8080", "--frontend", "/app/frontend"]
