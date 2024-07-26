FROM lukemathwalker/cargo-chef:latest-rust-1.79 AS chef
WORKDIR /app

LABEL org.opencontainers.image.source=https://github.com/paradigmxyz/reth
LABEL org.opencontainers.image.licenses="MIT OR Apache-2.0"

# Builds a cargo-chef plan
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json

# Build profile, release by default
ARG BUILD_PROFILE=release
ENV BUILD_PROFILE $BUILD_PROFILE

# Extra Cargo flags
ARG RUSTFLAGS=""
ENV RUSTFLAGS "$RUSTFLAGS"

# Extra Cargo features
ARG FEATURES="optimism"
ENV FEATURES $FEATURES

# Install system dependencies
RUN apt-get update && apt-get -y upgrade && apt-get install -y libclang-dev pkg-config lsb-release wget software-properties-common gnupg

# Install llvm
COPY .github/assets/install_llvm_ubuntu.sh /usr/local/bin/install_llvm_ubuntu.sh
RUN chmod +x /usr/local/bin/install_llvm_ubuntu.sh
ENV LLVM_VERSION=18
RUN /usr/local/bin/install_llvm_ubuntu.sh $LLVM_VERSION

# Builds dependencies
RUN cargo chef cook --profile $BUILD_PROFILE --features "$FEATURES" --recipe-path recipe.json

# Build application
COPY . .
RUN cargo build --profile $BUILD_PROFILE --features "$FEATURES" --locked --bin op-reth

# ARG is not resolved in COPY so we have to hack around it by copying the
# binary to a temporary location
RUN cp /app/target/$BUILD_PROFILE/op-reth /app/op-reth

# Use Ubuntu as the release image
FROM ubuntu AS runtime
WORKDIR /app

# Copy reth over from the build stage
COPY --from=builder /app/op-reth /usr/local/bin

RUN apt-get update && apt-get -y upgrade && apt-get install -y libclang-dev pkg-config lsb-release wget software-properties-common gnupg

# Install llvm
COPY .github/assets/install_llvm_ubuntu.sh /usr/local/bin/install_llvm_ubuntu.sh
RUN chmod +x /usr/local/bin/install_llvm_ubuntu.sh
ENV LLVM_VERSION=18
RUN /usr/local/bin/install_llvm_ubuntu.sh $LLVM_VERSION

# Copy licenses
COPY LICENSE-* ./

EXPOSE 30303 30303/udp 9001 8545 8546
ENTRYPOINT ["/usr/local/bin/op-reth"]
