# crust-seed — multi-stage Docker build
#
# Stage 1: webui    — Node builds the vendored React SPA into web/webui/dist
# Stage 2: planner  — cargo-chef recipe.json from Cargo.toml + Cargo.lock + src
# Stage 3: builder  — cooks deps from the recipe (cached), then builds the binary
#                     with the SPA baked in via rust-embed
# Stage 4: runtime  — distroless final image
#
# Why cargo-chef:
#   The dummy-main.rs trick re-runs ALL deps on any source change because the
#   "src" copy invalidates the layer that triggered the dep build. cargo-chef
#   separates the dep graph (recipe.json — a function of Cargo.toml +
#   Cargo.lock only) from the source, so changing src/ never rebuilds deps.
#
# Why cache mounts:
#   The Cargo registry and the build target/ live in BuildKit cache mounts that
#   survive between builds AND between layers.
#
# Difference from a plain Rust service: crust-seed serves a React web UI. The
# SPA is compiled by the Node stage and embedded into the binary (rust-embed),
# so the runtime image stays a single static-ish binary on distroless — no
# Node, no asset directory, no static-file mount.

# ─── Stage 1: web UI ─────────────────────────────────────────────────────────
FROM node:26-alpine AS webui
WORKDIR /web
ENV NPM_CONFIG_UPDATE_NOTIFIER=false
COPY web/package.json web/package-lock.json ./
COPY web/shared/package.json ./shared/
COPY web/api-types/package.json ./api-types/
COPY web/webui/package.json ./webui/
RUN npm ci --no-fund --no-audit
COPY web .
RUN npm run build

# ─── Stage 2: planner ────────────────────────────────────────────────────────
FROM lukemathwalker/cargo-chef:latest-rust-1.95-slim-bookworm AS planner
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY build.rs ./
RUN cargo chef prepare --recipe-path recipe.json

# ─── Stage 3: builder ────────────────────────────────────────────────────────
FROM lukemathwalker/cargo-chef:latest-rust-1.95-slim-bookworm AS builder
WORKDIR /build

RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config \
    ca-certificates \
    cmake \
    clang \
    libclang-dev \
    g++ \
    make \
    perl \
    git \
    && rm -rf /var/lib/apt/lists/*

# Cook deps only — depends on recipe.json, a deterministic function of
# Cargo.toml + Cargo.lock. Cached as long as those two files don't change.
COPY --from=planner /build/recipe.json recipe.json
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/build/target,sharing=locked \
    cargo chef cook --release --recipe-path recipe.json

# Build info surfaced by `crust-seed --version` and the web UI's about page.
ARG BUILD_COMMIT_SHA=""
ARG BUILD_BRANCH=""
ARG BUILD_VERSION=""
ENV BUILD_COMMIT_SHA=$BUILD_COMMIT_SHA \
    BUILD_BRANCH=$BUILD_BRANCH \
    BUILD_VERSION=$BUILD_VERSION

COPY Cargo.toml Cargo.lock build.rs ./
COPY src ./src
COPY migrations ./migrations
# The compiled SPA must be in place BEFORE cargo build: rust-embed reads it at
# compile time.
COPY --from=webui /web/webui/dist ./web/webui/dist
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/build/target,sharing=locked \
    cargo build --release --bin crust-seed && \
    cp target/release/crust-seed /usr/local/bin/crust-seed

# ─── Stage 4: distroless runtime ─────────────────────────────────────────────
# gcr.io/distroless/cc-debian12: no shell, no package manager, ~20 MB base.
# Ships ca-certificates (HTTPS to indexers) and zoneinfo (log timestamps).
FROM gcr.io/distroless/cc-debian12
COPY --from=builder /usr/local/bin/crust-seed /usr/local/bin/crust-seed
ENV CONFIG_DIR=/config \
    DOCKER_ENV=true
EXPOSE 2468
WORKDIR /config
# Run as non-root (distroless nonroot UID). The host must chown the volume
# mounted at /config to 65532:65532.
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/crust-seed"]
CMD ["daemon"]
