use eframe::egui;
use evdev::{AttributeSet, Device, EventType, InputEvent, InputEventKind, Key, RelativeAxisType};
use evdev::uinput::VirtualDeviceBuilder;
use parking_lot::Mutex;
use std::{
    collections::HashSet,
    ops::RangeInclusive,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

const APP_TITLE: &str = "autoclicke-rs";
const SYN_REPORT: u16 = 0; // evdev SynchronizationCode::SYN_REPORT
const UI_WIDTH: f32 = 280.0;
const DELAYED_START_MS: u64 = 300;
const CLICKER_IDLE_SLEEP_MS: u64 = 10;
const CLICKER_POLL_SLEEP_MS: u64 = 5;
// Prevents multiple keyboard device threads from double-firing a single hotkey press.
const HOTKEY_DEBOUNCE_MS: u64 = 200;

const TIME_UNITS: &[Unit] = &[Unit::Ms, Unit::Sec, Unit::Min];
const ALL_UNITS:  &[Unit] = &[Unit::Ms, Unit::Sec, Unit::Min, Unit::Clicks];

// ── Shared state between threads ─────────────────────────────────────────────

struct SharedState {
    clicking: AtomicBool,
    shutdown: AtomicBool,
    capturing: AtomicBool,
    delayed_start: AtomicBool,
    last_moved: Mutex<Instant>,
    last_hotkey: Mutex<Option<Instant>>,
    // Shared across all keyboard device threads so combos spanning multiple devices work.
    held_keys: Mutex<HashSet<Key>>,
    click_error: Mutex<String>,
}

impl SharedState {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            clicking: AtomicBool::new(false),
            shutdown: AtomicBool::new(false),
            capturing: AtomicBool::new(false),
            delayed_start: AtomicBool::new(false),
            last_moved: Mutex::new(Instant::now()),
            last_hotkey: Mutex::new(None),
            held_keys: Mutex::new(HashSet::new()),
            click_error: Mutex::new(String::new()),
        })
    }
}

// ── Config types ──────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq)]
enum ClickButton {
    Left,
    Right,
    Both,
}

impl ClickButton {
    fn label(self) -> &'static str {
        match self {
            Self::Left  => "Left",
            Self::Right => "Right",
            Self::Both  => "Both",
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Unit {
    Ms,
    Sec,
    Min,
    Clicks,
}

impl Unit {
    fn label(self) -> &'static str {
        match self {
            Self::Ms     => "ms",
            Self::Sec    => "s",
            Self::Min    => "min",
            Self::Clicks => "clicks",
        }
    }
}

#[derive(Clone)]
struct Setting {
    enabled: bool,
    value: u64,
    unit: Unit,
    range: RangeInclusive<u64>,
}

impl Setting {
    fn new(enabled: bool, value: u64, unit: Unit, range: RangeInclusive<u64>) -> Self {
        Self { enabled, value, unit, range }
    }

    fn to_millis(&self) -> u64 {
        match self.unit {
            Unit::Ms     => self.value,
            Unit::Sec    => self.value * 1_000,
            Unit::Min    => self.value * 60_000,
            Unit::Clicks => 0,
        }
    }
}

// ── evdev helpers ─────────────────────────────────────────────────────────────

fn find_devices(filter: impl Fn(&Device) -> bool) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir("/dev/input") else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if let Ok(dev) = Device::open(&path) {
            if filter(&dev) {
                found.push(path);
            }
        }
    }
    found
}

fn find_mouse_devices() -> Vec<PathBuf> {
    find_devices(|dev| {
        dev.supported_events().contains(EventType::RELATIVE)
            && dev.supported_relative_axes()
                .map_or(false, |axes| axes.contains(RelativeAxisType::REL_X))
    })
}

fn find_keyboard_devices() -> Vec<PathBuf> {
    find_devices(|dev| {
        dev.supported_events().contains(EventType::KEY)
            && dev.supported_keys()
                .map_or(false, |keys| keys.contains(Key::KEY_A))
    })
}

// ── uinput helpers ───────────────────────────────────────────────────────────

