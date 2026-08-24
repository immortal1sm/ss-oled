# ss-oled — fork of [not-jan/apex-tux](https://github.com/not-jan/apex-tux)

> **Note:** This is **ss-oled**, a fork of [not-jan/apex-tux](https://github.com/not-jan/apex-tux).
> All credit for the upstream codebase goes to **not-jan** and contributors. This fork
> diverges in design direction by adding event-driven provider switching, per-provider
> dwell times, and a reworked MPRIS layout.
>
> Upstream `not-jan/apex-tux` remains the source of truth for the OLED protocol,
> the renderer pipeline, and the provider framework. This fork layers new
> scheduling behavior on top of that foundation.

## What this fork adds (vs upstream)

1. **Event-driven MPRIS jumps** — when a track changes or playback resumes, the OLED
   immediately switches to the music screen instead of waiting for the rotation timer.
   Pause/stop do not trigger the jump.
2. **Per-provider dwell times** — the clock can sit for 5s while MPRIS and Sysinfo sit
   for 30s, configured via `interval.<name>` keys in settings.toml.
3. **Reworked MPRIS layout** — adds an elapsed/total timer row between the artist
   and the progress bar (e.g. `1:23 / 3:45`).
4. **Image provider enabled out of the box** — no need to pass `--features image`
   manually; the bundled `settings.toml` enables it.

Upstream-specific behavior (hardware protocol, providers framework, image renderer,
DBus notification pipeline, simulator) is unchanged.

---

# apex-tux - Linux support for the Apex series OLED screens

Make use of your OLED screen instead of letting the SteelSeries logo burn itself in :-)

## Screenshots

![Music Player in Simulator](./resources/simulator-music.png)
![Clock  in Simulator](./resources/simulator-clock.png)
![Music Player in device OLED screen](./resources/music.png)
![Sysinfo in device OLED screen](./resources/system-metrics.png)

## Features

- Music player integration (requires DBus)
- Discord notifications (requires DBus)
- Clock
- System metrics
- Scrolling text
- No burn-in from constantly displaying a static image

## Supported media players

