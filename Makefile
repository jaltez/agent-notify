.POSIX:

BINARY := agent-notify

.PHONY: build linux windows windows-gui test fmt vet clean install

build: linux windows

linux:
	mkdir -p bin
	go build -trimpath -ldflags "-s -w" -o bin/$(BINARY) .

windows:
	mkdir -p bin
	GOOS=windows GOARCH=amd64 CGO_ENABLED=0 \
		go build -trimpath -ldflags "-s -w" -o bin/$(BINARY).exe .

# No console window when launched from Explorer/shortcuts; CLI output is
# lost, so keep this variant for autostart use only.
windows-gui:
	mkdir -p bin
	GOOS=windows GOARCH=amd64 CGO_ENABLED=0 \
		go build -trimpath -ldflags "-s -w -H windowsgui" -o bin/$(BINARY)-gui.exe .

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
