use eframe::egui;
use evdev::uinput::{VirtualDevice, VirtualDeviceBuilder};
use evdev::{
    AbsoluteAxisType, AttributeSet, Device, EventType, InputEvent, InputEventKind, Key,
    RelativeAxisType,
};
use parking_lot::Mutex;
use std::{
    collections::{HashMap, HashSet},
    io,
    ops::RangeInclusive,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::SeqCst},
        Arc,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const APP_TITLE: &str = "autoclicke-rs";
const SYN_REPORT: u16 = 0; // evdev SynchronizationCode::SYN_REPORT

// Pause after pressing the in-app Start button so the click on the button itself
// isn't immediately followed by an autoclick on it.
const DELAYED_START: Duration = Duration::from_millis(300);
const IDLE_POLL: Duration = Duration::from_millis(5);
// How often /dev/input is rescanned for newly plugged-in devices.
const RESCAN_INTERVAL: Duration = Duration::from_secs(2);
// Guards against one physical press toggling twice if a key shows up on two devices.
const HOTKEY_DEBOUNCE: Duration = Duration::from_millis(150);
// Settings are written to disk this long after the last change.
const SAVE_DELAY: Duration = Duration::from_millis(500);
// The last stretch before a deadline is spent yielding instead of sleeping, because
// thread::sleep on Linux tends to overshoot by ~50-100µs.
const SPIN_WINDOW: Duration = Duration::from_micros(300);
// A click's hold time is capped at this fraction of the interval so there's always
// a gap between release and the next press.
const MAX_HOLD_FRACTION: f64 = 0.5;
const MIN_PERIOD_MS: f64 = 1.0;

const RATE_UNITS: &[Unit] = &[Unit::Cps, Unit::Ms, Unit::Sec, Unit::Min];
const TIME_UNITS: &[Unit] = &[Unit::Ms, Unit::Sec, Unit::Min];
const LIMIT_UNITS: &[Unit] = &[Unit::Ms, Unit::Sec, Unit::Min, Unit::Clicks];

// ── Shared state between threads ─────────────────────────────────────────────

struct SharedState {
    clicking: AtomicBool,
    shutdown: AtomicBool,
    capturing: AtomicBool,
    delayed_start: AtomicBool,
    session_clicks: AtomicU64,
    device_count: AtomicUsize,
    scanned_once: AtomicBool,
    last_moved: Mutex<Instant>,
    last_toggle: Mutex<Option<Instant>>,
    // Keys currently held across all devices, with a count in case the same key is
    // down on two devices at once. Shared so combos spanning devices work.
    held_keys: Mutex<HashMap<Key, u32>>,
    click_error: Mutex<String>,
}

impl SharedState {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            clicking: AtomicBool::new(false),
            shutdown: AtomicBool::new(false),
            capturing: AtomicBool::new(false),
            delayed_start: AtomicBool::new(false),
            session_clicks: AtomicU64::new(0),
            device_count: AtomicUsize::new(0),
            scanned_once: AtomicBool::new(false),
            last_moved: Mutex::new(Instant::now()),
            last_toggle: Mutex::new(None),
            held_keys: Mutex::new(HashMap::new()),
            click_error: Mutex::new(String::new()),
        })
    }
}

// ── Config types ──────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Debug)]
enum ClickButton {
    Left,
    Middle,
    Right,
    Both,
}

impl ClickButton {
    const ALL: [ClickButton; 4] = [Self::Left, Self::Middle, Self::Right, Self::Both];

    fn label(self) -> &'static str {
        match self {
            Self::Left => "Left",
            Self::Middle => "Middle",
            Self::Right => "Right",
            Self::Both => "L+R",
        }
    }

    fn keys(self) -> &'static [Key] {
        match self {
            Self::Left => &[Key::BTN_LEFT],
            Self::Middle => &[Key::BTN_MIDDLE],
            Self::Right => &[Key::BTN_RIGHT],
            Self::Both => &[Key::BTN_LEFT, Key::BTN_RIGHT],
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Middle => "middle",
            Self::Right => "right",
            Self::Both => "both",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|b| b.as_str() == s)
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Unit {
    Cps,
    Ms,
    Sec,
    Min,
    Clicks,
}

impl Unit {
    const ALL: [Unit; 5] = [Self::Cps, Self::Ms, Self::Sec, Self::Min, Self::Clicks];

    fn label(self) -> &'static str {
        match self {
            Self::Cps => "CPS",
            Self::Ms => "ms",
            Self::Sec => "s",
            Self::Min => "min",
            Self::Clicks => "clicks",
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Cps => "cps",
            Self::Ms => "ms",
            Self::Sec => "s",
            Self::Min => "min",
            Self::Clicks => "clicks",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|u| u.as_str() == s)
    }

    /// Whether a value in this unit describes a span of time (CPS counts: it's a period).
    fn is_time(self) -> bool {
        self != Self::Clicks
    }

    fn range(self) -> RangeInclusive<f64> {
        match self {
            Self::Cps => 0.01..=1000.0,
            Self::Ms | Self::Clicks => 1.0..=999_999.0,
            Self::Sec | Self::Min => 0.001..=999_999.0,
        }
    }

    fn decimals(self) -> usize {
        match self {
            Self::Clicks => 0,
            Self::Cps | Self::Ms => 2,
            Self::Sec | Self::Min => 3,
        }
    }

    /// Converts a span in milliseconds into a value in this unit.
    fn value_for_millis(self, ms: f64) -> f64 {
        match self {
            Self::Cps => 1000.0 / ms.max(1e-9),
            Self::Ms | Self::Clicks => ms,
            Self::Sec => ms / 1_000.0,
            Self::Min => ms / 60_000.0,
        }
    }
}

