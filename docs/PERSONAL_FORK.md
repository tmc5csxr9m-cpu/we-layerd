# Personal fork integration branch

The `personal/main` branch is the source of local release builds. It tracks the latest reviewed
upstream base while retaining the optional XDG ScreenCast/PipeWire global pointer implementation.
Branches used by upstream pull requests remain independent and do not inherit this personal-only
integration policy.

## Upstream notification

Install the checker and its user service:

```bash
install -Dm755 scripts/check-upstream-updates ~/.local/bin/we-layerd-check-upstream
install -Dm644 systemd/we-layerd-upstream-check.service \
  ~/.config/systemd/user/we-layerd-upstream-check.service
install -Dm600 systemd/upstream-check.env.example \
  ~/.config/we-layerd/upstream-check.env
~/.local/bin/we-layerd-check-upstream --initialize
systemctl --user daemon-reload
systemctl --user enable --now we-layerd-upstream-check.service
```

The service runs once when `graphical-session.target` starts. It checks both
`Aromatic05/we-layerd/main` and `Aromatic05/wallpaper-engine-renderer/master`, retries network
lookups a bounded number of times, and sends a desktop notification only when a branch head changes.
The last successfully notified heads are stored in
`${XDG_STATE_HOME:-~/.local/state}/we-layerd/upstream-heads`.
The optional `~/.config/we-layerd/upstream-check.env` supplies proxy variables to the user
manager, which does not necessarily inherit the login shell environment. Network failures are
retried at most three times by the service and never advance the recorded upstream heads.

Inspect the most recent run with:

```bash
systemctl --user status we-layerd-upstream-check.service
journalctl --user -u we-layerd-upstream-check.service
```
