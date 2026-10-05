# lynshen daemon for one user, as a container: the groundwork for a cloud
# agent (docs/cloud-agent.md). It is reached only through the relay; no
# port is published. Build from the repository root:
#
#   docker build -f docker/Dockerfile -t lynshen-daemon .
#
# Two volumes hold everything that must survive a restart:
#   /home/lynshen/.lynshen  state, sessions, auth.json, relay identity
#   /workspace            the repositories the agent works in

FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked --bin lynshen \
    && cp target/release/lynshen /usr/local/bin/lynshen

FROM debian:bookworm-slim
# git/ssh for cloning and pushing, ripgrep for the search tool, tini to
# reap the processes tool commands leave behind.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates git openssh-client ripgrep curl less procps tini \
    && rm -rf /var/lib/apt/lists/*
RUN useradd --create-home --shell /bin/bash --uid 1000 lynshen \
    && mkdir -p /workspace /home/lynshen/.lynshen \
    && chown lynshen:lynshen /workspace /home/lynshen/.lynshen
COPY --from=build /usr/local/bin/lynshen /usr/local/bin/lynshen
COPY docker/entrypoint.sh /usr/local/bin/lynshen-entrypoint
USER lynshen
ENV HOME=/home/lynshen
WORKDIR /workspace
VOLUME ["/home/lynshen/.lynshen", "/workspace"]
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s \
    CMD bash -c '</dev/tcp/127.0.0.1/7788' || exit 1
ENTRYPOINT ["tini", "--", "lynshen-entrypoint"]
