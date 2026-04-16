# glass

Custom notification daemon for KDE Plasma 6 (Wayland) with KDE blur protocol support.

## Features

- `org_kde_kwin_blur` protocol for frosted glass effect
- 2x HiDPI rendering (buffer scale 2)
- Rounded corners (tiny-skia bezier paths)
- Real app icons (freedesktop icon theme lookup)
- Click-to-dismiss + dbus signals (ActionInvoked, NotificationClosed)
- cosmic-text for text rendering

## Install

```bash
cargo build --release
cp target/release/glass ~/.local/bin/glass
systemctl --user enable --now glass.service
```

## Service

```bash
systemctl --user status glass.service
systemctl --user restart glass.service
journalctl --user -u glass -f
```

## Architecture

```
main.rs    → tokio runtime + wayland OS thread
dbus.rs    → org.freedesktop.Notifications server (zbus 5)
             + signal emitter (NotificationClosed, ActionInvoked)
wayland.rs → layer-shell surfaces + KDE blur + pointer input + rendering
```

## Config

Hardcoded constants in `wayland.rs`:
- `WIDTH=380, HEIGHT=94` — card size (logical)
- `RADIUS=16.0` — corner radius
- `SCALE=2` — HiDPI buffer scale
- `MARGIN_TOP=14, MARGIN_RIGHT=14` — position
- Fill: `rgba(22, 22, 38, 245)` — catppuccin mocha dark, 96% opaque
