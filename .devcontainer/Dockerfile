# Dev image for the app service in compose.yaml.
#
# Built on the official Rust image; the only thing this adds is making `cargo` survive a login
# shell. The base image puts /usr/local/cargo/bin on PATH through ENV, but Debian's /etc/profile
# *reassigns* PATH for login shells, so `bash -l` (and devcontainer terminals that source the
# profile) lose cargo. An /etc/profile.d entry is the only fix that survives a login shell.
FROM docker.io/library/rust:1.98-bookworm

RUN printf '%s\n' \
      'export PATH="/usr/local/cargo/bin:/usr/local/rustup/bin:$PATH"' \
      > /etc/profile.d/rust-cargo.sh \
    && chmod 0644 /etc/profile.d/rust-cargo.sh

WORKDIR /workspace