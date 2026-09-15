# airpods-linux

A private Ubuntu control stack for AirPods, built with Rust, C++20, and an
optional Linux kernel battery bridge. A single user daemon, `airpodsd`, owns
the AACP connection, virtual microphone, configuration, and battery state.
The CLI and GTK4 application communicate with it over the session D-Bus.

## Features

- List AirPods known to BlueZ and select the device used by the daemon.
- Receive an AAC-ELD microphone stream over AACP and expose a mono, 64 kHz,
  signed 16-bit PipeWire virtual source.
- Adjust microphone gain and limiter settings while streaming.
- Send Off, Noise Cancellation, Transparency, and Adaptive listening-mode commands.
- Read left and right battery levels and charging state from AACP notifications,
  with BLE advertisements as a fallback.
- Expose separate left and right batteries to UPower through the optional
  `airpods_power` kernel module.
- Save user settings and retry failed AACP/audio sessions with backoff.

Pair and connect the AirPods using the system's Bluetooth controls first.
The daemon waits for BlueZ to report an existing connection; it does not pair
devices or initiate their Bluetooth connection. Feature availability depends
on the AirPods model and firmware.

## Components

| Component | Responsibility | Source |
| --- | --- | --- |
| `airpodsd` | User daemon: configuration, D-Bus service, device inventory, audio lifecycle, and battery workers | [Daemon entry point](crates/airpodsd/src/main.rs) |
| `airpodsctl` | Command-line client for the daemon | [CLI](crates/airpodsctl/src/main.rs) |
| `airpods-gui` | GTK4 client using X11 or XWayland | [GUI entry point](crates/airpods-gui/src/main.rs) |
| `airpods-ipc` | Shared D-Bus contract, payload types, and control limits | [IPC contract](crates/airpods-ipc/src/lib.rs) |
| `airpods-core` | AACP transport, audio framing, and AACP/BLE battery parsing | [Core modules](crates/airpods-core/src/lib.rs) |
| `airpods-audio` | Rust interface to the C++ AAC-ELD decoder, DSP, queue, and PipeWire engine | [Rust interface](crates/airpods-audio/src/lib.rs), [C++ engine](crates/airpods-audio/native/audio_engine.cpp) |
| `airpods-power` | Kernel `power_supply` bridge for separate earbud batteries | [Kernel module](kernel/airpods-power/airpods_power.c) |

The audio path is:

```text
AirPods microphone
  → AACP over Bluetooth L2CAP PSM 0x1001
  → type 0x58 audio packets
  → AAC-ELD access units
  → FDK-AAC decoder
  → gain and limiter
  → bounded PCM queue
  → PipeWire virtual microphone
```

BlueZ access, AACP commands, audio processing, and battery publishing happen
inside the daemon. The clients use the shared D-Bus interface and do not edit
configuration files or manage systemd services directly.

## Requirements

### Build dependencies

- Rust and Cargo with support for the dependencies in `Cargo.lock`. The workspace
  uses edition 2024 and declares `rust-version = "1.92"`; dependencies may have
  additional compiler requirements.
- A C++20 compiler, CMake, Make, and pkg-config.
- GTK4 development files supporting GTK 4.6 or later.
- PipeWire development files providing `libpipewire-0.3 >= 1.6.8`.
- D-Bus development files required by the BlueZ integration.
- Build headers for the running Linux kernel, available under
  `/lib/modules/$(uname -r)/build`.

FDK-AAC is vendored in `third_party/fdk-aac` and compiled as a static library.
A separate system installation of FDK-AAC is not required.

### Runtime dependencies

- BlueZ and a Bluetooth adapter supporting the BR/EDR connection and BLE scanning
  used by this implementation.
- PipeWire, a session manager such as WirePlumber, and a PulseAudio-compatible
  server such as `pipewire-pulse` for the `pactl` operations.
- `pactl` on the daemon's `PATH`.
- An active user session with a session D-Bus and systemd user manager.
- GTK4 and an X11 display, or XWayland when running a Wayland session.
- UPower and the kernel module if separate system battery devices are desired.

## Building

Run from the repository root:

```bash
make build
```

This builds the entire Rust workspace in the default debug profile, followed
by the kernel module for the running kernel. The main outputs are:

