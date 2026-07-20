# Installs rfofs (JACK server binary), jack_probe (JACK diagnostics binary),
# rfofs-client (C-ABI shared library), and its C header together. PREFIX
# defaults to /usr/local, matching the
# `[install] root = "/usr/local"` default set in .cargo/config.toml.
#
# Run `make build` as your normal user first, THEN `sudo make install` (or
# `make install PREFIX=<writable dir>` without sudo). `install` never
# invokes cargo — it only copies already-built artifacts — so it can't hit
# "cargo: No such file or directory" under sudo's reset PATH, and it never
# leaves root-owned files under ./target/ that would break a later
# unprivileged `cargo build`.
#
# NOTE: the target-cpu=native rustflag in .cargo/config.toml means builds
# produced here are tied to the build machine's CPU — see README.md.

PREFIX  ?= /usr/local
BINDIR  := $(PREFIX)/bin
LIBDIR  := $(PREFIX)/lib
INCDIR  := $(PREFIX)/include

CARGO   ?= cargo
INSTALL ?= install

BIN_NAME    := rfofs
BIN_PATH    := target/release/$(BIN_NAME)
PROBE_NAME  := jack_probe
PROBE_PATH  := target/release/$(PROBE_NAME)
LIB_NAME    := librfofs_client.so
LIB_PATH    := target/release/$(LIB_NAME)
HEADER_NAME := rfofs_client.h
HEADER_PATH := rfofs-client/$(HEADER_NAME)

.PHONY: build install uninstall

build:
	$(CARGO) build --release --workspace

install:
	@test -f $(BIN_PATH) && test -f $(PROBE_PATH) && test -f $(LIB_PATH) || { \
		echo "error: build artifacts missing — run 'make build' first (as your normal user, not root)" >&2; \
		exit 1; \
	}
	$(INSTALL) -d $(BINDIR)
	$(INSTALL) -m 755 $(BIN_PATH) $(BINDIR)/$(BIN_NAME)
	$(INSTALL) -m 755 $(PROBE_PATH) $(BINDIR)/$(PROBE_NAME)
	$(INSTALL) -d $(LIBDIR)
	$(INSTALL) -m 755 $(LIB_PATH) $(LIBDIR)/$(LIB_NAME)
	-ldconfig $(LIBDIR) 2>/dev/null
	$(INSTALL) -d $(INCDIR)
	$(INSTALL) -m 644 $(HEADER_PATH) $(INCDIR)/$(HEADER_NAME)

uninstall:
	rm -f $(BINDIR)/$(BIN_NAME)
	rm -f $(BINDIR)/$(PROBE_NAME)
	rm -f $(LIBDIR)/$(LIB_NAME)
	-ldconfig $(LIBDIR) 2>/dev/null
	rm -f $(INCDIR)/$(HEADER_NAME)