#[derive(Clone, PartialEq, Debug)]
struct Setting {
    enabled: bool,
    value: f64,
    unit: Unit,
}

impl Setting {
    fn new(enabled: bool, value: f64, unit: Unit) -> Self {
        Self {
            enabled,
            value,
            unit,
        }
    }

    fn millis(&self) -> f64 {
        match self.unit {
            Unit::Cps => 1000.0 / self.value.max(1e-9),
            Unit::Ms => self.value,
            Unit::Sec => self.value * 1_000.0,
            Unit::Min => self.value * 60_000.0,
            Unit::Clicks => 0.0,
        }
    }

    /// Switch units, converting the value so the same rate/duration is kept.
    fn change_unit(&mut self, unit: Unit) {
        if self.unit.is_time() && unit.is_time() {
            self.value = unit.value_for_millis(self.millis());
        }
        self.unit = unit;
        self.clamp();
    }

    fn clamp(&mut self) {
        let range = self.unit.range();
        if !self.value.is_finite() {
            self.value = *range.start();
        }
        self.value = self.value.clamp(*range.start(), *range.end());
    }
}

#[derive(Clone, PartialEq, Debug)]
struct Config {
    button: ClickButton,
    rate: Setting,
    hold_ms: f64,
    jitter_enabled: bool,
    jitter_pct: f64,
    limit: Setting,
    still: Setting,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            button: ClickButton::Left,
            rate: Setting::new(true, 10.0, Unit::Cps),
            hold_ms: 10.0,
            jitter_enabled: false,
            jitter_pct: 10.0,
            limit: Setting::new(false, 1.0, Unit::Min),
            still: Setting::new(false, 1.0, Unit::Sec),
        }
    }
}

impl Config {
    fn period_ms(&self) -> f64 {
        self.rate.millis().max(MIN_PERIOD_MS)
    }

    fn jitter_fraction(&self) -> f64 {
        if self.jitter_enabled {
            (self.jitter_pct / 100.0).clamp(0.0, 0.5)
        } else {
            0.0
        }
    }

    /// How long to hold the button down for a click with the given period.
    fn hold_for(&self, period_ms: f64) -> f64 {
        self.hold_ms.min(period_ms * MAX_HOLD_FRACTION).max(0.0)
    }

    fn sanitize(&mut self) {
        self.rate.clamp();
        self.limit.clamp();
        self.still.clamp();
        if !self.hold_ms.is_finite() {
            self.hold_ms = Config::default().hold_ms;
        }
        self.hold_ms = self.hold_ms.clamp(0.0, 1000.0);
        if !self.jitter_pct.is_finite() {
            self.jitter_pct = Config::default().jitter_pct;
        }
        self.jitter_pct = self.jitter_pct.clamp(1.0, 50.0);
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum HotkeyMode {
    Toggle,
    Hold,
}

#[derive(Clone, PartialEq, Debug)]
struct Hotkey {
    keys: Vec<Key>,
    mode: HotkeyMode,
}

impl Default for Hotkey {
    fn default() -> Self {
        Self {
            keys: vec![Key::KEY_F8],
            mode: HotkeyMode::Toggle,
        }
    }
}

// ── Settings file ─────────────────────────────────────────────────────────────

fn config_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join(APP_TITLE).join("config"))
}

fn serialize_config(c: &Config, h: &Hotkey) -> String {
    let flag = |b: bool| if b { "1" } else { "0" };
    let codes: Vec<String> = h.keys.iter().map(|k| k.code().to_string()).collect();
    let mode = match h.mode {
        HotkeyMode::Toggle => "toggle",
        HotkeyMode::Hold => "hold",
    };
    format!(
        "button={}\n\
         rate={}\nrate_unit={}\n\
         hold_ms={}\n\
         jitter_enabled={}\njitter_pct={}\n\
         limit_enabled={}\nlimit={}\nlimit_unit={}\n\
         still_enabled={}\nstill={}\nstill_unit={}\n\
         hotkey={}\nhotkey_mode={}\n",
        c.button.as_str(),
        c.rate.value,
        c.rate.unit.as_str(),
        c.hold_ms,
        flag(c.jitter_enabled),
        c.jitter_pct,
        flag(c.limit.enabled),
        c.limit.value,
        c.limit.unit.as_str(),
        flag(c.still.enabled),
        c.still.value,
        c.still.unit.as_str(),
        codes.join(","),
        mode,
    )
}

/// Parses the settings file. Unknown keys and bad values are ignored so an old or
/// hand-edited file never stops the app from starting.
fn parse_config(text: &str) -> (Config, Hotkey) {
    let mut c = Config::default();
    let mut h = Hotkey::default();
    let num = |v: &str| v.parse::<f64>().ok().filter(|x| x.is_finite());
    let flag = |v: &str| matches!(v, "1" | "true");

    // Units are applied before values so the value isn't clamped to the old unit's range.
    let lines: Vec<(&str, &str)> = text
        .lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim(), v.trim()))
        .collect();

    for &(key, val) in &lines {
        match key {
            "rate_unit" => {
                if let Some(u) = Unit::parse(val).filter(|u| RATE_UNITS.contains(u)) {
                    c.rate.unit = u;
                }
            }
            "limit_unit" => {
                if let Some(u) = Unit::parse(val).filter(|u| LIMIT_UNITS.contains(u)) {
                    c.limit.unit = u;
                }
            }
            "still_unit" => {
                if let Some(u) = Unit::parse(val).filter(|u| TIME_UNITS.contains(u)) {
                    c.still.unit = u;
                }
            }
            _ => {}
        }
    }

    for &(key, val) in &lines {
        match key {
            "button" => {
                if let Some(b) = ClickButton::parse(val) {
                    c.button = b;
                }
            }
            "rate" => c.rate.value = num(val).unwrap_or(c.rate.value),
            "hold_ms" => c.hold_ms = num(val).unwrap_or(c.hold_ms),
            "jitter_enabled" => c.jitter_enabled = flag(val),
            "jitter_pct" => c.jitter_pct = num(val).unwrap_or(c.jitter_pct),
            "limit_enabled" => c.limit.enabled = flag(val),
            "limit" => c.limit.value = num(val).unwrap_or(c.limit.value),
            "still_enabled" => c.still.enabled = flag(val),
            "still" => c.still.value = num(val).unwrap_or(c.still.value),
            "hotkey" => {
                let keys: Option<Vec<Key>> = val
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(|s| s.parse::<u16>().ok().map(Key::new))
                    .collect();
                if let Some(keys) = keys {
                    h.keys = keys;
                }
            }
            "hotkey_mode" => {
                h.mode = match val {
                    "hold" => HotkeyMode::Hold,
                    _ => HotkeyMode::Toggle,
                }
            }
            _ => {}
        }
    }

    c.sanitize();
    (c, h)
}

