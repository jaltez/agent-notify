# Remote herdr sessions (SSH socket bridge)

agent-notify only watches the machine it runs on: Unix sockets under
`~/.config/herdr` (local backend), `herdr.exe` via PATH (windows backend),
and distro sockets via `wsl.exe` (wsl backend). A herdr running on another
host is invisible to all three. It can still be watched: forward the
remote's herdr Unix socket into your local `~/.config/herdr/sessions/<name>/`
and agent-notify auto-discovers it as an ordinary session.

Nothing is installed on the remote. OpenSSH forwards Unix sockets natively
(StreamLocal channels), so one `ssh -N -L` per remote is the whole bridge;
the remote only needs sshd.

```
remote host                              your machine
~/.config/herdr/herdr.sock    ssh -N -L    ~/.config/herdr/sessions/<name>/herdr.sock
        └────────────────────────<──────────────────────────────────────┘
                                (StreamLocal forward)
```

The socket must be created in the `~/.config/herdr` of the environment your
agent-notify actually scans. If the tray runs on Windows, that is the WSL
distro the tray polls through `wsl.exe`, not the Windows side.

Naming follows auto-discovery: a WSL/Linux daemon reports the session as
`local/<name>`, the Windows tray as `wsl/<name>`. That is expected, not a
misconfiguration.

herdr's own `herdr --remote <host>` attaches a terminal to a remote server,
but the socket belongs to the server; the attaching client does not expose
one locally, so it does not feed agent-notify.

## One-off test

```bash
mkdir -p ~/.config/herdr/sessions/vr02
rm -f  ~/.config/herdr/sessions/vr02/herdr.sock
ssh -N vr02-wsl \
  -L /home/you/.config/herdr/sessions/vr02/herdr.sock:/home/remote/.config/herdr/herdr.sock
```

- Both paths are absolute. The remote endpoint is used verbatim by sshd; `~`
  and shell variables are not expanded on the remote side.
- The local socket path must not exist when ssh starts; ssh removes it on
  clean exit.
- Check with `agent-notify probe`: the session should list with a snapshot.

## Keep it running

One systemd user unit per remote,
`~/.config/systemd/user/herdr-bridge-<name>.service`:

```ini
[Unit]
Description=herdr bridge: vr02

[Service]
ExecStartPre=/usr/bin/mkdir -p %h/.config/herdr/sessions/vr02
ExecStartPre=-/usr/bin/rm -f %h/.config/herdr/sessions/vr02/herdr.sock
ExecStart=/usr/bin/ssh -N -T \
  -o ExitOnForwardFailure=yes \
  -o ServerAliveInterval=30 -o ServerAliveCountMax=3 \
  -L %h/.config/herdr/sessions/vr02/herdr.sock:/home/remote/.config/herdr/herdr.sock \
  vr02-wsl
Restart=always
RestartSec=5

[Install]
WantedBy=default.target
```

```bash
systemctl --user enable --now herdr-bridge-vr02.service
```

Why the flags:

- `ExitOnForwardFailure=yes`: a failed forward must terminate ssh, otherwise
  it stays up doing nothing and Restart never fires.
- `ServerAliveInterval`/`CountMax`: detect dead tunnels through NATs
  instead of hanging on a half-open connection.
- `Restart=always` + `RestartSec=5`: a down remote (or a stopped WSL distro
  on the remote) is retried every 5 s; the bridge connects once it is back.

## Caveats

- The remote must be reachable. On a remote whose herdr lives in WSL, the
  distro only stays alive while something runs inside it, and sshd must be
  started; for bridges that survive your absence that means starting sshd
  at boot (on the remote's Windows: a scheduled task running
  `wsl -d <distro> -- service ssh start`, or systemd inside the distro).
- Forward only sockets you know are live. A remote with a stopped herdr
  server shows up as a permanently errored session, same as any dead
  socket; forwarding a stale socket adds noise.
- Multi-hop routes (ProxyJump) work, but each bridge is only as available
  as its hops.
- Version skew: compatibility is herdr's client/server contract. Keep the
  local herdr (the one agent-notify resolves through PATH) at least as new
  as the remotes. A newer client against an older server has been verified
  to work (0.9.5 client, 0.9.4 server).
- A herdr running on the remote's Windows side cannot be bridged this way:
  Win32 OpenSSH does not implement Unix-socket forwarding for Windows-side
  sockets. That case needs a relay process on the remote.
- The local backend needs the `herdr` binary in PATH (it shells out to it,
  and under `enabled = "auto"` it is silently skipped when the binary is
  not found). systemd user units do not inherit your shell PATH: if herdr
  lives in `~/.local/bin`, give the agent-notify unit
  `Environment="PATH=%h/.local/bin:/usr/bin:/bin"` or set `binary` under
  `[herdr.local]`.

## Undo

```bash
systemctl --user disable --now herdr-bridge-<name>.service
rm ~/.config/systemd/user/herdr-bridge-<name>.service
systemctl --user daemon-reload
rm -rf ~/.config/herdr/sessions/<name>
```
