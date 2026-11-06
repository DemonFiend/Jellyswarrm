#################################
# Stage 1: Build UI (Node.js optimized)
#################################
FROM node:24-alpine AS ui-build

# git is needed both to report the UI commit and to fetch the client when the
# submodule is not present in the build context.
RUN apk add --no-cache git

WORKDIR /app/ui

# The pinned jellyfin-web commit. Must match the `ui` submodule gitlink in this
# repository, which `git ls-tree HEAD ui` prints.
#
# `docker build <git-url>#<branch>` clones the repository without its
# submodules, so `ui/` arrives as an empty directory and there is no
# `.git/modules/ui` to copy. That is how this image is built, so rather than
# failing on a missing package.json, fetch the pinned commit directly. A
# checkout that does have the submodule keeps using it, leaving local builds
# unaffected.
ARG JELLYFIN_WEB_COMMIT=c06cb17b274c10f9015412305edb0afdc010ae7a

COPY ui/ ./

RUN if [ -f package.json ]; then \
      echo "Using the ui submodule from the build context"; \
    else \
      echo "Submodule absent; fetching jellyfin-web@${JELLYFIN_WEB_COMMIT}" && \
      git init -q && \
      git remote add origin https://github.com/jellyfin/jellyfin-web.git && \
      git fetch -q --depth 1 origin "${JELLYFIN_WEB_COMMIT}" && \
      git checkout -q FETCH_HEAD; \
    fi

# Install all dependencies (including dev deps needed for build)
RUN --mount=type=cache,target=/root/.npm \
    npm install --engine-strict=false --ignore-scripts

# Build production UI bundle
RUN npm run build:production

# Write ui-version.env file.
#
# The version comes from package.json, which is the value the web client parses
# and refuses to start without. `git rev-parse` has nothing to read in a
# submodule directory copied without its `.git`, so the commit falls back to the
# pinned one rather than failing the build or recording "unknown".
RUN UI_VERSION=$(node -p "require('./package.json').version") && \
    UI_COMMIT=$(git rev-parse HEAD 2>/dev/null || echo "${JELLYFIN_WEB_COMMIT}") && \
    printf "UI_VERSION=%s\nUI_COMMIT=%s\n" "${UI_VERSION#v}" "$UI_COMMIT" > dist/ui-version.env && \
    echo "Generated dist/ui-version.env: ${UI_VERSION#v} / $UI_COMMIT"



#################################
# Stage 2: Rust Dependencies Cache
#################################
FROM rust:1.92.0-alpine AS rust-deps

WORKDIR /app

# Install build dependencies
RUN apk add --no-cache \
	build-base \
	pkgconf \
	sqlite-dev \
	openssl-dev

# Copy Cargo manifests for dependency caching
COPY .cargo .cargo
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates/jellyswarrm-proxy/Cargo.toml crates/jellyswarrm-proxy/Cargo.toml
COPY crates/jellyswarrm-macros/Cargo.toml crates/jellyswarrm-macros/Cargo.toml
COPY crates/jellyfin-api/Cargo.toml crates/jellyfin-api/Cargo.toml

# Create dummy source files to build dependencies
RUN mkdir -p crates/jellyswarrm-proxy/src crates/jellyswarrm-macros/src crates/jellyfin-api/src \
	&& echo "fn main() {}" > crates/jellyswarrm-proxy/src/main.rs \
	&& echo "" > crates/jellyswarrm-proxy/src/lib.rs \
	&& echo "use proc_macro::TokenStream; #[proc_macro_attribute] pub fn multi_case_struct(_args: TokenStream, input: TokenStream) -> TokenStream { input }" > crates/jellyswarrm-macros/src/lib.rs \
	&& echo "" > crates/jellyfin-api/src/lib.rs

# Build dependencies only (will be cached)
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/tmp/target,sharing=locked \
    CARGO_TARGET_DIR=/tmp/target cargo build --release --bin jellyswarrm-proxy \
	&& cp /tmp/target/release/jellyswarrm-proxy /app/jellyswarrm-proxy-deps \
	&& rm -rf crates/jellyswarrm-proxy/src crates/jellyswarrm-macros/src crates/jellyfin-api/src

#################################
# Stage 3: Build Rust Application
#################################
FROM rust-deps AS rust-build

# Set env var so build.rs skips internal UI build (we already did it)
ENV JELLYSWARRM_SKIP_UI=1

# Copy UI build artifacts from stage 1
COPY --from=ui-build /app/ui/dist crates/jellyswarrm-proxy/static/

# Copy Rust source code and configuration
COPY crates/jellyswarrm-proxy/askama.toml crates/jellyswarrm-proxy/askama.toml
COPY crates/jellyswarrm-proxy/src crates/jellyswarrm-proxy/src
COPY crates/jellyswarrm-proxy/migrations crates/jellyswarrm-proxy/migrations
COPY crates/jellyswarrm-macros/src crates/jellyswarrm-macros/src
COPY crates/jellyfin-api/src crates/jellyfin-api/src

# Build only the application code (dependencies already cached)
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/tmp/target,sharing=locked \
    rm -rf /tmp/target/release/deps/libjellyswarrm_macros* /tmp/target/release/deps/jellyswarrm_macros* \
    && rm -rf /tmp/target/release/deps/libjellyfin_api* \
    && touch crates/jellyswarrm-macros/src/lib.rs \
    && touch crates/jellyfin-api/src/lib.rs \
    && CARGO_TARGET_DIR=/tmp/target cargo build --release --bin jellyswarrm-proxy \
    && cp /tmp/target/release/jellyswarrm-proxy /app/jellyswarrm-proxy

#################################
# Stage 4: Runtime Image (Alpine)
#################################
FROM alpine:3.20 AS runtime

WORKDIR /app

ENV RUST_LOG=info

# Install minimal runtime dependencies
RUN apk add --no-cache \
	ca-certificates \
	curl \
	sqlite-libs \
	&& update-ca-certificates

# Copy the compiled binary
COPY --from=rust-build /app/jellyswarrm-proxy /app/jellyswarrm-proxy

EXPOSE 3000

HEALTHCHECK --interval=30s --timeout=5s --start-period=30s --retries=3 \
    CMD curl --fail --silent --show-error --max-time 1 "http://127.0.0.1:${JELLYSWARRM_PORT:-3000}/health" > /dev/null \
        && curl --fail --silent --show-error --max-time 3 "http://127.0.0.1:${JELLYSWARRM_PORT:-3000}/ready" > /dev/null \
        || exit 1

ENTRYPOINT ["/app/jellyswarrm-proxy"]
