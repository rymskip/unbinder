# syntax=docker/dockerfile:1

# The benchmark runs in the environment pixi.lock pins, so the tools here are
# the ones the repository is developed with.
FROM ghcr.io/prefix-dev/pixi:0.81.0-bookworm-slim

# The linker rustc drives.
RUN apt-get update \
  && apt-get install --yes --no-install-recommends build-essential ca-certificates \
  && rm -rf /var/lib/apt/lists/*

WORKDIR /unbinder
COPY pixi.toml pixi.lock rust-toolchain.toml ./
# The first cargo invocation installs the toolchain rust-toolchain.toml pins.
RUN pixi install --locked && pixi run cargo --version

COPY Cargo.toml Cargo.lock clippy.toml README.md LICENSE package.json ./
COPY src src
COPY benches benches
COPY scripts scripts
RUN pixi run build:js && pixi run bench:build

COPY bench bench
RUN pixi run bench:install && pixi run bench:fixtures

CMD ["pixi", "run", "bench:run"]
