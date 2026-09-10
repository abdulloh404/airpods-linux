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
- `airpods-dsp.lua` — WirePlumber policy that inserts stereo/HRTF processing
  inside the existing AirPods A2DP sink without creating another output.

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

Set the listening mode through the daemon-owned AACP session:

```bash
airpodsctl mode off
airpodsctl mode anc
airpodsctl mode transparency
airpodsctl mode adaptive
```

Set output processing without selecting a separate PipeWire profile or sink:

```bash
airpodsctl sound mode off
airpodsctl sound mode wide
airpodsctl sound mode fix
airpodsctl sound mode spatial
airpodsctl sound status
```

`off` preserves the original stereo signal, `wide` applies normalized mid/side
widening, and `fix` renders a stationary binaural stage with the system SOFA
HRTF dataset. `spatial` uses the same graph and accepts `sound.yaw` and
`sound.pitch` metadata updates; automatic AirPods sensor forwarding is the next
integration step.

The kernel battery bridge is optional for audio operation. Without it, exact
Left and Right values remain available through `airpodsctl` and `airpods-gui`,
but stock UPower cannot represent both values as separate devices.

## Protocol reference

The vendored [AACP control-command reference](docs/references/librepods-control-commands.md)
comes from the GPL-3.0 licensed
[`librepods-org/librepods`](https://github.com/librepods-org/librepods)
repository at commit `53679cc90222e94ade84e66542d97ace2540e626`.

## Build


```bash
cd /home/abdulloh/github.com/airpods-linux

make build
sudo make install

sudo modprobe airpods_power

systemctl --user daemon-reload
systemctl --user enable --now airpodsd.service

airpods-gui
```
