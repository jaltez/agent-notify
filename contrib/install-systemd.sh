#!/bin/sh
# Install agent-notify as a systemd --user service (headless `run` mode).
# For the tray experience, run the binary from your desktop session instead.
set -eu

BIN="${1:-}"
UNIT_DIR="${HOME}/.config/systemd/user"
HERE="$(cd "$(dirname "$0")" && pwd)"

if [ -z "$BIN" ]; then
	for cand in "$(command -v agent-notify || true)" \
		"$HERE/../bin/agent-notify" "$HOME/.local/bin/agent-notify"; do
		if [ -n "$cand" ] && [ -x "$cand" ]; then
			BIN="$cand"
			break
		fi
	done
fi
if [ -z "$BIN" ]; then
	echo "agent-notify binary not found; pass it: $0 /path/to/agent-notify" >&2
	exit 1
fi

mkdir -p "$UNIT_DIR"
sed "s|^ExecStart=.*|ExecStart=$BIN run|" \
	"$HERE/agent-notify.service" > "$UNIT_DIR/agent-notify.service"
systemctl --user daemon-reload
systemctl --user enable agent-notify
systemctl --user restart agent-notify
sleep 2
systemctl --user --no-pager status agent-notify | head -5
