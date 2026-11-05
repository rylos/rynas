# Build environment of the rynas .deb: Debian 13 (trixie), so the binaries link against
# trixie's glibc and dpkg-shlibdeps computes the real library dependencies.
# Native libraries linked by the workspace: PAM (rynasd), SQLite (config, notifications,
# scheduler history), zstd (.rynasbak archives), OpenSSL through native-tls (notification
# channels: SMTP, HTTPS webhooks); clang for bindgen.
ARG RUST_IMAGE=rust:1.98-trixie
FROM ${RUST_IMAGE}
ARG CARGO_DEB_VERSION=3.8.0
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
        libpam0g-dev libsqlite3-dev libzstd-dev libssl-dev pkg-config \
        libclang-dev clang brotli dpkg-dev \
 && rm -rf /var/lib/apt/lists/*
RUN cargo install cargo-deb --locked --version "${CARGO_DEB_VERSION}" \
 && rm -rf /usr/local/cargo/registry