- [Lollypop](https://gitlab.gnome.org/World/lollypop) (tested)
- Firefox (Results may vary)
- Chromium / Chrome (Results may vary)
- mpv
- Telegram
- VLC
- Spotify

Source: [Arch Wiki](https://wiki.archlinux.org/title/MPRIS#Supported_clients)

## Supported devices

This currently supports the following devices:

- Apex Pro
- Apex 5
- Apex 7
- Apex Pro TKL Wireless Gen 3 (wired and wireless)

Other devices may be compatible and all that is needed is to add the ID to apex-hardware/src/usb.rs.

## Installation

For installing this software, follow these steps:

### UDev

1. Get the device id: `lsusb | grep "SteelSeries Apex"`:

```shell
$ lsusb | grep "SteelSeries Apex"
Bus 001 Device 002: ID 1038:1610 SteelSeries ApS SteelSeries Apex Pro
```

The **id** is the right part of the ID.

2. Enter the following data from [here](https://gist.github.com/ToadKing/d26f8f046a3b707e9e4b9821be5c9efc) (Shoutout [to @ToadKing](https://github.com/ToadKing)).

If those don't work and lead to an "Access denied" error please try the following rules and save the rules as `97-steelseries.rules`:

```shell
cat /etc/udev/rules.d/97-steelseries.rules
SUBSYSTEM=="input", GROUP="input", MODE="0666"

SUBSYSTEM=="usb", ATTRS{idVendor}=="1038", ATTRS{idProduct}=="<PRODUCT ID HERE>", MODE="0666", GROUP="plugdev"
KERNEL=="hidraw*", ATTRS{idVendor}=="1038", ATTRS{idProduct}=="<PRODUCT ID HERE>", MODE="0666", GROUP="plugdev"
```

1. Replace the `ATTRS{idProduct}==` value with the device **id**.

2. Save all files to `/etc/udev/rules.d/97-steelseries.rules`.

3. Finally, reload the `udev` rules: `sudo udevadm control --reload && sudo udevadm trigger`

### Rust

- Install Rust **nightly** using [rustup](https://rustup.rs/)
- Install required dependencies
  - For Ubuntu: `sudo apt install libssl-dev libdbus-1-dev libusb-1.0-0-dev`
- Clone the repository: `git clone git@github.com:not-jan/apex-tux.git`
- Change the directory into the repository: `cd apex-tux`
- Compile the app using the features you want
  - If you **don't** run DBus you have to disable the dbus feature: `cargo build --release --no-default-features --features crypto,usb`
  - Otherwise just run `cargo build --release --features sysinfo,hotkeys,image`
  - If you **don't** have an Apex device around at the moment or want to develop more easily you can enable the simulator: `cargo build --release --no-default-features --features crypto,clock,dbus-support,simulator`

## Configuration

The default configuration is in `settings.toml`.
This repository ships with a default configuration that covers most parts and contains documentation for the important keys.
The program will look for configuration first in the platform-specific `$USER_CONFIG_DIR/apex-tux/`, then in the current working directory.
You can also override specific settings with `APEX_*` environment variables.

You can also run the software to find errors on configuration and to decide what is the right setup you need:

```shell
$ target/release/apex-tux
23:43:05 [INFO] Registering MPRIS2 display source.
23:43:05 [INFO] Registering Sysinfo display source.
23:43:05 [WARN] Couldn't find network interface `eth0`
23:43:05 [INFO] Instead, found those interfaces:
23:43:05 [INFO]         lo
23:43:05 [INFO]         wlp3s0
23:43:05 [INFO]         enp2s0
23:43:05 [INFO]         docker0
23:43:05 [WARN] Couldn't find sensor `hwmon0 CPU Temperature`
23:43:05 [INFO] Instead, found those sensors:
23:43:05 [INFO]         acpitz temp1: 67°C (max: 67°C / critical: 120°C)
23:43:05 [INFO]         amdgpu edge: 47°C (max: 47°C)
23:43:05 [INFO]         iwlwifi_1 temp1: 39°C (max: 39°C)
23:43:05 [INFO]         k10temp Tctl: 66.5°C (max: 66.5°C)
23:43:05 [INFO]         nvme Composite HFM001TD3JX013N temp1: 36.85°C (max: 36.85°C / critical: 84.85°C)
23:43:05 [INFO]         nvme Composite Samsung SSD 980 PRO 1TB temp1: 32.85°C (max: 32.85°C / critical: 84.85°C)
23:43:05 [INFO]         nvme Sensor 1 HFM001TD3JX013N temp2: 36.85°C (max: 36.85°C)
23:43:05 [INFO]         nvme Sensor 1 Samsung SSD 980 PRO 1TB temp2: 32.85°C (max: 32.85°C)
23:43:05 [INFO]         nvme Sensor 2 HFM001TD3JX013N temp3: 43.85°C (max: 43.85°C)
23:43:05 [INFO]         nvme Sensor 2 Samsung SSD 980 PRO 1TB temp3: 38.85°C (max: 38.85°C)
23:43:05 [INFO] Registering Clock display source.
23:43:05 [INFO] Registering Gif display source. 
23:43:05 [INFO] Registering DBUS notification source.
23:43:05 [INFO] Found 5 registered providers
23:43:05 [INFO] Trying to connect to DBUS with player preference: Some("spotify")
23:43:05 [INFO] Trying to connect to DBUS with player preference: Some("spotify")
23:43:05 [INFO] Connected to music player: "org.mpris.MediaPlayer2.spotify"
```

In our case we need to set a right value for the sensor(`acpitz temp1`, critical temperatured one, i.e., cpu) and the network interface(`wlp3s0`, wifi) in the `[sysinfo]` section.

You can set your default media player on the `[mpris2]` section.


## Behavior (ss-oled fork)

By default (matching upstream), the OLED cycles through enabled providers
on a fixed timer — every 30 seconds, configurable via `[interval] refresh`.

**ss-oled adds two pieces of behavior on top of that:**

### 1. Per-provider dwell times

You can override the dwell time for any provider individually. Set
`[interval] clock = 5` to make the clock flash for just 5 seconds while
the rest keep the global 30s default.

```toml
[interval]
refresh = 30      # global fallback
clock = 5         # clock shows for just 5s
```

A value of 0 means "do not auto-rotate this provider away".

### 2. Event-driven MPRIS jumps

When using the MPRIS2 provider, the OLED automatically switches to it
whenever a new track starts or playback resumes from a paused state.
Pause and stop do NOT trigger the jump — the screen continues showing
whatever provider was active and rotates normally.

This means: staring at the clock or sysinfo while music plays, then
hitting "next track" or "play", immediately jumps the OLED to the music
screen instead of waiting up to 30 seconds for the timer.


## Usage

Simply run the binary under `target/release/apex-tux` and make sure the settings.toml is in your current directory.
The output should look something like this:

```shell
23:18:14 [INFO] Registering Clock display source.
23:18:14 [INFO] Registering MPRIS2 display source.
23:18:14 [INFO] Registering DBUS notification source.
23:18:14 [INFO] Found 3 registered providers
23:18:14 [INFO] Trying to connect to DBUS with player preference: Some("Lollypop")
23:18:18 [INFO] Trying to connect to DBUS with player preference: Some("Lollypop")
23:18:18 [INFO] Connected to music player: "org.mpris.MediaPlayer2.Lollypop"
23:34:01 [INFO] Ctrl + C received, shutting down!
23:34:01 [INFO] unregister hotkey ALT+SHIFT+A
23:34:01 [INFO] unregister hotkey ALT+SHIFT+D
```

You may change sources by pressing **Alt+Shift+A** or **Alt+Shift+D** (This might not work on Wayland). The simulator uses the arrow keys.

## Autostarting

Hotkey support requires autostarting under an interactive daemon, i.e. by your Desktop Environment. You can also build without the `hotkey` feature and run it as a systemd service.

### Desktop Environment

To start on boot the binary must be started under an interactive daemon, i.e. by your Desktop Environment. A systemd service will fail unless compiled without hotkey support. Most DEs support the following method/path but you may have to find your equivalent.

- Create `apex-tux.desktop` in `~/.config/autostart`  
- Edit `apex-tux.desktop` to contain:
```shell
[Desktop Entry]
Exec=/path/to/apex-tux/apex-tux
Name=apex-tux
Path=/path/to/apex-tux
Terminal=true
Type=Application
```
- Replace path to the apex-tux executable accordingly

### Systemd service

- Create `apex-tux.service` in `/lib/systemd/system/`
- Edit `apex-tux.service` to contain:
```ini
[Unit]
Description=Linux support for the Apex series OLED screens
After=multi-user.target

[Service]
Type=simple
ExecStart=/usr/bin/bash -c '( cd /path/to/apex-tux; /path/to/apex-tux/target/release/apex-tux ; )'
Restart=on-abort
User=YOUR_USERNAME

[Install]
WantedBy=multi-user.target
```
- Replace `YOUR_USERNAME` and `/path/to/apex-tux` path accordingly
- Enable and start the systemd service by running: `systemctl enable --now apex-tux.service`


## Development

If you have a feature to add or a bug to fix please feel free to open an issue or submit a pull request.

## TODO

- Windows support
- Test this on more than one Desktop Environment on X11
- More providers
  - Games?
  - GIFs?
- Change the USB crate to something async instead
- Add documentation on how to add custom providers
- Switch from GATs to async traits once they're here
- Add support for more notifications
- Package this up for Debian/Arch/Flatpak etc.

## Windows support ETA, when?

I've written a stub for SteelSeries Engine support on Windows, there is an [API for mediaplayer metadata](https://microsoft.github.io/windows-docs-rs/doc/windows/Media/Control/struct.GlobalSystemMediaTransportControlsSessionManager.html) but my time is kind of limited and I don't run Windows all that often.
It will happen eventually but it's not a priority.

## Why nightly Rust?

Way too many cool features to pass up on :D
