# Personal fork integration branch

The `personal/main` branch is the source of local release builds. It tracks the latest reviewed
upstream base while retaining the optional XDG ScreenCast/PipeWire global pointer implementation.
Branches used by upstream pull requests remain independent and do not inherit this personal-only
integration policy.

## Semantic ScreenCast portal helper

Restarting a build with personal-only global pointer tracking opens GNOME's ScreenCast portal.
The AT-SPI adapter selects or preserves exactly one monitor and invokes the accessible Share
action without screen coordinates or input injection. It fails closed when controls are missing or
ambiguous and writes diagnostic state below `artifacts/gui/`.

Install the Arch Linux runtime dependencies and inspect the dialog without approving it:

```bash
sudo pacman -S python-atspi at-spi2-core
./scripts/gui-test/approve-screen-share.py --dry-run
```

Approve a chooser that exposes exactly one monitor:

```bash
./scripts/gui-test/approve-screen-share.py
```

Pass `--monitor 'accessible monitor name'` when the chooser contains several monitors.

## Audio response

Local builds pin the renderer submodule to the fork's `personal/main` branch. Scene wallpapers use
the renderer's existing system-output spectrum path. The personal renderer additionally completes
Wallpaper Engine's Web audio-response contract:

- Web wallpapers opt in with `general.supportsaudioprocessing: true` in `project.json`.
- The renderer captures the desktop output through a PulseAudio monitor source (including
  PipeWire's PulseAudio compatibility server), computes 64 frequency bands per channel, and sends
  left 64 followed by right 64 at roughly 30 Hz.
- An explicit host-provided `audio_samples` stream takes precedence, preventing two spectrum
  producers from driving the same wallpaper.
- Capture is lazy: wallpapers that do not opt in never open a monitor device.

The data layout and opt-in behavior follow Wallpaper Engine's official
[Web audio visualization](https://docs.wallpaperengine.io/en/web/audio/visualizer.html) contract.
SceneScript's corresponding buffer sizes and per-frame update model are documented in the official
[audio-response tutorial](https://docs.wallpaperengine.io/en/scene/scenescript/tutorial/audio.html)
and [AudioBuffers reference](https://docs.wallpaperengine.io/en/scene/scenescript/reference/class/AudioBuffers.html).

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