fn create_clicker_device() -> std::io::Result<evdev::uinput::VirtualDevice> {
    let mut keys = AttributeSet::<Key>::new();
    keys.insert(Key::BTN_LEFT);
    keys.insert(Key::BTN_RIGHT);
    VirtualDeviceBuilder::new()?
        .name(APP_TITLE)
        .with_keys(&keys)?
        .build()
}

fn emit_click(device: &mut evdev::uinput::VirtualDevice, key: Key) -> std::io::Result<()> {
    let press   = InputEvent::new(EventType::KEY,             key.code(), 1);
    let syn     = InputEvent::new(EventType::SYNCHRONIZATION, SYN_REPORT, 0);
    let release = InputEvent::new(EventType::KEY,             key.code(), 0);
    device.emit(&[press, syn, release, syn])
}

// ── Clicker helpers ───────────────────────────────────────────────────────────

fn is_still_enough(threshold: &Setting, last_moved: &Mutex<Instant>) -> bool {
    if !threshold.enabled { return true; }
    let required = threshold.to_millis();
    required == 0 || last_moved.lock().elapsed() >= Duration::from_millis(required)
}

fn is_duration_over(setting: &Setting, session_start: Instant, session_clicks: u64) -> bool {
    if !setting.enabled { return false; }
    match setting.unit {
        Unit::Clicks => setting.value > 0 && session_clicks >= setting.value,
        _ => {
            let total = setting.to_millis();
            total > 0 && session_start.elapsed() >= Duration::from_millis(total)
        }
    }
}

// ── Background threads ────────────────────────────────────────────────────────

fn spawn_mouse_watchers(state: Arc<SharedState>, ctx: egui::Context) {
    for path in find_mouse_devices() {
        let state = Arc::clone(&state);
        let ctx   = ctx.clone();
        thread::spawn(move || {
            let Ok(mut dev) = Device::open(&path) else { return };
            loop {
                if state.shutdown.load(Ordering::Acquire) { break; }
                match dev.fetch_events() {
                    Ok(events) => {
                        for ev in events {
                            if matches!(ev.kind(), InputEventKind::RelAxis(_)) {
                                *state.last_moved.lock() = Instant::now();
                            }
                        }
                    }
                    Err(e) => {
                        *state.click_error.lock() = format!("Mouse device lost: {e}");
                        ctx.request_repaint();
                        break;
                    }
                }
            }
        });
    }
}

// Returns the number of keyboard device threads spawned.
fn spawn_hotkey_watcher(
    state: Arc<SharedState>,
    hotkey: Arc<Mutex<Vec<Key>>>,
    ctx: egui::Context,
) -> usize {
    let paths = find_keyboard_devices();
    let count = paths.len();
    for path in paths {
        let state  = Arc::clone(&state);
        let hotkey = Arc::clone(&hotkey);
        let ctx    = ctx.clone();
        thread::spawn(move || {
            let Ok(mut dev) = Device::open(&path) else { return };
            loop {
                if state.shutdown.load(Ordering::Acquire) { break; }
                match dev.fetch_events() {
                    Ok(events) => {
                        for ev in events {
                            let InputEventKind::Key(k) = ev.kind() else { continue };
                            match ev.value() {
                                1 => { // key down
                                    // held_keys is shared across all keyboard threads so combos
                                    // whose keys arrive on different devices are detected.
                                    // Lock order: held_keys → hotkey → last_hotkey.
                                    let mut held = state.held_keys.lock();
                                    held.insert(k);
                                    // Check `capturing` inside the hotkey lock to close the
                                    // TOCTOU window where the GUI sets capturing=true between
                                    // our load and the lock acquisition.
                                    let hk = hotkey.lock();
                                    if !state.capturing.load(Ordering::Relaxed)
                                        && !hk.is_empty()
                                        && held.len() == hk.len()
                                        && hk.iter().all(|k| held.contains(k))
                                    {
                                        // Debounce: if two device threads both insert the last
                                        // key of the combo within HOTKEY_DEBOUNCE_MS, only the
                                        // first one fires.
                                        let mut last = state.last_hotkey.lock();
                                        let elapsed_ok = last.map_or(true, |t| {
                                            t.elapsed().as_millis() as u64 >= HOTKEY_DEBOUNCE_MS
                                        });
                                        if elapsed_ok {
                                            *last = Some(Instant::now());
                                            drop(last);
                                            drop(hk);
                                            drop(held);
                                            state.clicking.fetch_xor(true, Ordering::Relaxed);
                                            ctx.request_repaint();
                                        }
                                    }
                                }
                                0 => { // key up — snapshot held (with k still in) for capture, then remove k
                                    let snapshot: Vec<Key> = {
                                        let mut held = state.held_keys.lock();
                                        let snap = held.iter().copied().collect();
                                        held.remove(&k);
                                        snap
                                    };
                                    if state.capturing.load(Ordering::Relaxed) && !snapshot.is_empty() {
                                        if state.capturing
                                            .compare_exchange(true, false, Ordering::AcqRel, Ordering::Relaxed)
                                            .is_ok()
                                        {
                                            let mut combo = snapshot;
                                            combo.sort_by_key(|k| key_order(*k));
                                            *hotkey.lock() = combo;
                                            ctx.request_repaint();
                                        }
                                    }
                                }
                                _ => {} // repeat, ignore
                            }
                        }
                    }
                    Err(e) => {
                        *state.click_error.lock() = format!("Keyboard device lost: {e}");
                        ctx.request_repaint();
                        break;
                    }
                }
            }
        });
    }
    count
}