fn load_config() -> (Config, Hotkey) {
    match config_path().and_then(|p| std::fs::read_to_string(p).ok()) {
        Some(text) => parse_config(&text),
        None => (Config::default(), Hotkey::default()),
    }
}

fn save_config(c: &Config, h: &Hotkey) -> io::Result<()> {
    let path = config_path()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no HOME or XDG_CONFIG_HOME"))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serialize_config(c, h))?;
    std::fs::rename(tmp, path)
}

// ── Timing helpers ────────────────────────────────────────────────────────────

fn ms(v: f64) -> Duration {
    Duration::from_secs_f64((v / 1000.0).max(0.0))
}

/// Small xorshift RNG; plenty for timing jitter and avoids an extra dependency.
struct Rng(u64);

impl Rng {
    fn seeded() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0x9E37_79B9_7F4A_7C15, |d| {
                u64::from(d.subsec_nanos()) ^ d.as_secs().rotate_left(30)
            });
        Self(nanos | 1)
    }

    /// Uniform in [0, 1).
    fn next_f64(&mut self) -> f64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        // Put 52 random bits in the mantissa of a float in [1, 2), then shift down.
        f64::from_bits(0x3FF0_0000_0000_0000 | (x >> 12)) - 1.0
    }

    /// A multiplier in [1 - fraction, 1 + fraction].
    fn jitter(&mut self, fraction: f64) -> f64 {
        if fraction <= 0.0 {
            return 1.0;
        }
        1.0 + fraction * (2.0 * self.next_f64() - 1.0)
    }
}

/// Sleeps until `deadline` as precisely as practical: a normal sleep for most of
/// the wait, then yielding for the last few hundred microseconds.
fn precise_sleep_until(deadline: Instant) {
    loop {
        let now = Instant::now();
        if now >= deadline {
            return;
        }
        let remaining = deadline.saturating_duration_since(now);
        if remaining > SPIN_WINDOW {
            thread::sleep(remaining.saturating_sub(SPIN_WINDOW));
        } else {
            thread::yield_now();
        }
    }
}

/// Waits until `deadline`, returning `false` early if clicking gets switched off.
fn wait_while_clicking(state: &SharedState, deadline: Instant) -> bool {
    const CHUNK: Duration = Duration::from_millis(10);
    loop {
        if !state.clicking.load(SeqCst) || state.shutdown.load(SeqCst) {
            return false;
        }
        let now = Instant::now();
        if now >= deadline {
            return true;
        }
        if deadline.saturating_duration_since(now) > CHUNK + SPIN_WINDOW {
            thread::sleep(CHUNK);
        } else {
            precise_sleep_until(deadline);
            return state.clicking.load(SeqCst);
        }
    }
}

// ── evdev helpers ─────────────────────────────────────────────────────────────

/// Keys that can be part of the hotkey: keyboard keys plus the middle and side
/// mouse buttons. Left/right click are excluded (you click the capture button with
/// them), as are touchpad, tablet and gamepad buttons.
fn hotkey_eligible(k: Key) -> bool {
    let code = k.code();
    // 0x112..=0x117 is BTN_MIDDLE through BTN_TASK (middle, side and extra buttons).
    code < 0x100 || (0x112..=0x117).contains(&code) || code >= 0x160
}

#[derive(Clone, Copy)]
struct DeviceKind {
    /// Relative pointer (mouse, trackball, trackpoint).
    rel: bool,
    /// Absolute pointer we treat as mouse movement (touchpad, pen tablet).
    abs: bool,
    /// Has keys/buttons usable for the hotkey.
    hotkeys: bool,
}

fn classify(dev: &Device) -> Option<DeviceKind> {
    // Never listen to our own virtual clicker.
    if dev.name() == Some(APP_TITLE) {
        return None;
    }
    let keys = dev.supported_keys();
    let rel = dev
        .supported_relative_axes()
        .is_some_and(|axes| axes.contains(RelativeAxisType::REL_X));
    // Accelerometers also report ABS_X, so only count devices that look like a
    // touchpad or pen.
    let abs = dev
        .supported_absolute_axes()
        .is_some_and(|axes| axes.contains(AbsoluteAxisType::ABS_X))
        && keys.is_some_and(|k| k.contains(Key::BTN_TOOL_FINGER) || k.contains(Key::BTN_TOOL_PEN));
    let hotkeys = keys.is_some_and(|k| k.iter().any(hotkey_eligible));
    if rel || abs || hotkeys {
        Some(DeviceKind { rel, abs, hotkeys })
    } else {
        None
    }
}

// ── uinput helpers ───────────────────────────────────────────────────────────

