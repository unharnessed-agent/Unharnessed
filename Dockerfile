# syntax=docker/dockerfile:1
# Unharnessed in a container: the README asks you to run it in one ("the
# kernel is not a sandbox"); this image is that container.
#
#   docker build -t unharnessed .
#   docker run --rm -it -v "$PWD:/workspace" -e ANTHROPIC_API_KEY unharnessed -p "Fix the failing tests"
#
# The build stage compiles `uh-cli` with the pinned toolchain; the runtime
# stage installs the binary with its sidecars (`unharnessed-runtime/` and
# `skills/` beside the executable, the packaged layout), uv, and a
# pre-built kernel venv, all as a non-root user.

FROM rust:1.98.1-slim AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
RUN cargo build --release --locked -p uh-cli --bin unharnessed

FROM python:3.12-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends git ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=ghcr.io/astral-sh/uv:0.13.5 /uv /uvx /usr/local/bin/
RUN useradd --create-home --uid 1000 agent
COPY --from=build /src/target/release/unharnessed /opt/unharnessed/bin/unharnessed
COPY unharnessed-runtime/ /opt/unharnessed/bin/unharnessed-runtime/
COPY skills/ /opt/unharnessed/bin/skills/
ENV PATH=/opt/unharnessed/bin:$PATH \
    UNHARNESSED_KERNEL_VENV=/opt/unharnessed/kernel-venv
# The kernel venv is pre-built at build time and kept outside ~/.unharnessed
# (via UNHARNESSED_KERNEL_VENV), so mounting that directory for sign-ins,
# settings and sessions never shadows it.
RUN chown -R agent:agent /opt/unharnessed
USER agent
RUN unharnessed --unharnessed-bootstrap
WORKDIR /workspace
ENTRYPOINT ["/opt/unharnessed/bin/unharnessed"]
CMD ["--help"]