fn spawn_clicker(
    state: Arc<SharedState>,
    interval: Arc<Mutex<Setting>>,
    click_duration: Arc<Mutex<Setting>>,
    still_threshold: Arc<Mutex<Setting>>,
    button: Arc<Mutex<ClickButton>>,
    ctx: egui::Context,
) {
    thread::spawn(move || {
        let mut dev = match create_clicker_device() {
            Ok(d) => d,
            Err(e) => {
                let hint = match e.kind() {
                    std::io::ErrorKind::PermissionDenied =>
                        "add yourself to the 'input' or 'uinput' group",
                    std::io::ErrorKind::NotFound =>
                        "the uinput module may not be loaded — try: modprobe uinput",
                    _ => "check /dev/uinput permissions",
                };
                *state.click_error.lock() = format!("Cannot open /dev/uinput: {e} — {hint}");
                ctx.request_repaint();
                return;
            }
        };

        let mut session_start: Option<Instant> = None;
        let mut clicking_since: Option<Instant> = None;
        let mut session_clicks: u64 = 0;

        loop {
            if state.shutdown.load(Ordering::Acquire) { break; }

            // Acquire pairs with the Release CAS in the GUI's Start/Stop button so that
            // `delayed_start = true` is visible whenever `clicking = true` is.
            if !state.clicking.load(Ordering::Acquire) {
                session_start = None;
                session_clicks = 0;
                if clicking_since.take().is_some() {
                    state.delayed_start.store(false, Ordering::Relaxed);
                }
                thread::sleep(Duration::from_millis(CLICKER_IDLE_SLEEP_MS));
                continue;
            }

            if clicking_since.is_none() {
                clicking_since = Some(Instant::now());
                *state.last_moved.lock() = Instant::now();
                state.click_error.lock().clear();
            }

            if state.delayed_start.load(Ordering::Relaxed) {
                if clicking_since.unwrap().elapsed() < Duration::from_millis(DELAYED_START_MS) {
                    thread::sleep(Duration::from_millis(CLICKER_POLL_SLEEP_MS));
                    continue;
                }
                state.delayed_start.store(false, Ordering::Relaxed);
            }

            if !is_still_enough(&still_threshold.lock(), &state.last_moved) {
                session_start = None;
                thread::sleep(Duration::from_millis(CLICKER_POLL_SLEEP_MS));
                continue;
            }

            let now = Instant::now();
            let session = session_start.get_or_insert(now);

            if is_duration_over(&click_duration.lock(), *session, session_clicks) {
                state.clicking.store(false, Ordering::Relaxed);
                session_start = None;
                ctx.request_repaint();
                continue;
            }

            let btn = *button.lock();
            let emit_err = match btn {
                ClickButton::Left  => emit_click(&mut dev, Key::BTN_LEFT).err(),
                ClickButton::Right => emit_click(&mut dev, Key::BTN_RIGHT).err(),
                ClickButton::Both  => {
                    let a = emit_click(&mut dev, Key::BTN_LEFT);
                    let b = emit_click(&mut dev, Key::BTN_RIGHT);
                    match (a, b) {
                        (Err(e1), Err(e2)) => Some(std::io::Error::other(format!("{e1}; {e2}"))),
                        (Err(e), _) | (_, Err(e)) => Some(e),
                        _ => None,
                    }
                }
            };

            match emit_err {
                Some(e) => {
                    *state.click_error.lock() = format!("Emit error: {e}");
                    state.clicking.store(false, Ordering::Relaxed);
                    ctx.request_repaint();
                    // Click was not delivered; don't advance session_clicks.
                }
                None => {
                    session_clicks += 1;
                }
            }

            let sleep_ms = interval.lock().to_millis().max(1);
            thread::sleep(Duration::from_millis(sleep_ms));
        }
    });
}

