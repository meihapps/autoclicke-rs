# autoclicke-rs

A Linux GUI autoclicker built with [egui](https://github.com/emilk/egui) and [evdev](https://github.com/emberian/evdev).

## Features

- Click interval — set how fast to click (ms / s / min)
- Button selector — left, right, or both
- Clicking duration — auto-stop after a time limit or a click count
- Still threshold — only click when the mouse hasn't moved for a set time
- Global hotkey — capture any key combo to toggle clicking from anywhere
- Manual start/stop button in the UI

## Requirements

- Linux (Wayland). X11 may work but is untested.
- `uinput` support
- Membership in the `input` group (for `/dev/input/event*` and `/dev/uinput` access)

```sh
sudo usermod -aG input $USER  # log out and back in after
```

The app will show an error message with hints if permissions are missing.

## Install

```sh
cargo install autoclicke-rs
```

Or build from source:

```sh
git clone https://github.com/meihapps/autoclicke-rs
cd autoclicke-rs
cargo build --release
./target/release/autoclicke-rs
```

## Usage

Launch the app and configure your settings in the window:

1. Pick a **button** (Left / Right / Both)
2. Set a **click interval**
3. Optionally enable a **duration limit** (time or click count)
4. Optionally enable a **still threshold** so clicking only fires while the mouse is idle
5. Set a **hotkey** by clicking the hotkey button and pressing your desired key combo (default: F8)
6. Hit **Start** or press your hotkey to begin
