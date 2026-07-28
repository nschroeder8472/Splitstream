# Splitstream

A lightweight Windows audio router. Put each app in a group, give every group its
own volume and its own output device, and keep them independent — game to the
headset, music to the speakers, without either app taking exclusive control of a
device.

Conceptually similar to SteelSeries Sonar or VoiceMeeter, with a deliberately
simpler mental model: you think in terms of app-named groups, each routed to an
output you pick.

> **Status:** v0.1, pre-release. It runs for hours on real hardware and the audio
> path is validated, but it has not been through a public release cycle. Expect
> rough edges in the UI before you expect them in the audio.

## Features

- **Per-app groups** — assign running apps to groups by process name, with live
  session discovery and search. Unassigned apps keep working normally.
- **Independent volumes** — per-group gain plus a master, with each group either
  bound to master or independent of it. Per-group mute and solo.
- **Per-group output device** — several groups may target the same device.
- **Never exclusive** — physical endpoints are always opened in shared mode, so
  other software keeps working.
- **Per-group DSP** — graphical EQ, ducking, and an always-on output limiter.
- **Spatial audio** — optional per-group binaural rendering.
- **Level meters** — per-group post-fader and per-output post-limiter.
- **Profiles** — save and switch whole routing setups.
- **Tray-resident** — starts with Windows, global hotkeys, dark/light/system
  themes, and an installer.

## How it works

```
   Apps                  Splitstream                        Devices
┌──────────┐      ┌──────────────────────────┐
│ game.exe │──┐   │  per-pid capture          │
├──────────┤  ├──▶│  → gain → DSP → spatial   │──▶ Game group ──▶ Headset
│ music    │──┘   │  → channel matrix → SRC   │
├──────────┤      │  → mix per output         │──▶ Media group ─▶ Speakers
│ chat     │─────▶│                           │
└──────────┘      └──────────────────────────┘
```

Windows mixes every session on an endpoint into one stream before any external
code can read it, so plain loopback cannot separate one app from another.
Splitstream instead captures **each app's audio directly by process id**, using
`ActivateAudioInterfaceAsync` with `AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK`
— a documented API, not an undocumented redirect hack. Each group's audio is
then processed independently and mixed per output device.

Capture and render run off independent hardware clocks that drift apart over
minutes, so the engine decouples them with elastic buffers and continuously
resamples to compensate. That drift control, not the DSP, is the hard part.

### The silent sink

Splitstream captures a *copy* of each app's audio — the app is still rendering to
whatever endpoint Windows sent it to, so without something to absorb it you would
hear both the original and Splitstream's processed version.

The fix is one silent sink: an output device set as the Windows default that
nobody listens to, so apps render somewhere inaudible and Splitstream's output is
the only thing you hear. It carries no audio anyone listens to and is never a
capture path. Splitstream restores your previous default device on a clean exit.

**A virtual audio device is strongly recommended, but not strictly required.**
Any output you never listen to works — a motherboard line-out with nothing
plugged into it, an unused HDMI audio output, or a second sound card. A virtual
device is simply the tidier option: it can't be unplugged by accident, it costs
you no physical port, and it never plays anything into a room.