// ── GUI ───────────────────────────────────────────────────────────────────────

struct App {
    state: Arc<SharedState>,

    interval:        Arc<Mutex<Setting>>,
    click_duration:  Arc<Mutex<Setting>>,
    still_threshold: Arc<Mutex<Setting>>,
    button: Arc<Mutex<ClickButton>>,
    hotkey: Arc<Mutex<Vec<Key>>>,

    ui_interval:       Setting,
    ui_click_duration: Setting,
    ui_still:          Setting,
    ui_button:         ClickButton,
    hotkey_display:    String,
    prev_capturing:    bool,
    keyboard_count:    usize,
}

impl App {
    fn new(
        state: Arc<SharedState>,
        interval: Arc<Mutex<Setting>>,
        click_duration: Arc<Mutex<Setting>>,
        still_threshold: Arc<Mutex<Setting>>,
        button: Arc<Mutex<ClickButton>>,
        hotkey: Arc<Mutex<Vec<Key>>>,
        keyboard_count: usize,
    ) -> Self {
        let ui_interval       = interval.lock().clone();
        let ui_click_duration = click_duration.lock().clone();
        let ui_still          = still_threshold.lock().clone();
        let ui_button         = *button.lock();
        let hotkey_display    = hotkey_label(&hotkey.lock());
        Self {
            state,
            interval,
            click_duration,
            still_threshold,
            button,
            hotkey,
            ui_interval,
            ui_click_duration,
            ui_still,
            ui_button,
            hotkey_display,
            prev_capturing: false,
            keyboard_count,
        }
    }
}

fn key_order(k: Key) -> (u8, u16) {
    let priority = match k {
        Key::KEY_LEFTMETA  | Key::KEY_RIGHTMETA  => 0, // super
        Key::KEY_LEFTCTRL  | Key::KEY_RIGHTCTRL  => 1, // ctrl
        Key::KEY_LEFTALT   | Key::KEY_RIGHTALT   => 2, // alt
        Key::KEY_LEFTSHIFT | Key::KEY_RIGHTSHIFT => 3, // shift
        Key::KEY_FN                              => 4, // fn
        _                                        => 5, // everything else
    };
    (priority, k.code())
}

fn hotkey_label(keys: &[Key]) -> String {
    if keys.is_empty() { return "None".to_string(); }
    let mut out = String::new();
    for (i, k) in keys.iter().enumerate() {
        if i > 0 { out.push('+'); }
        let s = format!("{k:?}");
        out.push_str(s.strip_prefix("KEY_").unwrap_or(&s));
    }
    out
}

