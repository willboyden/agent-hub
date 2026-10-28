# TEMPLATE image for the containerised agent-knowledge (see docker-compose.broker.yml). Not built by the app or by CI.
# Build from the repo root:  docker build -f deploy/knowledge/Dockerfile -t agent-knowledge:local .
# UNVERIFIED: this file has not been built in this repo's gates (they build no containers). Pin both base images by
# digest for real use, and adapt them to your environment.
#
# Stage 1 builds the release binary. The toolchain version comes from backend/rust-toolchain.toml; protoc for the Lance
# engine's build scripts is the vendored one (the protoc-bin-vendored crate, reached through backend/tools/protoc, which
# backend/.cargo/config.toml sets as PROTOC), so no system package is installed.
FROM rust:1.99.1-slim AS build
WORKDIR /src
COPY backend/ ./backend/
WORKDIR /src/backend
RUN cargo build --locked --release -p agent-knowledge --bin agent-knowledge

# Stage 2: the binary alone (it links only libc, libm and libgcc_s). No interpreter, no virtualenv.
FROM debian:trixie-slim
COPY --from=build /src/backend/target/release/agent-knowledge /usr/local/bin/agent-knowledge
RUN useradd --uid 10001 --no-create-home app && mkdir /data && chown app /data
USER app
EXPOSE 8795