fn create_clicker_device() -> io::Result<VirtualDevice> {
    let mut keys = AttributeSet::<Key>::new();
    for k in [Key::BTN_LEFT, Key::BTN_RIGHT, Key::BTN_MIDDLE] {
        keys.insert(k);
    }
    // We never move the pointer, but libinput only treats a device as a mouse if it
    // has relative axes; without them some compositors ignore the clicks.
    let mut axes = AttributeSet::<RelativeAxisType>::new();
    axes.insert(RelativeAxisType::REL_X);
    axes.insert(RelativeAxisType::REL_Y);
    VirtualDeviceBuilder::new()?
        .name(APP_TITLE)
        .with_keys(&keys)?
        .with_relative_axes(&axes)?
        .build()
}

fn emit_buttons(device: &mut VirtualDevice, keys: &[Key], value: i32) -> io::Result<()> {
    let mut events: Vec<InputEvent> = keys
        .iter()
        .map(|k| InputEvent::new(EventType::KEY, k.code(), value))
        .collect();
    events.push(InputEvent::new(EventType::SYNCHRONIZATION, SYN_REPORT, 0));
    device.emit(&events)
}

// ── Clicker helpers ───────────────────────────────────────────────────────────

fn is_still_enough(threshold: &Setting, last_moved: &Mutex<Instant>) -> bool {
    if !threshold.enabled {
        return true;
    }
    let required = threshold.millis();
    required <= 0.0 || last_moved.lock().elapsed() >= ms(required)
}

#[expect(
    clippy::cast_precision_loss,
    reason = "click counts stay far below 2^52, where f64 is exact"
)]
fn limit_reached(limit: &Setting, session_start: Instant, session_clicks: u64) -> bool {
    if !limit.enabled {
        return false;
    }
    match limit.unit {
        Unit::Clicks => session_clicks as f64 >= limit.value,
        _ => session_start.elapsed() >= ms(limit.millis()),
    }
}

// ── Background threads ────────────────────────────────────────────────────────

fn release_held(held: &mut HashMap<Key, u32>, k: Key) {
    if let Some(count) = held.get_mut(&k) {
        *count = count.saturating_sub(1);
        if *count == 0 {
            held.remove(&k);
        }
    }
}

// Lock order everywhere: held_keys → hotkey → last_toggle.
fn on_key_down(k: Key, state: &SharedState, hotkey: &Mutex<Hotkey>, ctx: &egui::Context) {
    let mut held = state.held_keys.lock();
    *held.entry(k).or_insert(0) += 1;

    if state.capturing.load(SeqCst) {
        if k == Key::KEY_ESC
            && state
                .capturing
                .compare_exchange(true, false, SeqCst, SeqCst)
                .is_ok()
        {
            ctx.request_repaint();
        }
        return;
    }

    // Fire when the key just pressed completes the combo. Other held keys don't
    // matter, so the hotkey still works while you're holding W or a mouse button.
    let hk = hotkey.lock();
    if !hk.keys.contains(&k) || !hk.keys.iter().all(|key| held.contains_key(key)) {
        return;
    }
    match hk.mode {
        HotkeyMode::Toggle => {
            let mut last = state.last_toggle.lock();
            if last.is_none_or(|t| t.elapsed() >= HOTKEY_DEBOUNCE) {
                *last = Some(Instant::now());
                state.clicking.fetch_xor(true, SeqCst);
            }
        }
        HotkeyMode::Hold => state.clicking.store(true, SeqCst),
    }
    ctx.request_repaint();
}

fn on_key_up(
    k: Key,
    state: &SharedState,
    hotkey: &Mutex<Hotkey>,
    ctx: &egui::Context,
    allow_capture: bool,
) {
    let mut held = state.held_keys.lock();

    // Capture finishes on the first release, taking everything held at that moment.
    if allow_capture && state.capturing.load(SeqCst) {
        let mut combo: Vec<Key> = held.keys().copied().collect();
        if !combo.contains(&k) {
            combo.push(k);
        }
        if state
            .capturing
            .compare_exchange(true, false, SeqCst, SeqCst)
            .is_ok()
        {
            combo.sort_by_key(|k| key_order(*k));
            hotkey.lock().keys = combo;
            ctx.request_repaint();
        }
        release_held(&mut held, k);
        return;
    }

    release_held(&mut held, k);
    let hk = hotkey.lock();
    if hk.mode == HotkeyMode::Hold && hk.keys.contains(&k) && state.clicking.swap(false, SeqCst) {
        ctx.request_repaint();
    }
}

fn watch_device(
    mut dev: Device,
    kind: DeviceKind,
    state: &SharedState,
    hotkey: &Mutex<Hotkey>,
    ctx: &egui::Context,
) {
    let mut held_here: HashSet<Key> = HashSet::new();
    while !state.shutdown.load(SeqCst) {
        // An error means the device was unplugged; the scanner reopens it if it returns.
        let Ok(events) = dev.fetch_events() else {
            break;
        };
        for ev in events {
            match ev.kind() {
                InputEventKind::RelAxis(_) if kind.rel => {
                    *state.last_moved.lock() = Instant::now();
                }
                InputEventKind::AbsAxis(_) if kind.abs => {
                    *state.last_moved.lock() = Instant::now();
                }
                InputEventKind::Key(k) if kind.hotkeys && hotkey_eligible(k) => {
                    // value 1 = down, 0 = up, 2 = autorepeat (ignored). The set filters
                    // out duplicate downs/ups for the same key on this device.
                    match ev.value() {
                        1 if held_here.insert(k) => on_key_down(k, state, hotkey, ctx),
                        0 if held_here.remove(&k) => on_key_up(k, state, hotkey, ctx, true),
                        _ => {}
                    }
                }
                _ => {}
            }
        }
    }
    // The device went away with keys down: release them so they can't complete a
    // combo later, and stop a hold-mode session that was running on them.
    for k in held_here {
        on_key_up(k, state, hotkey, ctx, false);
    }
}

