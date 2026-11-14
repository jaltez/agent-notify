.POSIX:

BINARY := agent-notify
VERSION ?= $(shell git describe --tags --always --dirty 2>/dev/null || echo dev)
WIN_TARGET := x86_64-pc-windows-gnu

# Releases are not built here: push a v* tag and
# .github/workflows/release.yml builds + publishes (same archive names
# the installers and in-app self-update consume).

.PHONY: build linux windows windows-console test fmt vet clean install dev-install

build: linux windows windows-console

# Build the Windows exe and install it over the running tray (asks for
# elevation only if the destination denies the write).
dev-install: windows
	powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts/dev-update.ps1

linux:
	mkdir -p bin
	AGENT_NOTIFY_VERSION=$(VERSION) cargo build --release
	cp target/release/$(BINARY) bin/

# windowsgui subsystem: no console window at all (double-click friendly).
# CLI subcommands still print when launched from a terminal via console
# attach, and over WSL interop via inherited pipes.
windows:
	mkdir -p bin
	AGENT_NOTIFY_VERSION=$(VERSION) \
		cargo build --release --target $(WIN_TARGET) --features windowsgui
	cp target/$(WIN_TARGET)/release/$(BINARY).exe bin/$(BINARY).exe

# Console-subsystem build for debugging (shows a window when launched
# outside a terminal).
windows-console:
	mkdir -p bin
	AGENT_NOTIFY_VERSION=$(VERSION) \
		cargo build --release --target $(WIN_TARGET)
	cp target/$(WIN_TARGET)/release/$(BINARY).exe bin/$(BINARY)-console.exe

test:
	cargo test

fmt:
	cargo fmt

# Advisory lint pass (clippy is not warning-free yet; not gated in CI).
vet:
	cargo clippy

clean:
	cargo clean
	rm -rf bin dist

# Installs the Linux binary. The systemd user service is deployed by
# `agent-notify setup --service` (unit embedded in the binary).
PREFIX ?= /usr/local

install: linux
	install -Dm755 bin/$(BINARY) $(DESTDIR)$(PREFIX)/bin/$(BINARY)
