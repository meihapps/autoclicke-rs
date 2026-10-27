# autoclicke-rs

A Linux GUI autoclicker built with [egui](https://github.com/emilk/egui) and [evdev](https://github.com/emberian/evdev).

## Features

- Click rate: clicks per second, or an interval in ms / s / min, up to 1000 CPS
- Accurate timing: clicks are scheduled against a fixed timeline, so the real rate matches the one you set
- Hold time: how long each click holds the button down (default 10 ms), so games that check button state once per frame don't miss clicks
- Randomized timing: vary each interval by up to ±50%
- Button selector: left, middle, right, or left+right together
- Stop after: auto-stop after a time limit or a click count
- Still threshold: only click when the mouse (or touchpad) hasn't moved for a set time
- Global hotkey: any key combo, including mouse side buttons, that works even while you're holding other keys
- Hotkey modes: press to toggle, or hold to click
- Hotplug: keyboards and mice plugged in (or reconnected after suspend) after launch are picked up automatically
- Settings are saved to `~/.config/autoclicke-rs/config`

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

1. Pick a **button** (Left / Middle / Right / L+R)
2. Set a **click rate** (CPS or an interval; switching units converts the value)
3. Adjust the **hold time** if needed. Raise it if a game misses clicks; it's automatically capped at half the interval
4. Optionally enable **randomized timing**, a **stop after** limit (time or click count), or a **still threshold** so clicking only fires while the mouse is idle
5. Set a **hotkey** by clicking the hotkey button and pressing your key combo (default: F8). Esc cancels. Mouse side buttons work too
6. Choose **Press to toggle** or **Hold to click**
7. Hit **Start** or use your hotkey