/// Watches /dev/input and starts a listener for every mouse/keyboard, including
/// ones plugged in (or woken from suspend) after startup.
fn spawn_device_scanner(state: Arc<SharedState>, hotkey: Arc<Mutex<Hotkey>>, ctx: egui::Context) {
    thread::spawn(move || {
        let active: Arc<Mutex<HashSet<PathBuf>>> = Arc::default();
        while !state.shutdown.load(SeqCst) {
            if let Ok(entries) = std::fs::read_dir("/dev/input") {
                for entry in entries.flatten() {
                    let path = entry.path();
                    let is_event = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with("event"));
                    if !is_event || active.lock().contains(&path) {
                        continue;
                    }
                    let Ok(dev) = Device::open(&path) else {
                        continue;
                    };
                    let Some(kind) = classify(&dev) else { continue };

                    active.lock().insert(path.clone());
                    let state = Arc::clone(&state);
                    let hotkey = Arc::clone(&hotkey);
                    let ctx = ctx.clone();
                    let active = Arc::clone(&active);
                    thread::spawn(move || {
                        watch_device(dev, kind, &state, &hotkey, &ctx);
                        active.lock().remove(&path);
                    });
                }
            }
            let count = active.lock().len();
            let changed = state.device_count.swap(count, SeqCst) != count;
            let first = !state.scanned_once.swap(true, SeqCst);
            if changed || first {
                ctx.request_repaint();
            }
            thread::sleep(RESCAN_INTERVAL);
        }
    });
}

fn spawn_clicker(state: Arc<SharedState>, config: Arc<Mutex<Config>>, ctx: egui::Context) {
    thread::spawn(move || {
        let mut dev = match create_clicker_device() {
            Ok(d) => d,
            Err(e) => {
                let hint = match e.kind() {
                    io::ErrorKind::PermissionDenied => {
                        "add yourself to the 'input' or 'uinput' group"
                    }
                    io::ErrorKind::NotFound => {
                        "the uinput module may not be loaded — try: modprobe uinput"
                    }
                    _ => "check /dev/uinput permissions",
                };
                *state.click_error.lock() = format!("Cannot open /dev/uinput: {e} — {hint}");
                ctx.request_repaint();
                return;
            }
        };

        let mut rng = Rng::seeded();
        let mut active = false;
        let mut session_start = Instant::now();
        let mut next_click = Instant::now();

        loop {
            if state.shutdown.load(SeqCst) {
                break;
            }

            if !state.clicking.load(SeqCst) {
                if active {
                    active = false;
                    state.delayed_start.store(false, SeqCst);
                    ctx.request_repaint();
                }
                thread::sleep(IDLE_POLL);
                continue;
            }

            if !active {
                active = true;
                session_start = Instant::now();
                state.session_clicks.store(0, SeqCst);
                *state.last_moved.lock() = session_start;
                state.click_error.lock().clear();
                if state.delayed_start.swap(false, SeqCst)
                    && !wait_while_clicking(&state, session_start + DELAYED_START)
                {
                    continue;
                }
                next_click = Instant::now();
            }

            let cfg = config.lock().clone();

            if limit_reached(&cfg.limit, session_start, state.session_clicks.load(SeqCst)) {
                state.clicking.store(false, SeqCst);
                ctx.request_repaint();
                continue;
            }

            // Paused while the mouse is moving. The time limit keeps running, and the
            // first click after the pause happens straight away.
            if !is_still_enough(&cfg.still, &state.last_moved) {
                thread::sleep(IDLE_POLL);
                next_click = Instant::now();
                continue;
            }

            if !wait_while_clicking(&state, next_click) {
                continue;
            }

            let period = cfg.period_ms() * rng.jitter(cfg.jitter_fraction());
            let hold = cfg.hold_for(period);
            let keys = cfg.button.keys();

            // Press, hold, release. The release is always sent, even if stopping, so a
            // button can never be left stuck down.
            let pressed_at = Instant::now();
            let press = emit_buttons(&mut dev, keys, 1);
            if hold > 0.0 {
                precise_sleep_until(pressed_at + ms(hold));
            }
            let release = emit_buttons(&mut dev, keys, 0);

            match press.and(release) {
                Ok(()) => {
                    state.session_clicks.fetch_add(1, SeqCst);
                }
                Err(e) => {
                    *state.click_error.lock() = format!("Emit error: {e}");
                    state.clicking.store(false, SeqCst);
                    ctx.request_repaint();
                    continue;
                }
            }

            // Schedule against a fixed timeline so the rate doesn't drift slow. If we
            // fell a whole period behind (system stall), restart the timeline instead
            // of firing a burst of catch-up clicks.
            next_click += ms(period);
            let now = Instant::now();
            if now > next_click + ms(period) {
                next_click = now;
            }
        }
    });
}

// ── GUI ───────────────────────────────────────────────────────────────────────

struct App {
    state: Arc<SharedState>,
    config: Arc<Mutex<Config>>,
    hotkey: Arc<Mutex<Hotkey>>,

    ui_cfg: Config,
    hotkey_display: String,
    prev_capturing: bool,
    dirty_since: Option<Instant>,
    save_error: Option<String>,
}

impl App {
    fn new(
        state: Arc<SharedState>,
        config: Arc<Mutex<Config>>,
        hotkey: Arc<Mutex<Hotkey>>,
    ) -> Self {
        let ui_cfg = config.lock().clone();
        let hotkey_display = hotkey_label(&hotkey.lock().keys);
        Self {
            state,
            config,
            hotkey,
            ui_cfg,
            hotkey_display,
            prev_capturing: false,
            dirty_since: None,
            save_error: None,
        }
    }

