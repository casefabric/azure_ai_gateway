# ------------------------------------------------------------------------------
# Cargo Build Stage
# ------------------------------------------------------------------------------
# Builder and runtime deliberately share the same Debian release. The binary
# links the distribution's glibc dynamically, so a builder from a different
# distro produces something that only fails once the container starts. The
# Debian cross toolchains below link against that same release's glibc, so this
# holds for cross builds too.
#
# The builder always runs on the build host's own platform and cross-compiles
# for the target, because compiling under QEMU emulation is many times slower.
#
# The slim variant has the C toolchain the native dependencies (ring) need and
# little else. Nothing from this stage ships.
FROM --platform=$BUILDPLATFORM rust:1.98-slim-trixie AS cargo-build

ARG BUILDPLATFORM
ARG TARGETPLATFORM

# Pick the Rust target and, when it differs from the host, install its C cross
# toolchain and tell cargo which compiler and linker to use. Debian names the
# package gcc-x86-64-linux-gnu but the tool it installs x86_64-linux-gnu-gcc,
# hence the separate pkg and gcc names.
#
# The upgrade picks up the Debian security fixes published since the rust image
# was last rebuilt; scanners otherwise report those against this stage.
RUN case "$TARGETPLATFORM" in \
      linux/amd64) triple=x86_64-unknown-linux-gnu;  pkg=x86-64;  gcc=x86_64-linux-gnu-gcc;  arch=amd64 ;; \
      linux/arm64) triple=aarch64-unknown-linux-gnu; pkg=aarch64; gcc=aarch64-linux-gnu-gcc; arch=arm64 ;; \
      *) echo "unsupported TARGETPLATFORM: $TARGETPLATFORM" >&2; exit 1 ;; \
    esac \
    && echo "$triple" > /rust-target \
    && touch /cargo-env \
    && apt-get update \
    && apt-get upgrade -y --no-install-recommends \
    && if [ "$TARGETPLATFORM" != "$BUILDPLATFORM" ]; then \
         apt-get install -y --no-install-recommends gcc-${pkg}-linux-gnu libc6-dev-${arch}-cross \
         && rustup target add "$triple" \
         && printf 'export CARGO_TARGET_%s_LINKER=%s\nexport CC_%s=%s\n' \
              "$(echo "$triple" | tr a-z- A-Z_)" "$gcc" "$(echo "$triple" | tr - _)" "$gcc" > /cargo-env; \
       fi \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /usr/src/azure_ai_gateway

# Compile the dependency graph on its own layer, against a stub main.rs, so that
# editing sources does not rebuild every crate. Cargo builds everything in
# [dependencies] here regardless of what the stub actually uses.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo "fn main() {}" > src/main.rs
RUN . /cargo-env && cargo build --release --locked --target "$(cat /rust-target)"

# The real sources. Cargo decides by mtime and COPY preserves the context's, so
# touch the entrypoint to be sure the stub's artifact is not reused. The binary
# is copied to a fixed path so the final stage need not know the target triple.
COPY . .
RUN touch src/main.rs \
    && . /cargo-env \
    && cargo build --release --locked --target "$(cat /rust-target)" \
    && cp "target/$(cat /rust-target)/release/azure_ai_gateway" /usr/local/bin/azure_ai_gateway

# The working directory is laid out here, with the owners and modes it must end
# up with, because the runtime image has no shell to create or fix them up in.
# log-config.yaml writes to logs/ relative to the working directory, so that
# one directory has to be writable by the account the service runs as
# (distroless `nonroot`, uid and gid 65532); the config itself stays read-only.
RUN install -d -m 755 /out/work \
    && install -m 444 log-config.yaml /out/work/log-config.yaml \
    && install -d -m 755 -o 65532 -g 65532 /out/work/logs

# ------------------------------------------------------------------------------
# Final Stage
# ------------------------------------------------------------------------------

# Distroless: Debian 13's glibc, libgcc, ca-certificates and tzdata, and nothing
# else. No shell, no package manager, no apt database for a scanner to report
# against. The same Debian release as the builder, so the dynamically linked
# glibc matches.
#
# reqwest is built with rustls (no OpenSSL), so the `cc` flavour (rather than
# `base`) is enough. Its ca-certificates are what the calls to ollama.com and
# api.anthropic.com are verified against.
FROM gcr.io/distroless/cc-debian13:nonroot

# The binary and its log config stay owned by root, so a compromised process
# cannot rewrite them.
COPY --from=cargo-build --chown=root:root --chmod=755 /usr/local/bin/azure_ai_gateway /usr/local/bin/azure_ai_gateway

# Copied as a directory, so the owners and modes come from the builder's layout
# above (COPY keeps numeric ownership when no --chown is given).
COPY --from=cargo-build /out/work/ /var/lib/azure_ai_gateway/

ARG VERSION="dev"
ARG COMMITID="unknown"

LABEL org.opencontainers.image.title="azure_ai_gateway" \
      org.opencontainers.image.description="Makes Ollama or the Anthropic API look like an Azure AI Foundry project endpoint, for the Mendix Agent Editor" \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.revision="${COMMITID}"

USER nonroot

# log-config.yaml and logs/ are resolved against the working directory. Mount a
# volume on /var/lib/azure_ai_gateway/logs to keep the logs (it must be
# writable by uid 65532), or mount another log config over log-config.yaml.
WORKDIR /var/lib/azure_ai_gateway

EXPOSE 8135

# The binary reads all of its settings from the environment itself (clap `env`),
# so CMD needs no shell to turn them into flags. Explicit flags on the command
# line still win. It always binds 0.0.0.0.
ENV AZURE_AI_GATEWAY_PORT="8135"

# `ollama` or `anthropic`. The latter needs ANTHROPIC_API_KEY, which is passed
# at run time (`-e ANTHROPIC_API_KEY`) and never baked into the image.
ENV AZURE_AI_GATEWAY_BACKEND="ollama"

# localhost inside the container is the container itself, so the default points
# at an Ollama on the Docker host. Docker Desktop resolves this name out of the
# box; on Linux add `--add-host=host.docker.internal:host-gateway`. That Ollama
# must listen on more than 127.0.0.1 to be reachable (OLLAMA_HOST=0.0.0.0).
ENV OLLAMA_URL="http://host.docker.internal:11434"

# Exec form, and there is no shell in the image anyway: the service is PID 1 and
# gets SIGTERM directly, so `docker stop` returns at once instead of waiting
# out the timeout behind a /bin/sh.
CMD ["azure_ai_gateway"]
