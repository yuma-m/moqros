# syntax=docker/dockerfile:1

ARG ROS_DISTRO=jazzy

# ---------------------------------------------------------------------------
# moq-relay from crates.io
# ---------------------------------------------------------------------------
FROM rust:1-bookworm AS relay-build
ARG MOQ_RELAY_VERSION=0.17.2
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    cargo install moq-relay --version ${MOQ_RELAY_VERSION} --locked \
      --no-default-features --features noq,websocket --root /opt/moq

FROM debian:bookworm-slim AS relay
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=relay-build /opt/moq/bin/moq-relay /usr/local/bin/moq-relay
COPY docker/relay.toml /etc/moq/relay.toml
EXPOSE 4443/udp 4443/tcp
ENTRYPOINT ["moq-relay", "/etc/moq/relay.toml"]

# ---------------------------------------------------------------------------
# moqros workspace (needs a sourced ROS 2 environment for r2r)
# ---------------------------------------------------------------------------
FROM ros:${ROS_DISTRO}-ros-base AS build
RUN apt-get update && apt-get install -y --no-install-recommends \
      curl ca-certificates clang libclang-dev pkg-config libvpx-dev \
    && rm -rf /var/lib/apt/lists/*
RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable
ENV PATH=/root/.cargo/bin:$PATH \
    # Only generate bindings for the message packages moqros uses.
    IDL_PACKAGE_FILTER="std_msgs;sensor_msgs;geometry_msgs;rosgraph_msgs"
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/root/.cargo/registry \
    --mount=type=cache,target=/src/target \
    . /opt/ros/${ROS_DISTRO}/setup.sh \
    && cargo build --release --workspace --locked \
    && cp target/release/moqros-pub target/release/moqros-sub target/release/image_source /usr/local/bin/

FROM ros:${ROS_DISTRO}-ros-base AS ros
RUN apt-get update && apt-get install -y --no-install-recommends ffmpeg libvpx9 \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /usr/local/bin/moqros-pub /usr/local/bin/moqros-sub /usr/local/bin/image_source /usr/local/bin/