| Output | Purpose |
| --- | --- |
| `target/debug/airpodsd` | Daemon executable |
| `target/debug/airpodsctl` | CLI executable |
| `target/debug/airpods-gui` | GTK4 application |
| `kernel/airpods-power/airpods_power.ko` | Kernel battery module |

The current Makefile builds and installs the kernel module along with the
applications, even though audio can run without a loaded battery module.
There is currently no Debian packaging or DKMS integration in this repository.

### PipeWire build and runtime paths

The Makefile locates `pipewire` on `PATH`, derives its installation prefix,
and prepends `<prefix>/lib/pkgconfig` to the build's `PKG_CONFIG_PATH`.
`PIPEWIRE_EXECUTABLE` and `PIPEWIRE_PREFIX` can be overridden as Make variables.
Both Rust build scripts require PipeWire development files at least version 1.6.8.
The daemon build script also passes the discovered library directories to the
linker as runtime search paths.

The supplied [systemd unit](systemd/airpodsd.service) separately sets these
runtime environment variables:

```ini
LD_LIBRARY_PATH=/opt/pipewire-1.6.8/lib
PIPEWIRE_MODULE_DIR=/opt/pipewire-1.6.8/lib/pipewire-0.3
SPA_PLUGIN_DIR=/opt/pipewire-1.6.8/lib/spa-0.2
```

These service paths are fixed in the current source. If your PipeWire installation
uses another location, review and adapt the service environment before installation.
Selecting a different build prefix does not update the service unit automatically.

## Installation

Build first, then run the installer from the intended user's active desktop session:

```bash
sudo make install
```

The installer resolves the target user from `SUDO_USER` and checks for an active
session bus. It then:

1. Installs the daemon, CLI, and GUI in `/usr/local/bin`.
2. Installs the user service in `/usr/local/lib/systemd/user` and the desktop
   launcher in `/usr/local/share/applications`.
3. Installs the kernel module in `/lib/modules/<kernel-release>/extra`.
4. Installs the udev rule and module autoload configuration in `/etc/udev/rules.d`
   and `/etc/modules-load.d`.
5. Runs `depmod`, reloads udev rules and the user's systemd configuration, and
   enables and restarts `airpodsd.service`.
6. Calls `airpodsctl mic start` as the target user, saving the request to enable
   the microphone.

The installer does not load the kernel module into the current session. To enable
system battery integration immediately:

```bash
sudo modprobe airpods_power
```

The installed modules-load configuration requests the module on subsequent boots.
Rebuild and reinstall it for a new kernel release when needed.

### Select the AirPods

After connecting the AirPods through the system Bluetooth controls, list the devices:

```bash
airpodsctl device list
```

Use an address from that output in the following commands. Replace the example
address with your device's actual address:

```bash
airpodsctl device select AA:BB:CC:DD:EE:FF
airpodsctl mic start
airpodsctl status
```

The installer can request microphone startup before a device has been selected.
In that case, the daemon reports `no AirPods device is selected` until a selection
is made. Selecting a device stores its address; it does not connect Bluetooth.

Select the input named `Microphone virtual - Abdulloh's AirPods Pro` in the
application that should receive microphone audio. This display name and the
PipeWire node name `Microphone_Virtual_Abdullohs_AirPods_Pro` are fixed in the source
and do not change with the selected device's name.

## CLI usage

| Command | Action |
| --- | --- |
| `airpodsctl status` | Show daemon state, selected device, microphone state, gain, limiter, bridge availability, and the last error |
| `airpodsctl device list` | List known AirPods; `*` marks the selected device |
| `airpodsctl device select <ADDRESS>` | Save the device address used by the daemon |
| `airpodsctl mic start` | Request microphone startup and save the enabled state |
| `airpodsctl mic stop` | Request microphone shutdown and save the disabled state |
| `airpodsctl mic status` | Show microphone state and reconnect-attempt information |
| `airpodsctl mic gain <DB>` | Set pre-limiter gain from 0 to 30 dB |
| `airpodsctl mic limiter -- <DBFS>` | Set the limiter ceiling from -12 to 0 dBFS |
| `airpodsctl battery` | Show left/right percentages and charging state |

For example:

```bash
airpodsctl mic gain 18
airpodsctl mic limiter -- -3
```

