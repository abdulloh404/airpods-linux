# airpods-linux

Private Ubuntu control stack for AirPods. One user daemon owns Bluetooth,
high-resolution microphone, and battery state; the CLI and GTK4 application
control it over the session D-Bus.

## Components

- `airpodsd` — long-running Rust user daemon and the only hardware owner.
- `airpodsctl` — Rust command-line client.
- `airpods-gui` — Rust GTK4 client using the X11 backend on X11 or XWayland.
- `airpods-core` — AACP transport, framing, and AirPods battery decoding.
- `airpods-audio` — C++20 AAC-ELD/DSP/PipeWire engine with a Rust facade.
- `airpods-power` — optional Linux `power_supply` bridge for separate Left and
  Right batteries in UPower.

## Build

Required system development libraries are GTK4, PipeWire, BlueZ, D-Bus, and
the headers for the running Linux kernel. FDK-AAC is already vendored and does
not need to be installed separately.

```bash
make build
```

The project does not currently build a Debian package. The user service unit
is kept in `systemd/airpodsd.service` for later installation work.

## Runtime ownership

`airpodsd` creates the PipeWire virtual source and owns the AACP connection.
Neither client edits configuration files, invokes `systemctl`, nor connects to
the AirPods directly.

The kernel battery bridge is optional for audio operation. Without it, exact
Left and Right values remain available through `airpodsctl` and `airpods-gui`,
but stock UPower cannot represent both values as separate devices.