fn setting_field(ui: &mut egui::Ui, id: &str, setting: &mut Setting, units: &[Unit]) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        let h      = ui.spacing().interact_size.y;
        let gap    = ui.spacing().item_spacing.x;
        let unit_w = 58.0;
        let val_w  = ui.available_width() - unit_w - gap;

        let r = ui.add_sized(
            [val_w, h],
            egui::DragValue::new(&mut setting.value)
                .clamp_range(setting.range.clone()),
        );
        changed |= r.changed();
        if r.hovered() {
            let scroll = ui.input_mut(|i| {
                let dy = i.raw_scroll_delta.y;
                i.raw_scroll_delta.y = 0.0;
                dy
            });
            let (&min, &max) = (setting.range.start(), setting.range.end());
            if scroll > 0.0 { setting.value = setting.value.saturating_add(1).min(max); changed = true; }
            if scroll < 0.0 { setting.value = setting.value.saturating_sub(1).max(min); changed = true; }
        }

        egui::ComboBox::from_id_source(id)
            .selected_text(setting.unit.label())
            .width(unit_w - gap)
            .show_ui(ui, |ui| {
                for &unit in units {
                    if ui.selectable_value(&mut setting.unit, unit, unit.label()).changed() {
                        changed = true;
                    }
                }
            });
    });
    changed
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Scale the UI to fill the window width. Height handled by ScrollArea.
        {
            let ppp    = ctx.pixels_per_point();
            let phys_w = ctx.input(|i| i.screen_rect().width()) * ppp;
            let new_ppp = (phys_w / UI_WIDTH).clamp(0.5, 4.0);
            if (new_ppp - ppp).abs() > 0.001 {
                ctx.set_pixels_per_point(new_ppp);
            }
        }

        let capturing = self.state.capturing.load(Ordering::Relaxed);
        if self.prev_capturing && !capturing {
            self.hotkey_display = hotkey_label(&self.hotkey.lock());
        }
        self.prev_capturing = capturing;

        let clicking = self.state.clicking.load(Ordering::Relaxed);

        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false; 2])
                .show(ui, |ui| {
                    ui.vertical_centered(|ui| {
                        let h = ui.spacing().interact_size.y;

                        ui.heading(APP_TITLE);
                        ui.add_space(6.0);

                        let (status_text, status_color) = if clicking {
                            ("● Active", egui::Color32::from_rgb(100, 220, 100))
                        } else {
                            ("○ Stopped", egui::Color32::from_rgb(160, 160, 160))
                        };
                        ui.colored_label(status_color, status_text);

                        ui.add_space(6.0);
                        ui.separator();
                        ui.add_space(6.0);

                        // Button selector — three equal-width segments.
                        ui.label("Button");
                        ui.horizontal(|ui| {
                            let gap = ui.spacing().item_spacing.x;
                            let w = (ui.available_width() - gap * 2.0) / 3.0;
                            for variant in [ClickButton::Left, ClickButton::Right, ClickButton::Both] {
                                let selected = self.ui_button == variant;
                                if ui
                                    .add_sized([w, h], egui::SelectableLabel::new(selected, variant.label()))
                                    .clicked()
                                {
                                    self.ui_button = variant;
                                    *self.button.lock() = variant;
                                }
                            }
                        });

                        ui.add_space(6.0);

                        // Click interval.
                        ui.label("Click interval");
                        if setting_field(ui, "interval", &mut self.ui_interval, TIME_UNITS) {
                            *self.interval.lock() = self.ui_interval.clone();
                        }

                        ui.add_space(6.0);

                        // Clicking duration (time or clicks).
                        if ui.checkbox(&mut self.ui_click_duration.enabled, "Clicking duration").changed() {
                            *self.click_duration.lock() = self.ui_click_duration.clone();
                        }
                        if self.ui_click_duration.enabled {
                            if setting_field(ui, "click_duration", &mut self.ui_click_duration, ALL_UNITS) {
                                *self.click_duration.lock() = self.ui_click_duration.clone();
                            }
                        }

                        ui.add_space(6.0);

                        // Still threshold.
                        if ui.checkbox(&mut self.ui_still.enabled, "Still threshold").changed() {
                            *self.still_threshold.lock() = self.ui_still.clone();
                        }
                        if self.ui_still.enabled {
                            if setting_field(ui, "still_threshold", &mut self.ui_still, TIME_UNITS) {
                                *self.still_threshold.lock() = self.ui_still.clone();
                            }
                        }

                        ui.add_space(6.0);
                        ui.separator();
                        ui.add_space(6.0);

                        // Hotkey capture — full-width button.
                        ui.label("Toggle hotkey (global)");
                        if self.keyboard_count == 0 {
                            ui.colored_label(
                                egui::Color32::from_rgb(220, 200, 80),
                                "No keyboard devices found",
                            );
                        }
                        if capturing {
                            if ui
                                .add_sized([ui.available_width(), h], egui::Button::new("Press any key(s)…"))
                                .clicked()
                            {
                                self.state.capturing.store(false, Ordering::Relaxed);
                            }
                        } else {
                            let label = format!("[{}]", self.hotkey_display);
                            if ui
                                .add_sized([ui.available_width(), h], egui::Button::new(label))
                                .clicked()
                            {
                                self.state.capturing.store(true, Ordering::Relaxed);
                            }
                        }

                        ui.add_space(8.0);

                        // Manual start/stop — full-width button.
                        let toggle_label = if clicking { "Stop" } else { "Start" };
                        if ui
                            .add_sized([ui.available_width(), h * 1.5], egui::Button::new(toggle_label))
                            .clicked()
                        {
                            if !clicking {
                                self.state.delayed_start.store(true, Ordering::Relaxed);
                            }
                            // CAS avoids overwriting a concurrent state change from the clicker
                            // thread (e.g. duration-stop racing with manual stop). Release
                            // ordering ensures `delayed_start = true` is visible to the clicker's
                            // Acquire load of `clicking`.
                            let _ = self.state.clicking.compare_exchange(
                                clicking, !clicking, Ordering::Release, Ordering::Relaxed,
                            );
                        }

                        let err = self.state.click_error.lock().clone();
                        if !err.is_empty() {
                            ui.add_space(4.0);
                            ui.colored_label(egui::Color32::from_rgb(220, 80, 80), &err);
                        }
                    });
                });
        });
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.state.shutdown.store(true, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use evdev::Key;

    // hotkey_label relies on evdev's Debug format ("KEY_*"). Pin it here so any
    // upstream change that would silently break labels is caught immediately.
    #[test]
    fn evdev_key_debug_format_for_hotkey_label() {
        assert_eq!(format!("{:?}", Key::KEY_A),        "KEY_A");
        assert_eq!(format!("{:?}", Key::KEY_F8),       "KEY_F8");
        assert_eq!(format!("{:?}", Key::KEY_LEFTCTRL), "KEY_LEFTCTRL");
        assert_eq!(format!("{:?}", Key::BTN_LEFT),     "BTN_LEFT"); // no KEY_ prefix — passes through as-is
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
}

// ── Entry point ───────────────────────────────────────────────────────────────

fn main() -> eframe::Result<()> {
    let state = SharedState::new();
    let interval        = Arc::new(Mutex::new(Setting::new(true,  100, Unit::Ms,  1..=999_999)));
    let click_duration  = Arc::new(Mutex::new(Setting::new(false,   1, Unit::Min, 1..=999_999)));
    let still_threshold = Arc::new(Mutex::new(Setting::new(false,   1, Unit::Sec, 1..=999_999)));
    let button = Arc::new(Mutex::new(ClickButton::Left));
    let hotkey: Arc<Mutex<Vec<Key>>> = Arc::new(Mutex::new(vec![Key::KEY_F8]));

    eframe::run_native(
        APP_TITLE,
        eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_title(APP_TITLE)
                .with_inner_size([UI_WIDTH, 300.0]),
            renderer: eframe::Renderer::Wgpu,
            ..Default::default()
        },
        Box::new(|cc| {
            let ctx = cc.egui_ctx.clone();
            spawn_mouse_watchers(Arc::clone(&state), ctx.clone());
            let keyboard_count = spawn_hotkey_watcher(
                Arc::clone(&state), Arc::clone(&hotkey), ctx.clone(),
            );
            spawn_clicker(
                Arc::clone(&state),
                Arc::clone(&interval),
                Arc::clone(&click_duration),
                Arc::clone(&still_threshold),
                Arc::clone(&button),
                ctx,
            );
            Box::new(App::new(
                state, interval, click_duration, still_threshold, button, hotkey,
                keyboard_count,
            ))
        }),
    )
}
