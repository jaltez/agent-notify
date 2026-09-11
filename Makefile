.POSIX:

BINARY := agent-notify
VERSION ?= $(shell git describe --tags --always --dirty 2>/dev/null || echo dev)
PKG := github.com/jaltez/agent-notify/internal/buildinfo
LDFLAGS = -s -w -X $(PKG).Version=$(VERSION)

.PHONY: build linux windows windows-console test fmt vet clean install release

build: linux windows windows-console

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

# Installs the Linux binary and the systemd user service (headless run).
PREFIX ?= /usr/local

install: linux
	install -Dm755 bin/$(BINARY) $(DESTDIR)$(PREFIX)/bin/$(BINARY)
	install -Dm644 contrib/agent-notify.service \
		$(DESTDIR)$(HOME)/.config/systemd/user/agent-notify.service
	systemctl --user daemon-reload
