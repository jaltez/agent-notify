.POSIX:

BINARY := agent-notify

.PHONY: build linux windows windows-console test fmt vet clean install release

build: linux windows windows-console

# Release artifacts: dist/agent-notify-{linux,windows}-amd64 archives + SHA256SUMS
release: clean build
	mkdir -p dist
	cp bin/agent-notify bin/agent-notify.exe bin/agent-notify-console.exe dist/
	cp README.md LICENSE CHANGELOG.md dist/
	cd dist && tar czf agent-notify-linux-amd64.tar.gz agent-notify README.md LICENSE CHANGELOG.md
	cd dist && zip -q agent-notify-windows-amd64.zip agent-notify.exe agent-notify-console.exe README.md LICENSE CHANGELOG.md
	rm dist/README.md dist/LICENSE dist/CHANGELOG.md
	cd dist && sha256sum agent-notify* > SHA256SUMS
	@echo "artifacts in dist/"

linux:
	mkdir -p bin
	go build -trimpath -ldflags "-s -w" -o bin/$(BINARY) .

# windowsgui subsystem: no console window at all (double-click friendly).
# CLI subcommands still print when launched from a terminal via console
# attach, and over WSL interop via inherited pipes.
windows:
	mkdir -p bin
	GOOS=windows GOARCH=amd64 CGO_ENABLED=0 \
		go build -trimpath -ldflags "-s -w -H windowsgui" -o bin/$(BINARY).exe .

# Console-subsystem build for debugging (shows a window when launched
# outside a terminal).
windows-console:
	mkdir -p bin
	GOOS=windows GOARCH=amd64 CGO_ENABLED=0 \
		go build -trimpath -ldflags "-s -w" -o bin/$(BINARY)-console.exe .

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
