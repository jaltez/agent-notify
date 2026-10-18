.POSIX:

BINARY := agent-notify
VERSION ?= $(shell git describe --tags --always --dirty 2>/dev/null || echo dev)
PKG := github.com/jaltez/agent-notify/internal/buildinfo
LDFLAGS = -s -w -X $(PKG).Version=$(VERSION)

.PHONY: build linux windows windows-console test fmt vet clean install release dev-install

build: linux windows windows-console

# Build the Windows exe and install it over the running tray (asks for
# elevation only if the destination denies the write).
dev-install: windows
	powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts/dev-update.ps1

# Local snapshot release (production releases come from the tag workflow
# running goreleaser; see .goreleaser.yml).
release:
	goreleaser release --snapshot --clean

linux:
	mkdir -p bin
	go build -trimpath -ldflags "$(LDFLAGS)" -o bin/$(BINARY) .

# windowsgui subsystem: no console window at all (double-click friendly).
# CLI subcommands still print when launched from a terminal via console
# attach, and over WSL interop via inherited pipes.
windows:
	mkdir -p bin
	GOOS=windows GOARCH=amd64 CGO_ENABLED=0 \
		go build -trimpath -ldflags "$(LDFLAGS) -H windowsgui" -o bin/$(BINARY).exe .

# Console-subsystem build for debugging (shows a window when launched
# outside a terminal).
windows-console:
	mkdir -p bin
	GOOS=windows GOARCH=amd64 CGO_ENABLED=0 \
		go build -trimpath -ldflags "$(LDFLAGS)" -o bin/$(BINARY)-console.exe .

test:
	go test ./...

fmt:
	gofmt -w .

vet:
	go vet ./...

clean:
	rm -rf bin dist

# Installs the Linux binary. The systemd user service is deployed by
# `agent-notify setup --service` (unit embedded in the binary).
PREFIX ?= /usr/local

install: linux
	install -Dm755 bin/$(BINARY) $(DESTDIR)$(PREFIX)/bin/$(BINARY)