    fn mark_dirty(&mut self) {
        self.dirty_since = Some(Instant::now());
    }

    fn save_now(&mut self) {
        let hotkey = self.hotkey.lock().clone();
        self.save_error = save_config(&self.ui_cfg, &hotkey)
            .err()
            .map(|e| format!("Couldn't save settings: {e}"));
        self.dirty_since = None;
    }
}

fn key_order(k: Key) -> (u8, u16) {
    let priority = match k {
        Key::KEY_LEFTMETA | Key::KEY_RIGHTMETA => 0,   // super
        Key::KEY_LEFTCTRL | Key::KEY_RIGHTCTRL => 1,   // ctrl
        Key::KEY_LEFTALT | Key::KEY_RIGHTALT => 2,     // alt
        Key::KEY_LEFTSHIFT | Key::KEY_RIGHTSHIFT => 3, // shift
        Key::KEY_FN => 4,                              // fn
        _ => 5,                                        // everything else
    };
    (priority, k.code())
}

fn key_name(k: Key) -> String {
    match k {
        Key::BTN_MIDDLE => "MouseMiddle".to_string(),
        Key::BTN_SIDE => "Mouse4".to_string(),
        Key::BTN_EXTRA => "Mouse5".to_string(),
        Key::BTN_FORWARD => "MouseForward".to_string(),
        Key::BTN_BACK => "MouseBack".to_string(),
        _ => {
            let s = format!("{k:?}");
            s.strip_prefix("KEY_").unwrap_or(&s).to_string()
        }
    }
}

fn hotkey_label(keys: &[Key]) -> String {
    if keys.is_empty() {
        return "None".to_string();
    }
    keys.iter()
        .map(|k| key_name(*k))
        .collect::<Vec<_>>()
        .join("+")
}

fn setting_field(ui: &mut egui::Ui, id: &str, setting: &mut Setting, units: &[Unit]) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        let h = ui.spacing().interact_size.y;
        let gap = ui.spacing().item_spacing.x;
        let unit_w = 72.0;
        let val_w = ui.available_width() - unit_w - gap;

        let range = setting.unit.range();
        let speed = (setting.value.abs() * 0.01).max(0.01);
        let r = ui.add_sized(
            [val_w, h],
            egui::DragValue::new(&mut setting.value)
                .clamp_range(range.clone())
                .speed(speed)
                .max_decimals(setting.unit.decimals()),
        );
        changed |= r.changed();
        if r.hovered() {
            let scroll = ui.input_mut(|i| {
                let dy = i.raw_scroll_delta.y;
                i.raw_scroll_delta.y = 0.0;
                dy
            });
            let step = if setting.value >= 2.0 || setting.unit.decimals() == 0 {
                1.0
            } else {
                0.1
            };
            if scroll > 0.0 {
                setting.value = (setting.value + step).min(*range.end());
                changed = true;
            }
            if scroll < 0.0 {
                setting.value = (setting.value - step).max(*range.start());
                changed = true;
            }
        }

        let mut unit = setting.unit;
        egui::ComboBox::from_id_source(id)
            .selected_text(unit.label())
            .width(unit_w - gap)
            .show_ui(ui, |ui| {
                for &u in units {
                    ui.selectable_value(&mut unit, u, u.label());
                }
            });
        if unit != setting.unit {
            setting.change_unit(unit);
            changed = true;
        }
    });
    changed
}

fn hint(ui: &mut egui::Ui, text: impl Into<String>) {
    ui.label(egui::RichText::new(text.into()).small().weak());
}

/// Width of each of `n` equal segments filling the current row.
#[expect(clippy::cast_precision_loss, reason = "only a handful of segments")]
fn segment_width(ui: &egui::Ui, n: usize) -> f32 {
    let n = n as f32;
    let gap = ui.spacing().item_spacing.x;
    (ui.available_width() - gap * (n - 1.0)) / n
}

fn section_break(ui: &mut egui::Ui) {
    ui.add_space(6.0);
    ui.separator();
    ui.add_space(6.0);
}

impl App {
    fn status_ui(&self, ui: &mut egui::Ui, clicking: bool) {
        ui.heading(APP_TITLE);
        ui.add_space(6.0);
        if clicking {
            let n = self.state.session_clicks.load(SeqCst);
            ui.colored_label(
                egui::Color32::from_rgb(100, 220, 100),
                format!("● Active · {n} clicks"),
            );
        } else {
            ui.colored_label(egui::Color32::from_rgb(160, 160, 160), "○ Stopped");
        }
    }

    /// Button, rate and hold time. Returns whether the config changed.
    fn click_settings_ui(&mut self, ui: &mut egui::Ui) -> bool {
        let h = ui.spacing().interact_size.y;
        let mut changed = false;

        ui.label("Button");
        ui.horizontal(|ui| {
            let w = segment_width(ui, ClickButton::ALL.len());
            for variant in ClickButton::ALL {
                let selected = self.ui_cfg.button == variant;
                if ui
                    .add_sized(
                        [w, h],
                        egui::SelectableLabel::new(selected, variant.label()),
                    )
                    .clicked()
                    && !selected
                {
                    self.ui_cfg.button = variant;
                    changed = true;
                }
            }
        });

        ui.add_space(6.0);

        ui.label("Click rate");
        changed |= setting_field(ui, "rate", &mut self.ui_cfg.rate, RATE_UNITS);
        let period = self.ui_cfg.period_ms();
        if self.ui_cfg.rate.unit == Unit::Cps {
            hint(ui, format!("one click every {period:.1} ms"));
        } else {
            hint(ui, format!("{:.2} clicks per second", 1000.0 / period));
        }

        ui.add_space(6.0);

        ui.label("Hold each click for");
        changed |= ui
            .add_sized(
                [ui.available_width(), h],
                egui::DragValue::new(&mut self.ui_cfg.hold_ms)
                    .clamp_range(0.0..=1000.0)
                    .speed(0.25)
                    .max_decimals(1)
                    .suffix(" ms"),
            )
            .changed();
        let effective = self.ui_cfg.hold_for(period);
        if self.ui_cfg.hold_ms <= 0.0 {
            hint(ui, "instant press and release — some games miss these");
        } else if effective < self.ui_cfg.hold_ms {
            hint(
                ui,
                format!("capped to {effective:.1} ms (half the interval)"),
            );
        }

        changed
    }

