# Stage 1: Build the Rust binary
FROM rust:1.89-bookworm AS builder

WORKDIR /build

# Cache dependencies: build with a dummy main first so dependency layer is reused
# when only source files change.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main(){}' > src/main.rs && cargo build --release && rm -rf src

# Now copy real source and do an incremental rebuild (only crustyclaw crate recompiles).
COPY src/ src/
RUN touch src/main.rs && cargo build --release && strip target/release/crustyclaw

# Stage 2: Runtime with Node.js (for claude CLI) and ffmpeg (for voice)
FROM node:22-bookworm-slim

RUN apt-get update && \
    apt-get install -y --no-install-recommends ffmpeg ca-certificates && \
    rm -rf /var/lib/apt/lists/*

# Pin to an exact version — floating @1 silently breaks when CLI flags change.
# Check for updates: npm view @anthropic-ai/claude-code version
ARG CLAUDE_CODE_VERSION=2.1.63
RUN npm install -g @anthropic-ai/claude-code@${CLAUDE_CODE_VERSION}

# Create non-root user
RUN useradd -m -s /bin/bash -u 1000 crustyclaw

COPY --from=builder /build/target/release/crustyclaw /usr/local/bin/crustyclaw

USER crustyclaw
WORKDIR /home/crustyclaw

ENV RUST_LOG=info

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
  CMD crustyclaw statusline 2>/dev/null | head -1 | grep -qv "offline" || exit 1

CMD ["crustyclaw"]