If you go virtual, [VB-CABLE](https://vb-audio.com/Cable/) is the default
recommendation, and anything already installed works identically — SteelSeries
Sonar or VoiceMeeter, for instance — since the sink is chosen by device name.

> **VB-CABLE is donationware.** It is free to download and use, and VB-Audio asks
> for a donation if you find it useful. If Splitstream is working for you, a good
> share of the credit belongs to their driver — **please
> [donate to VB-Audio](https://vb-audio.com/Cable/)**. We are not affiliated with
> them and receive nothing from it; we just think people who make the free
> infrastructure everyone builds on should get paid for it.

### Why we don't ship our own virtual endpoint

The obvious question: why not include the sink instead of asking you to install
one?

Because Windows will not let us, cheaply. Creating an audio endpoint requires a
kernel-mode driver, and Windows will not load one that isn't signed through
Microsoft's hardware program. That means an ongoing certificate, hardware-lab
submission, and re-signing every time the driver changes — real recurring cost
and real process overhead, for a component whose entire job is to be silent.
Bundling someone else's driver doesn't avoid this; it just moves the cost and
adds a licensing question on top.

So for now Splitstream is deliberately built around a sink you already have, or a
free one you install once. It's one extra step at setup, and then you never think
about it again.

**If this project finds an audience, we will revisit it.** Shipping our own
endpoint would make the whole thing feel like one product instead of two —
install, run, done, with no third-party driver in the story. That is where we'd
like to end up. We're just a small operation with no financial backing, and that
work has to be justified by enough people actually using this. If that's
something you want, star the repo, file issues, tell people — that's the signal
that makes it worth doing.

## Quick Start

### Prerequisites

- Windows 11, latest version recommended
- An output device to act as the silent sink — a virtual audio device is
  strongly recommended, though any output you never listen to will do (see above)

### Setup

1. Install Splitstream (or `cargo build --release` and run
   `target\release\splitstream.exe`).
2. On first run, pick the device to use as the sink. Splitstream takes it as the
   Windows default and gives it back when you quit.
3. Create groups, pick an output device for each, and assign your running apps.

Settings live in `%APPDATA%\Splitstream\splitstream.toml` and can be edited by
hand — the app watches the file and picks up changes live. Logs are in
`%APPDATA%\Splitstream\logs\`.

### Configuration

| Key | Scope | Description |
|-----|-------|-------------|
| `master` | global | Master volume (0.0–1.0) |
| `muted` | global | Global mute |
| `app.sink_device` | app | Device used as the silent sink |
| `app.manage_default` | app | Whether Splitstream takes the sink as the Windows default |
| `app.autostart` | app | Start with Windows |
| `app.excluded` | app | Apps never routed, left on the system default |
| `app.volume_bind` | app | Group the media keys control |
| `app.theme` / `app.accent` | app | `system`/`dark`/`light`, and accent colour |
| `group.name` | group | Group name |
| `group.output_device` | group | Output device for this group |
| `group.gain` | group | Group volume (0.0–1.0) |
| `group.follow_master` | group | Whether master affects this group |
| `group.spatial` | group | Binaural rendering |
| `group.match_rules` | group | Process names routed into this group |
| `group.muted` | group | Per-group mute |

## Building from Source

```bash
cargo build --release
cargo test --workspace
```

Windows only, with the MSVC toolchain. Developed against Rust 1.93; there is no
formally supported MSRV yet. The installer is built from
`installer/splitstream.iss` with [Inno Setup](https://jrsoftware.org/isinfo.php).

### Workspace layout

| Crate | Responsibility |
|-------|----------------|
| `audio-core` | Mixing, resampling, DSP, metering — no Windows APIs, fully unit-tested |
| `engine` | Audio graph, threads, drift control, flow control |
| `win-audio` | WASAPI capture, render, and device enumeration |
| `win-shell` | Tray, shell, and process integration |
| `control` | Routing rules, config, and profiles |
| `app` | UI and wiring |

### Diagnosing audio problems

`SPLITSTREAM_AUDIT=1` emits one line per second with the flow-control state —
ring fills, applied resample ratio, per-group and per-output peaks, drop and xrun
counters, and why an output ring last rejected a push:

```powershell
$env:SPLITSTREAM_AUDIT = "1"
target\release\splitstream.exe
```

If you are reporting anything audible, please include 15–20 consecutive lines
captured while it is happening. Those counters distinguish starvation from
truncation from a routing miss; descriptions of the sound alone do not.

## Documentation

- `Splitstream-Engineering-Spec.md` — architecture, component boundaries, runtime
  model, and the Windows-audio constraints that drive the design
- `.lattice/context/` — per-feature design records, including the measurements
  and dead ends behind each decision

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for development setup and guidelines.

## License

[MIT](LICENSE)