    /// Randomized timing, auto-stop and still threshold. Returns whether the config changed.
    fn extra_settings_ui(&mut self, ui: &mut egui::Ui) -> bool {
        let mut changed = false;

        changed |= ui
            .checkbox(&mut self.ui_cfg.jitter_enabled, "Randomize timing")
            .changed();
        if self.ui_cfg.jitter_enabled {
            changed |= ui
                .add(egui::Slider::new(&mut self.ui_cfg.jitter_pct, 1.0..=50.0).suffix("%"))
                .changed();
            hint(ui, "each interval varies by up to ± this much");
        }

        ui.add_space(6.0);

        changed |= ui
            .checkbox(&mut self.ui_cfg.limit.enabled, "Stop after")
            .changed();
        if self.ui_cfg.limit.enabled {
            changed |= setting_field(ui, "limit", &mut self.ui_cfg.limit, LIMIT_UNITS);
        }

        ui.add_space(6.0);

        changed |= ui
            .checkbox(
                &mut self.ui_cfg.still.enabled,
                "Only click when mouse is still for",
            )
            .changed();
        if self.ui_cfg.still.enabled {
            changed |= setting_field(ui, "still", &mut self.ui_cfg.still, TIME_UNITS);
        }

        changed
    }

    /// Hotkey capture and mode. Returns whether the hotkey settings changed.
    fn hotkey_ui(&mut self, ui: &mut egui::Ui, capturing: bool) -> bool {
        let h = ui.spacing().interact_size.y;
        let mut changed = false;

        ui.label("Hotkey (global)");
        if self.state.scanned_once.load(SeqCst) && self.state.device_count.load(SeqCst) == 0 {
            ui.colored_label(
                egui::Color32::from_rgb(220, 200, 80),
                "Can't read any input devices — are you in the 'input' group?",
            );
        }

        let label = if capturing {
            "Press key(s)… (Esc to cancel)".to_string()
        } else {
            format!("[{}]", self.hotkey_display)
        };
        if ui
            .add_sized([ui.available_width(), h], egui::Button::new(label))
            .clicked()
        {
            self.state.capturing.store(!capturing, SeqCst);
        }

        ui.horizontal(|ui| {
            let w = segment_width(ui, 2);
            let current = self.hotkey.lock().mode;
            for (mode, label) in [
                (HotkeyMode::Toggle, "Press to toggle"),
                (HotkeyMode::Hold, "Hold to click"),
            ] {
                if ui
                    .add_sized([w, h], egui::SelectableLabel::new(current == mode, label))
                    .clicked()
                    && current != mode
                {
                    self.hotkey.lock().mode = mode;
                    changed = true;
                }
            }
        });

        changed
    }

