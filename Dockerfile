# ============================================
# Stage 1: Build the active-call binary
# ============================================
FROM --platform=$BUILDPLATFORM rust:bookworm AS builder

# Install build dependencies
RUN apt-get update && apt-get install -y \
    libasound2-dev \
    libopus-dev \
    cmake \
    pkg-config \
    clang \
    libclang-dev \
    && rm -rf /var/lib/apt/lists/*

# Set environment for bindgen and clang
ENV LIBCLANG_PATH=/usr/lib/llvm-14/lib
ENV CC=clang
ENV CXX=clang++

# Create build directory
WORKDIR /build

# Copy the source code
COPY . .

# Build the release binary
RUN cargo build --release && \
    mkdir -p /build/bin && \
    cp target/release/active-call /build/bin/

# ============================================
# Stage 2: Create the runtime image
# ============================================
FROM debian:bookworm-slim

LABEL maintainer="shenjindi@miuda.ai"
LABEL org.opencontainers.image.source="https://github.com/restsend/active-call"
LABEL org.opencontainers.image.description="A SIP/WebRTC voice agent"

# Set environment variables
ARG DEBIAN_FRONTEND=noninteractive
ENV LANG=C.UTF-8
ENV TZ=UTC

# Install runtime dependencies
RUN --mount=type=cache,target=/var/cache/apt \
    apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    tzdata \
    libopus0 \
    curl \
    && rm -rf /var/lib/apt/lists/*

# Create app user for security
RUN groupadd -r activeuser && useradd -r -g activeuser activeuser

# Create application directory structure
WORKDIR /app
RUN mkdir -p /app/config/mediacache /app/config/cdr /app/config/recorders /app/static

# Copy built binary and static files
COPY --from=builder /build/bin/active-call /app/active-call
COPY --from=builder /build/static /app/static

# Set ownership
RUN chown -R activeuser:activeuser /app

# Switch to non-root user
USER activeuser

# Expose ports
EXPOSE 8080
EXPOSE 13050/udp
EXPOSE 20000-30000/udp

# Default entrypoint
ENTRYPOINT ["/app/active-call"]

# Default command
CMD ["--conf", "/app/config.toml"]
