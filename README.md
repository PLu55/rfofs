# RFofs

## Installation

### Binary only (`rfofs`)

    cargo install --path . --bin rfofs

This workspace's `.cargo/config.toml` sets `[install] root = "/usr/local"`,
so this installs to `/usr/local/bin/rfofs` by default. Override the
destination with `--root <dir>`.

**Always pass `--bin rfofs` explicitly.** Without it, `cargo install --path
.` installs every binary target in this package, including the internal
`jack_probe` dev tool (`src/bin/jack_probe.rs`), which is not meant to be
installed system-wide.

### Binary + client library

    make build
    sudo make install

`make build` compiles `rfofs` and `rfofs-client` as your normal user.
`make install` then copies `rfofs` (to `$(PREFIX)/bin`), rfofs-client's
shared library, `librfofs_client.so` (to `$(PREFIX)/lib`), and its C header,
`rfofs_client.h` (to `$(PREFIX)/include`) — it never invokes `cargo`, so it's
safe to run under `sudo` (running `cargo build` itself as root would leave
root-owned files under `./target/` and break later unprivileged builds).
`PREFIX` defaults to `/usr/local`, matching the `cargo install` default
above. Override it with `make install PREFIX=/opt/rfofs` (drop `sudo` if
`PREFIX` is user-writable).

See `rfofs-client/README.md` for the library's C API and how external
processes (e.g. Racket) load and use it.

Uninstall with:

    sudo make uninstall
    sudo make uninstall PREFIX=/opt/rfofs   # if installed to a custom prefix

### Portability note

`.cargo/config.toml` sets `rustflags = ["-C", "target-cpu=native"]`, so
release builds — including anything produced by the commands above — are
compiled for the exact CPU of the machine that built them. Don't copy these
binaries/libraries to a different machine; rebuild there instead.