    /// Start/stop button and any error messages.
    fn controls_ui(&self, ui: &mut egui::Ui, clicking: bool) {
        let h = ui.spacing().interact_size.y;
        let toggle_label = if clicking { "Stop" } else { "Start" };
        if ui
            .add_sized(
                [ui.available_width(), h * 1.5],
                egui::Button::new(toggle_label),
            )
            .clicked()
        {
            if !clicking {
                self.state.delayed_start.store(true, SeqCst);
            }
            // A compare-exchange so a concurrent stop (time limit reached, hotkey)
            // isn't overwritten by a stale read.
            let _ = self
                .state
                .clicking
                .compare_exchange(clicking, !clicking, SeqCst, SeqCst);
        }

        let err = self.state.click_error.lock().clone();
        if !err.is_empty() {
            ui.add_space(4.0);
            ui.colored_label(egui::Color32::from_rgb(220, 80, 80), &err);
        }
        if let Some(err) = &self.save_error {
            ui.add_space(4.0);
            ui.colored_label(egui::Color32::from_rgb(220, 80, 80), err);
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let capturing = self.state.capturing.load(SeqCst);
        if self.prev_capturing && !capturing {
            self.hotkey_display = hotkey_label(&self.hotkey.lock().keys);
            self.mark_dirty();
        }
        self.prev_capturing = capturing;

        let clicking = self.state.clicking.load(SeqCst);
        let mut changed = false;
        let mut hotkey_changed = false;

        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false; 2])
                .show(ui, |ui| {
                    ui.vertical_centered(|ui| {
                        self.status_ui(ui, clicking);
                        section_break(ui);
                        changed |= self.click_settings_ui(ui);
                        ui.add_space(6.0);
                        changed |= self.extra_settings_ui(ui);
                        section_break(ui);
                        hotkey_changed = self.hotkey_ui(ui, capturing);
                        ui.add_space(8.0);
                        self.controls_ui(ui, clicking);
                    });
                });
        });

        if changed {
            *self.config.lock() = self.ui_cfg.clone();
        }
        if changed || hotkey_changed {
            self.mark_dirty();
        }
        if let Some(since) = self.dirty_since {
            if since.elapsed() >= SAVE_DELAY {
                self.save_now();
            } else {
                ctx.request_repaint_after(SAVE_DELAY);
            }
        }
        if clicking {
            // Keep the click counter ticking.
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.state.shutdown.store(true, SeqCst);
        if self.dirty_since.is_some() {
            self.save_now();
        }
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

fn main() -> eframe::Result<()> {
    let (config, hotkey) = load_config();
    let state = SharedState::new();
    let config = Arc::new(Mutex::new(config));
    let hotkey = Arc::new(Mutex::new(hotkey));

    let mut opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(APP_TITLE)
            .with_inner_size([300.0, 560.0])
            .with_min_inner_size([260.0, 300.0]),
        renderer: eframe::Renderer::Wgpu,
        ..Default::default()
    };
    // On X11-only systems libwayland-client.so is absent and winit errors instead of
    // falling back gracefully. Force X11 when no Wayland session is detected.
    if std::env::var_os("WAYLAND_DISPLAY").is_none_or(|v| v.is_empty()) {
        opts.event_loop_builder = Some(Box::new(|builder| {
            use winit::platform::x11::EventLoopBuilderExtX11;
            builder.with_x11();
        }));
    }

    eframe::run_native(
        APP_TITLE,
        opts,
        Box::new(|cc| {
            let ctx = cc.egui_ctx.clone();
            spawn_device_scanner(Arc::clone(&state), Arc::clone(&hotkey), ctx.clone());
            spawn_clicker(Arc::clone(&state), Arc::clone(&config), ctx);
            Box::new(App::new(state, config, hotkey))
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use evdev::Key;

    #[track_caller]
    fn assert_close(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 1e-9,
            "expected {expected}, got {actual}"
        );
    }

    // key_name relies on evdev's Debug format ("KEY_*"). Pin it here so any
    // upstream change that would silently break labels is caught immediately.
    #[test]
    fn evdev_key_debug_format_for_hotkey_label() {
        assert_eq!(format!("{:?}", Key::KEY_A), "KEY_A");
        assert_eq!(format!("{:?}", Key::KEY_F8), "KEY_F8");
        assert_eq!(format!("{:?}", Key::KEY_LEFTCTRL), "KEY_LEFTCTRL");
        assert_eq!(format!("{:?}", Key::BTN_LEFT), "BTN_LEFT"); // no KEY_ prefix — passes through as-is
    }

    #[test]
    fn hotkey_label_empty_returns_none() {
        assert_eq!(hotkey_label(&[]), "None");
    }

    #[test]
    fn hotkey_label_combo_joins_with_plus() {
        let combo = vec![Key::KEY_LEFTCTRL, Key::KEY_A];
        assert_eq!(hotkey_label(&combo), "LEFTCTRL+A");
    }

    #[test]
    fn hotkey_label_names_mouse_buttons() {
        assert_eq!(hotkey_label(&[Key::BTN_SIDE]), "Mouse4");
        assert_eq!(
            hotkey_label(&[Key::KEY_LEFTSHIFT, Key::BTN_EXTRA]),
            "LEFTSHIFT+Mouse5"
        );
    }

    #[test]
    fn hotkey_eligibility() {
        assert!(hotkey_eligible(Key::KEY_A));
        assert!(hotkey_eligible(Key::KEY_F8));
        assert!(hotkey_eligible(Key::BTN_SIDE));
        assert!(hotkey_eligible(Key::BTN_MIDDLE));
        assert!(!hotkey_eligible(Key::BTN_LEFT));
        assert!(!hotkey_eligible(Key::BTN_RIGHT));
        assert!(!hotkey_eligible(Key::BTN_TOUCH));
        assert!(!hotkey_eligible(Key::BTN_TOOL_FINGER));
    }

    #[test]
    fn unit_change_keeps_rate() {
        let mut s = Setting::new(true, 20.0, Unit::Cps);
        s.change_unit(Unit::Ms);
        assert_close(s.value, 50.0);
        s.change_unit(Unit::Sec);
        assert_close(s.value, 0.05);
        s.change_unit(Unit::Cps);
        assert_close(s.value, 20.0);
    }

    #[test]
    fn unit_change_to_clicks_keeps_value() {
        let mut s = Setting::new(true, 5.0, Unit::Min);
        s.change_unit(Unit::Clicks);
        assert_close(s.value, 5.0);
        assert_eq!(s.unit, Unit::Clicks);
    }

    #[test]
    fn hold_is_capped_to_half_the_period() {
        let c = Config {
            hold_ms: 30.0,
            ..Config::default()
        };
        assert_close(c.hold_for(100.0), 30.0);
        assert_close(c.hold_for(20.0), 10.0);
    }

    #[test]
    fn jitter_stays_in_bounds() {
        let mut rng = Rng::seeded();
        for _ in 0..10_000 {
            let j = rng.jitter(0.2);
            assert!((0.8..=1.2).contains(&j));
        }
        assert_close(rng.jitter(0.0), 1.0);
    }

    #[test]
    fn config_round_trips() {
        let c = Config {
            button: ClickButton::Middle,
            rate: Setting::new(true, 66.67, Unit::Ms),
            hold_ms: 12.5,
            jitter_enabled: true,
            jitter_pct: 15.0,
            limit: Setting::new(true, 500.0, Unit::Clicks),
            still: Setting::new(true, 0.25, Unit::Sec),
        };
        let h = Hotkey {
            keys: vec![Key::KEY_LEFTCTRL, Key::BTN_SIDE],
            mode: HotkeyMode::Hold,
        };
        let (c2, h2) = parse_config(&serialize_config(&c, &h));
        assert_eq!(c, c2);
        assert_eq!(h, h2);
    }

    #[test]
    fn bad_config_falls_back_to_defaults() {
        let (c, h) = parse_config("rate=banana\nbutton=nope\nhotkey=x,y\nrate_unit=clicks\n");
        assert_eq!(c, Config::default());
        assert_eq!(h, Hotkey::default());
    }
}