The `--` separates a negative positional value from command-line options.
Start and stop commands acknowledge the request before the audio worker finishes
starting or stopping. Read `status` or `mic status` to follow the resulting state.

### Listening modes

| Command | Requested mode |
| --- | --- |
| `airpodsctl mode off` | Off |
| `airpodsctl mode anc` | Noise Cancellation |
| `airpodsctl mode transparency` | Transparency |
| `airpodsctl mode adaptive` | Adaptive, on supported models |

The daemon sends AACP control identifier `0x0D` through its existing microphone
session, or through a temporary AACP session when the microphone is not streaming.
A successful response confirms that the command was sent. The implementation does
not read back the current listening mode or persist the mode in its configuration.

## GTK4 application

```bash
airpods-gui
```

The application provides device selection, a microphone switch, gain and limiter
controls, left/right battery indicators, daemon state, bridge availability, and
error messages. Listening modes are currently available through the CLI and D-Bus.

The GUI runs GTK on the main thread and D-Bus operations on a separate Tokio worker.
It subscribes to daemon signals and supports manual refresh. Its 80 ms UI timer
drains received events rather than polling the daemon every 80 ms.
The application explicitly selects the X11 backend, so Wayland sessions need XWayland.

## Configuration

The daemon stores configuration at:

```text
$XDG_CONFIG_HOME/airpods-linux/config.toml
```

When `XDG_CONFIG_HOME` is unset, the usual Linux location is
`~/.config/airpods-linux/config.toml`. The default values are:

```toml
selected_device = ""
mic_enabled = false
gain_db = 18.0
limiter_db = -3.0
```

Use the CLI or GUI to update settings through the daemon. Configuration is loaded
at daemon startup and saved using a temporary file, file synchronization, and
atomic rename. There is no configuration-file watcher.
Missing fields use defaults. A missing file uses the default configuration;
invalid or unreadable configuration causes the daemon to use defaults and report
the load error in its status.

Gain accepts finite values from 0 to 30 dB, and the limiter accepts finite values
from -12 to 0 dBFS. Although the audio library's standalone default gain is 0 dB,
the daemon overrides it with its configured value, which defaults to 18 dB.
Queue capacity, jitter settings, and listening mode are not stored in this file.

If saving a start/stop request fails, the daemon still applies the requested
microphone state in memory and returns an error about persistence. Check the
reported runtime state separately from whether the setting was saved.

## Audio lifecycle

The C++ engine decodes raw AAC-ELD, applies gain and a limiter with a 100 ms release
time constant, and sends PCM through a bounded single-producer/single-consumer
queue to PipeWire. The default physical queue holds 250 ms of audio. Buffering
starts with a 60 ms target and adapts between 40 and 80 ms; old samples are discarded
when the queue exceeds a 150 ms hard threshold. These are engine buffer settings,
not a measurement of end-to-end microphone latency.

The virtual source requests a 10 ms PipeWire node latency. The engine also uses
PipeWire rate matching, when available, to compensate for clock drift. Audio
metrics are available in the library API but are not currently exposed by the daemon.

| State | Meaning |
| --- | --- |
| `idle` | Microphone operation is disabled, or the lifecycle loop has exited |
| `error` | Microphone operation was requested without a selected device |
| `connecting` | Waiting for the Bluetooth connection or setting up AACP and the audio engine |
| `streaming` | The native source has started and the AACP START command has succeeded |
| `stopping` | Cleanup has completed for a session leaving the receive loop |
| `recovering` | A session failed and the daemon is waiting before another attempt |

`streaming` is set before the first usable audio frame arrives. A watchdog detects
three seconds without usable queued audio. Consecutive decoder failures or full
queues also end an attempt. Failed sessions are retried with delays of 1, 2, 4, 8,
16, and then at most 32 seconds.

Starting or stopping the microphone can temporarily set the selected AirPods'
active A2DP profile to `off` and restore the previous `a2dp-sink` profile through
`pactl`. This may briefly interrupt playback. The daemon retries profile restoration
and retains a failed restoration for the next reset attempt.

## Battery reporting and UPower

AACP battery notifications take priority when available and carry integer
percentages from 0 to 100. Otherwise, while the selected device is reported as
connected, the battery worker performs BLE fallback scans on connection and
approximately every 30 seconds. A scan waits up to 10 seconds for a supported
advertisement. BLE percentages have 10% resolution.

Battery data is invalidated when the selected device is reported disconnected
or after two consecutive fallback scan failures. While using AACP data, the
worker republishes its cached values to the kernel bridge every 30 seconds;
this refreshes the bridge's timeout without proving a new measurement arrived.

The optional bridge receives seven-byte updates on `/dev/airpods_power` and
registers `airpods_left` and `airpods_right` under `/sys/class/power_supply`
when values are available. It removes absent devices and clears both devices
if updates stop for 90 seconds. The daemon also invalidates the bridge during
normal shutdown. The supplied udev rule uses mode `0660` and `uaccess` for access
from the active user session.

Without the module, microphone operation and CLI/GUI battery reporting remain
available. The status field for bridge availability reflects access to the bridge;
it does not confirm that UPower has already displayed the devices.

### Current limitations

- Device inventory is refreshed approximately every five seconds and filters the
  BlueZ `Name`, falling back to `Alias`, for the word `airpods` without regard to case.
  Devices whose chosen name does not contain that word will not appear in the list.
- Bluetooth connection checks and BLE scans use the default BlueZ adapter;
  adapter selection is not exposed in the CLI or GUI.
- BLE fallback accepts the first supported advertisement and does not correlate
  it with the selected device's address. Nearby AirPods can therefore produce
  battery readings for another pair.
- The core parsers understand case battery values, but D-Bus, the GUI, and the
  kernel bridge currently expose only left and right earbuds.
- The decoder configuration and virtual-source format are fixed. Recognition by
  the battery parser does not guarantee microphone or listening-mode support.

## D-Bus integration

| Setting | Value |
| --- | --- |
| Bus | User session bus |
| Service | `io.github.abdulloh404.AirPods` |
| Object path | `/io/github/abdulloh404/AirPods` |
| Interface | `io.github.abdulloh404.AirPods.Manager1` |

The interface exposes `Status`, `ListDevices`, `SelectDevice`, `StartMic`, `StopMic`,
`SetGain`, `SetLimiterDb`, `SetListeningMode`, and `Battery`. It emits
`StatusChanged`, `DevicesChanged`, and `BatteryChanged` with complete snapshots
of the corresponding data.

Status and battery reads return the daemon's cached state. Clients should use the
[shared contract](crates/airpods-ipc/src/lib.rs) for exact field types and ordering.
Unknown earbud percentages are represented by `-1`. The API currently has no
separate version-negotiation mechanism beyond the `Manager1` interface name.

## Inspecting the running installation

Run these commands as the desktop user running the daemon:

```bash
systemctl --user status airpodsd.service
journalctl --user -u airpodsd.service -n 100 --no-pager
airpodsctl status
airpodsctl mic status
airpodsctl device list
airpodsctl battery
pactl list short sources
```

If the daemon remains in `connecting`, check the Bluetooth connection for the
selected address. If the bridge is unavailable, check that the kernel module is
loaded and the user can write `/dev/airpods_power`. If PipeWire initialization
fails after installation, compare the service environment with the installed
PipeWire library and module locations described above.

## Uninstallation and cleanup

```bash
sudo make uninstall
```

The uninstall target disables and stops the user's service, unloads the battery
module if loaded, removes installed project files and legacy per-user launchers,
and deletes the target user's `airpods-linux` configuration, state, data, and cache
directories. Back up settings first if you need to keep them. The module path used
by the Makefile is for the currently running kernel.

To remove local build outputs:

```bash
make clean
```

## Protocol references and vendored code

The [AACP control-command reference](docs/references/librepods-control-commands.md)
comes from the GPL-3.0 licensed
[`librepods-org/librepods`](https://github.com/librepods-org/librepods)
repository at commit `53679cc90222e94ade84e66542d97ace2540e626`. The larger command
catalog is reference material; the current public control API exposes listening
mode changes rather than every command in that catalog.

The vendored FDK-AAC snapshot is version `v2.0.2`, commit
`801f67f671929311e0c9952c5f92d6e147c7b003`, from
[`mstorsjo/fdk-aac`](https://github.com/mstorsjo/fdk-aac). See the
[version record](third_party/fdk-aac/VENDORED.md) and
[upstream notice](third_party/fdk-aac/NOTICE) for its source and distribution terms.
