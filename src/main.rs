use anyhow::{anyhow, Result};
use cairo::{Antialias, Context, Format, ImageSurface, Surface};
use chrono::{Local, Locale, Timelike, format::{StrftimeItems, Item as ChronoItem}};
use drm::control::ClipRect;
use freedesktop_icons::lookup;
use input::{
    event::{
        device::DeviceEvent,
        keyboard::{KeyState, KeyboardEvent, KeyboardEventTrait},
        touch::{TouchEvent, TouchEventPosition, TouchEventSlot},
        Event, EventTrait,
    },
    Device as InputDevice, Libinput, LibinputInterface,
};
use input_linux::{uinput::UInputHandle, EventKind, Key, SynchronizeKind};
use input_linux_sys::{input_event, input_id, timeval, uinput_setup};
use libc::{c_char, O_ACCMODE, O_RDONLY, O_RDWR, O_WRONLY};
use librsvg_rebind::{prelude::HandleExt, Handle, Rectangle};
use nix::{
    errno::Errno,
    sys::{
        epoll::{Epoll, EpollCreateFlags, EpollEvent, EpollFlags},
        signal::{SigSet, Signal},
    },
    unistd::geteuid,
};
use privdrop::PrivDrop;
use std::{
    cmp::min,
    collections::HashMap,
    fs::{self, File, OpenOptions},
    os::{
        fd::{AsFd, AsRawFd},
        unix::{fs::OpenOptionsExt, io::OwnedFd},
    },
    panic::{self, AssertUnwindSafe},
    path::{Path, PathBuf},
    time::Instant,
};
use udev::MonitorBuilder;

mod backlight;
mod config;
mod display;
mod fonts;
mod hyprland;
mod pixel_shift;
mod pomodoro;
mod sysinfo;
mod visualizer;

use crate::config::ConfigManager;
use backlight::BacklightManager;
use config::{ButtonConfig, Config};
use display::DrmBackend;
use hyprland::{HyprlandClient, ScreenshotCache, WorkspaceInfo};
use pixel_shift::{PixelShiftManager, PIXEL_SHIFT_WIDTH_PX};
use pomodoro::{PomodoroTimer, PomodoroState};
use sysinfo::SystemStats;
use visualizer::AudioVisualizer;

const BUTTON_SPACING_PX: i32 = 16;
// Color constants are now configurable through the config system
const ICON_SIZE: i32 = 48;
const TIMEOUT_MS: i32 = 10 * 1000;

/// Shared state for widget buttons (Pomodoro, SystemStats, Visualizer)
struct WidgetState {
    pomodoro: PomodoroTimer,
    sysinfo: SystemStats,
    visualizer: AudioVisualizer,
    last_sysinfo_update: Instant,
}

impl WidgetState {
    fn new() -> Self {
        Self {
            pomodoro: PomodoroTimer::new(),
            sysinfo: SystemStats::new(60),
            visualizer: AudioVisualizer::new(16, 0.85),
            last_sysinfo_update: Instant::now(),
        }
    }

    fn update(&mut self) -> bool {
        let mut needs_redraw = false;

        // Update pomodoro timer
        if self.pomodoro.tick() {
            needs_redraw = true;
        }

        // Update sysinfo every second
        if self.last_sysinfo_update.elapsed().as_millis() >= 1000 {
            self.sysinfo.sample();
            self.last_sysinfo_update = Instant::now();
            needs_redraw = true;
        }

        // Update visualizer if running
        if self.visualizer.is_running() {
            self.visualizer.update();
            needs_redraw = true;
        }

        needs_redraw
    }
}

/// Convert HSV to RGB (h: 0-1, s: 0-1, v: 0-1)
fn hsv_to_rgb(h: f64, s: f64, v: f64) -> (f64, f64, f64) {
    let c = v * s;
    let h_prime = h * 6.0;
    let x = c * (1.0 - ((h_prime % 2.0) - 1.0).abs());
    let m = v - c;
    let (r, g, b) = match h_prime as i32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    (r + m, g + m, b + m)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BatteryState {
    NotCharging,
    Charging,
    Low,
}

struct BatteryImages {
    plain: Vec<Handle>,
    charging: Vec<Handle>,
    bolt: Handle,
}

#[derive(Eq, PartialEq, Copy, Clone)]
enum BatteryIconMode {
    Percentage,
    Icon,
    Both
}

impl BatteryIconMode {
    fn should_draw_icon(self) -> bool {
        self != BatteryIconMode::Percentage
    }
    fn should_draw_text(self) -> bool {
        self != BatteryIconMode::Icon
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SliderType {
    Volume,
    Brightness,
    KeyboardBacklight,
}

struct SliderConfig {
    slider_type: SliderType,
    icon: Handle,
    get_command: Option<String>,
    set_command: Option<String>,
}

enum ButtonImage {
    Text(String),
    Svg(Handle),
    Bitmap(ImageSurface),
    Time(Vec<ChronoItem<'static>>, Locale),
    Battery(String, BatteryIconMode, BatteryImages),
    Slider(SliderConfig),
    SliderText(SliderType, String),
    HyprlandWorkspaces(Option<Handle>),
    Pomodoro,
    SystemStats,
    Visualizer(usize), // bar_count
}

struct Button {
    image: ButtonImage,
    changed: bool,
    active: bool,
    action: Key,
}

fn try_load_svg(path: &str) -> Result<ButtonImage> {
    Ok(ButtonImage::Svg(
        Handle::from_file(path)?.ok_or(anyhow!("failed to load image"))?,
    ))
}

fn try_load_png(path: impl AsRef<Path>) -> Result<ButtonImage> {
    let mut file = File::open(path)?;
    let surf = ImageSurface::create_from_png(&mut file)?;
    if surf.height() == ICON_SIZE && surf.width() == ICON_SIZE {
        return Ok(ButtonImage::Bitmap(surf));
    }
    let resized = ImageSurface::create(Format::ARgb32, ICON_SIZE, ICON_SIZE).unwrap();
    let c = Context::new(&resized).unwrap();
    c.scale(
        ICON_SIZE as f64 / surf.width() as f64,
        ICON_SIZE as f64 / surf.height() as f64,
    );
    c.set_source_surface(surf, 0.0, 0.0).unwrap();
    c.set_antialias(Antialias::Best);
    c.paint().unwrap();
    Ok(ButtonImage::Bitmap(resized))
}

fn try_load_image(name: impl AsRef<str>, theme: Option<impl AsRef<str>>) -> Result<ButtonImage> {
    let name = name.as_ref();
    let locations;

    // Load list of candidate locations
    if let Some(theme) = theme {
        // Freedesktop icons
        let theme = theme.as_ref();
        let candidates = vec![
            lookup(name)
                .with_cache()
                .with_theme(theme)
                .with_size(ICON_SIZE as u16)
                .force_svg()
                .find(),
            lookup(name)
                .with_cache()
                .with_theme(theme)
                .force_svg()
                .find(),
        ];

        // .flatten() removes `None` and unwraps `Some` values
        locations = candidates.into_iter().flatten().collect();
    } else {
        // Standard file icons
        locations = vec![
            PathBuf::from(format!("/etc/tiny-dfr/{name}.svg")),
            PathBuf::from(format!("/etc/tiny-dfr/{name}.png")),
            PathBuf::from(format!("/usr/share/tiny-dfr/{name}.svg")),
            PathBuf::from(format!("/usr/share/tiny-dfr/{name}.png")),
        ];
    };

    // Try to load each candidate
    let mut last_err = anyhow!("no suitable icon path was found"); // in case locations is empty

    for location in locations {
        let result = match location.extension().and_then(|s| s.to_str()) {
            Some("png") => try_load_png(&location),
            Some("svg") => try_load_svg(
                location
                    .to_str()
                    .ok_or(anyhow!("image path is not unicode"))?,
            ),
            _ => Err(anyhow!("invalid file extension")),
        };

        match result {
            Ok(image) => return Ok(image),
            Err(err) => {
                last_err = err.context(format!("while loading path {}", location.display()));
            }
        };
    }

    // if function hasn't returned by now, all sources have been exhausted
    Err(last_err.context(format!("failed loading all possible paths for icon {name}")))
}

fn find_battery_device() -> Option<String> {
    let power_supply_path = "/sys/class/power_supply";
    if let Ok(entries) = fs::read_dir(power_supply_path) {
        for entry in entries.flatten() {
            let dev_path = entry.path();
            let type_path = dev_path.join("type");
            if let Ok(typ) = fs::read_to_string(&type_path) {
                if typ.trim() == "Battery" {
                    if let Some(name) = dev_path.file_name().and_then(|n| n.to_str()) {
                        return Some(name.to_string());
                    }
                }
            }
        }
    }
    None
}

fn get_battery_state(battery: &str) -> (u32, BatteryState) {
    let status_path = format!("/sys/class/power_supply/{}/status", battery);
    let status = fs::read_to_string(&status_path)
        .unwrap_or_else(|_| "Unknown".to_string());

    #[cfg(target_arch = "x86_64")]
    let capacity = {
        let charge_now_path = format!("/sys/class/power_supply/{}/charge_now", battery);
        let charge_full_path = format!("/sys/class/power_supply/{}/charge_full", battery);
        let charge_now = fs::read_to_string(&charge_now_path)
            .ok()
            .and_then(|s| s.trim().parse::<f64>().ok());
        let charge_full = fs::read_to_string(&charge_full_path)
            .ok()
            .and_then(|s| s.trim().parse::<f64>().ok());
        match (charge_now, charge_full) {
            (Some(now), Some(full)) if full > 0.0 => ((now / full) * 100.0).round() as u32,
            _ => 100,
        }
    };

    #[cfg(target_arch = "aarch64")]
    let capacity = {
        let capacity_path = format!("/sys/class/power_supply/{}/capacity", battery);
        fs::read_to_string(&capacity_path)
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .unwrap_or(100)
    };

    let status = match status.trim() {
        "Charging" | "Full" => BatteryState::Charging,
        "Discharging" if capacity < 10 => BatteryState::Low,
        _ => BatteryState::NotCharging,
    };
    (capacity, status)
}

impl Button {
    fn with_config(cfg: ButtonConfig) -> Button {
        if cfg.hyprland_workspaces == Some(true) {
            Button::new_hyprland_workspaces(cfg.action, cfg.icon, cfg.theme)
        } else if cfg.pomodoro == Some(true) {
            Button::new_pomodoro(cfg.action)
        } else if cfg.sysinfo == Some(true) {
            Button::new_sysinfo(cfg.action)
        } else if let Some(bar_count) = cfg.visualizer {
            Button::new_visualizer(cfg.action, bar_count)
        } else if let Some(slider_type) = cfg.slider {
            Button::new_slider(
                cfg.action,
                &slider_type,
                cfg.text,
                cfg.icon,
                cfg.theme,
                cfg.slider_get_command,
                cfg.slider_set_command,
            )
        } else if let Some(text) = cfg.text {
            Button::new_text(text, cfg.action)
        } else if let Some(icon) = cfg.icon {
            Button::new_icon(&icon, cfg.theme, cfg.action)
        } else if let Some(time) = cfg.time {
            Button::new_time(cfg.action, &time, cfg.locale.as_deref())
        } else if let Some(battery_mode) = cfg.battery {
            if let Some(battery) = find_battery_device() {
                Button::new_battery(cfg.action, battery, battery_mode, cfg.theme)
            } else {
                Button::new_text("Battery N/A".to_string(), cfg.action)
            }
        } else {
            panic!("Invalid config, a button must have either Text, Icon, Time, Battery, Slider, Pomodoro, Sysinfo, Visualizer, or HyprlandWorkspaces")
        }
    }
    fn new_text(text: String, action: Key) -> Button {
        Button {
            action,
            active: false,
            changed: false,
            image: ButtonImage::Text(text),
        }
    }
    fn new_icon(path: impl AsRef<str>, theme: Option<impl AsRef<str>>, action: Key) -> Button {
        let image = try_load_image(path, theme).expect("failed to load icon");
        Button {
            action,
            image,
            active: false,
            changed: false,
        }
    }
    fn load_battery_image(icon: &str, theme: Option<impl AsRef<str>>) -> Handle {
        if let ButtonImage::Svg(svg) = try_load_image(icon, theme).unwrap() {
            return svg;
        }
        panic!("failed to load icon");
    }
    fn new_battery(action: Key, battery: String, battery_mode: String, theme: Option<impl AsRef<str>>) -> Button {
        let bolt = Self::load_battery_image("bolt", theme.as_ref());
        let mut plain = Vec::new();
        let mut charging = Vec::new();
        for icon in [
            "battery_0_bar", "battery_1_bar", "battery_2_bar", "battery_3_bar",
            "battery_4_bar", "battery_5_bar", "battery_6_bar", "battery_full",
        ] {
            plain.push(Self::load_battery_image(icon, theme.as_ref()));
        }
        for icon in [
            "battery_charging_20", "battery_charging_30", "battery_charging_50",
            "battery_charging_60", "battery_charging_80",
            "battery_charging_90", "battery_charging_full",
        ] {
            charging.push(Self::load_battery_image(icon, theme.as_ref()));
        }
        let battery_mode = match battery_mode.as_str() {
            "icon" => BatteryIconMode::Icon,
            "percentage" => BatteryIconMode::Percentage,
            "both" => BatteryIconMode::Both,
            _ => panic!("invalid battery mode, accepted modes: icon, percentage, both"),
        };
        Button {
            action,
            active: false,
            changed: false,
            image: ButtonImage::Battery(battery, battery_mode, BatteryImages {
                plain, bolt, charging
            }),
        }
    }

    fn new_time(action: Key, format: &str, locale_str: Option<&str>) -> Button {
        let format_str = if format == "24hr" {
            "%H:%M    %a %-e %b"
        } else if format == "12hr" {
            "%-l:%M %p    %a %-e %b"
        } else {
            format
        };

        let format_items = match StrftimeItems::new(format_str).parse_to_owned() {
            Ok(s) => s,
            Err(e) => panic!("Invalid time format, consult the configuration file for examples of correct ones: {e:?}"),
        };

        let locale = locale_str.and_then(|l| Locale::try_from(l).ok()).unwrap_or(Locale::POSIX);
        Button {
            action,
            active: false,
            changed: false,
            image: ButtonImage::Time(format_items, locale),
        }
    }

    fn new_slider(
        action: Key,
        slider_type: &str,
        text: Option<String>,
        icon: Option<String>,
        theme: Option<impl AsRef<str>>,
        get_command: Option<String>,
        set_command: Option<String>,
    ) -> Button {
        let slider_type_enum = match slider_type {
            "Volume" => SliderType::Volume,
            "Brightness" => SliderType::Brightness,
            "KeyboardBacklight" => SliderType::KeyboardBacklight,
            _ => panic!("Invalid slider type: {}, expected Volume, Brightness, or KeyboardBacklight", slider_type),
        };

        // Prefer icon over text if both provided
        if let Some(icon_name) = icon {
            let image = try_load_image(icon_name, theme).expect("failed to load slider icon");
            let icon_handle = match image {
                ButtonImage::Svg(h) => h,
                _ => panic!("Slider icons must be SVG"),
            };

            return Button {
                action,
                active: false,
                changed: false,
                image: ButtonImage::Slider(SliderConfig {
                    slider_type: slider_type_enum,
                    icon: icon_handle,
                    get_command,
                    set_command,
                }),
            };
        }

        // If text is provided, create a text-based slider
        if let Some(text) = text {
            return Button {
                action,
                active: false,
                changed: false,
                image: ButtonImage::SliderText(slider_type_enum, text),
            };
        }

        // Otherwise use default icon for the slider type
        let default_icon = match slider_type {
            "Volume" => "volume_up",
            "Brightness" => "brightness_high",
            "KeyboardBacklight" => "backlight_high",
            _ => unreachable!(),
        };

        let image = try_load_image(default_icon, theme).expect("failed to load slider icon");
        let icon_handle = match image {
            ButtonImage::Svg(h) => h,
            _ => panic!("Slider icons must be SVG"),
        };

        Button {
            action,
            active: false,
            changed: false,
            image: ButtonImage::Slider(SliderConfig {
                slider_type: slider_type_enum,
                icon: icon_handle,
                get_command,
                set_command,
            }),
        }
    }

    fn new_hyprland_workspaces(action: Key, icon: Option<String>, theme: Option<impl AsRef<str>>) -> Button {
        // Use provided icon or fall back to a default
        let icon_name = icon.unwrap_or_else(|| "view-grid".to_string());
        let icon_handle = match try_load_image(&icon_name, theme) {
            Ok(ButtonImage::Svg(h)) => Some(h),
            _ => None,
        };

        Button {
            action,
            active: false,
            changed: false,
            image: ButtonImage::HyprlandWorkspaces(icon_handle),
        }
    }

    fn new_pomodoro(action: Key) -> Button {
        Button {
            action,
            active: false,
            changed: false,
            image: ButtonImage::Pomodoro,
        }
    }

    fn new_sysinfo(action: Key) -> Button {
        Button {
            action,
            active: false,
            changed: false,
            image: ButtonImage::SystemStats,
        }
    }

    fn new_visualizer(action: Key, bar_count: usize) -> Button {
        Button {
            action,
            active: false,
            changed: false,
            image: ButtonImage::Visualizer(bar_count.max(4).min(32)),
        }
    }

    fn render(
        &self,
        c: &Context,
        height: i32,
        button_left_edge: f64,
        button_width: u64,
        y_shift: f64,
        config: &crate::config::Config,
        widgets: &WidgetState,
    ) {
        match &self.image {
            ButtonImage::Text(text) => {
                self.set_text_color(c, config);
                let extents = c.text_extents(text).unwrap();
                c.move_to(
                    button_left_edge + (button_width as f64 / 2.0 - extents.width() / 2.0).round(),
                    y_shift + (height as f64 / 2.0 + extents.height() / 2.0).round(),
                );
                c.show_text(text).unwrap();
            }
            ButtonImage::Svg(svg) => {
                let x =
                    button_left_edge + (button_width as f64 / 2.0 - (ICON_SIZE / 2) as f64).round();
                let y = y_shift + ((height as f64 - ICON_SIZE as f64) / 2.0).round();

                self.render_svg_with_color(c, svg, x, y, config, self.active);
            }
            ButtonImage::Bitmap(surf) => {
                let x =
                    button_left_edge + (button_width as f64 / 2.0 - (ICON_SIZE / 2) as f64).round();
                let y = y_shift + ((height as f64 - ICON_SIZE as f64) / 2.0).round();
                c.set_source_surface(surf, x, y).unwrap();
                c.rectangle(x, y, ICON_SIZE as f64, ICON_SIZE as f64);
                c.fill().unwrap();
            }
            ButtonImage::Time(format, locale) => {
                self.set_text_color(c, config);
                let current_time = Local::now();
                let formatted_time = current_time.format_localized_with_items(format.iter(), *locale).to_string();
                let time_extents = c.text_extents(&formatted_time).unwrap();
                c.move_to(
                    button_left_edge + (button_width as f64 / 2.0 - time_extents.width() / 2.0).round(),
                    y_shift + (height as f64 / 2.0 + time_extents.height() / 2.0).round()
                );
                c.show_text(&formatted_time).unwrap();
            }
            ButtonImage::Battery(battery, battery_mode, icons) => {
                let (capacity, state) = get_battery_state(battery);
                let icon = if battery_mode.should_draw_icon() {
                    Some(match state {
                        BatteryState::Charging => match capacity {
                            0..=20 => &icons.charging[0],
                            21..=30 => &icons.charging[1],
                            31..=50 => &icons.charging[2],
                            51..=60 => &icons.charging[3],
                            61..=80 => &icons.charging[4],
                            81..=99 => &icons.charging[5],
                            _ => &icons.charging[6],
                        },
                        _ => match capacity {
                            0 => &icons.plain[0],
                            1..=20 => &icons.plain[1],
                            21..=30 => &icons.plain[2],
                            31..=50 => &icons.plain[3],
                            51..=60 => &icons.plain[4],
                            61..=80 => &icons.plain[5],
                            81..=99 => &icons.plain[6],
                            _ => &icons.plain[7],
                        },
                    })
                } else if state == BatteryState::Charging {
                    Some(&icons.bolt)
                } else {
                    None
                };
                let percent_str = format!("{:.0}%", capacity);
                let extents = c.text_extents(&percent_str).unwrap();
                let mut width = extents.width();
                let mut text_offset = 0;
                if let Some(svg) = icon {
                    if !battery_mode.should_draw_text() {
                        width = ICON_SIZE as f64;
                    } else {
                        width += ICON_SIZE as f64;
                    }
                    text_offset = ICON_SIZE;
                    let x =
                        button_left_edge + (button_width as f64 / 2.0 - width / 2.0).round();
                    let y = y_shift + ((height as f64 - ICON_SIZE as f64) / 2.0).round();

                    self.render_svg_with_color(c, svg, x, y, config, self.active);
                }
                if battery_mode.should_draw_text() {
                    self.set_text_color(c, config);
                    c.move_to(
                        button_left_edge + (button_width as f64 / 2.0 - width / 2.0 + text_offset as f64).round(),
                        y_shift + (height as f64 / 2.0 + extents.height() / 2.0).round(),
                    );
                    c.show_text(&percent_str).unwrap();
                }
            }
            ButtonImage::Slider(slider_cfg) => {
                // Render slider icon (overlay will be shown on interaction)
                let x =
                    button_left_edge + (button_width as f64 / 2.0 - (ICON_SIZE / 2) as f64).round();
                let y = y_shift + ((height as f64 - ICON_SIZE as f64) / 2.0).round();

                self.render_svg_with_color(c, &slider_cfg.icon, x, y, config, self.active);
            }
            ButtonImage::SliderText(_, text) => {
                // Render slider text (overlay will be shown on interaction)
                self.set_text_color(c, config);
                let extents = c.text_extents(text).unwrap();
                c.move_to(
                    button_left_edge + (button_width as f64 / 2.0 - extents.width() / 2.0).round(),
                    y_shift + (height as f64 / 2.0 + extents.height() / 2.0).round(),
                );
                c.show_text(text).unwrap();
            }
            ButtonImage::HyprlandWorkspaces(maybe_svg) => {
                // Render workspace icon or fallback text
                if let Some(svg) = maybe_svg {
                    let x =
                        button_left_edge + (button_width as f64 / 2.0 - (ICON_SIZE / 2) as f64).round();
                    let y = y_shift + ((height as f64 - ICON_SIZE as f64) / 2.0).round();
                    self.render_svg_with_color(c, svg, x, y, config, self.active);
                } else {
                    // Fallback: render "WS" text
                    self.set_text_color(c, config);
                    let text = "WS";
                    let extents = c.text_extents(text).unwrap();
                    c.move_to(
                        button_left_edge + (button_width as f64 / 2.0 - extents.width() / 2.0).round(),
                        y_shift + (height as f64 / 2.0 + extents.height() / 2.0).round(),
                    );
                    c.show_text(text).unwrap();
                }
            }
            ButtonImage::Pomodoro => {
                // Pomodoro timer with progress bar and time display
                let pomo = &widgets.pomodoro;
                let bot = y_shift + height as f64 * 0.25;
                let top = y_shift + height as f64 * 0.75;
                let bar_height = top - bot;
                let padding = 6.0;
                let bar_left = button_left_edge + padding;
                let bar_width = button_width as f64 - padding * 2.0;

                // Progress bar background
                c.set_source_rgb(0.15, 0.15, 0.15);
                c.rectangle(bar_left, bot, bar_width, bar_height);
                c.fill().unwrap();

                // Progress bar fill (color based on state)
                let progress = pomo.progress();
                let (r, g, b) = match pomo.state {
                    PomodoroState::Working => (0.8, 0.2, 0.2),
                    PomodoroState::ShortBreak => (0.2, 0.7, 0.3),
                    PomodoroState::LongBreak => (0.2, 0.5, 0.8),
                    PomodoroState::Idle => (0.4, 0.4, 0.4),
                };
                c.set_source_rgb(r, g, b);
                c.rectangle(bar_left, bot, bar_width * progress as f64, bar_height);
                c.fill().unwrap();

                // Time text overlay
                let time_str = pomo.format_time();
                self.set_text_color(c, config);
                c.set_font_size(24.0);
                let extents = c.text_extents(&time_str).unwrap();
                c.move_to(
                    button_left_edge + (button_width as f64 / 2.0 - extents.width() / 2.0).round(),
                    y_shift + (height as f64 / 2.0 + extents.height() / 2.0).round(),
                );
                c.show_text(&time_str).unwrap();
                c.set_font_size(32.0);
            }
            ButtonImage::SystemStats => {
                // System stats: CPU | RAM | Temp as mini bars
                let stats = &widgets.sysinfo;
                let cpu = stats.get_cpu_percent();
                let ram = stats.get_ram_percent();
                let temp = stats.get_temp_celsius();

                let bot = y_shift + height as f64 * 0.3;
                let top = y_shift + height as f64 * 0.7;
                let bar_height = top - bot;
                let padding = 4.0;
                let total_width = button_width as f64 - padding * 2.0;
                let bar_width = (total_width - 4.0) / 3.0; // 3 bars with 2px gaps

                // CPU bar (cyan)
                let cpu_x = button_left_edge + padding;
                c.set_source_rgb(0.15, 0.15, 0.15);
                c.rectangle(cpu_x, bot, bar_width, bar_height);
                c.fill().unwrap();
                c.set_source_rgb(0.0, 0.8, 0.8);
                let cpu_fill = (cpu / 100.0) as f64 * bar_height;
                c.rectangle(cpu_x, top - cpu_fill, bar_width, cpu_fill);
                c.fill().unwrap();

                // RAM bar (magenta)
                let ram_x = cpu_x + bar_width + 2.0;
                c.set_source_rgb(0.15, 0.15, 0.15);
                c.rectangle(ram_x, bot, bar_width, bar_height);
                c.fill().unwrap();
                c.set_source_rgb(0.8, 0.2, 0.8);
                let ram_fill = (ram / 100.0) as f64 * bar_height;
                c.rectangle(ram_x, top - ram_fill, bar_width, ram_fill);
                c.fill().unwrap();

                // Temp bar (yellow/red gradient based on temp)
                let temp_x = ram_x + bar_width + 2.0;
                c.set_source_rgb(0.15, 0.15, 0.15);
                c.rectangle(temp_x, bot, bar_width, bar_height);
                c.fill().unwrap();
                let temp_norm = ((temp - 30.0) / 70.0).clamp(0.0, 1.0) as f64;
                c.set_source_rgb(0.9, 0.9 - temp_norm * 0.7, 0.1);
                let temp_fill = temp_norm * bar_height;
                c.rectangle(temp_x, top - temp_fill, bar_width, temp_fill);
                c.fill().unwrap();
            }
            ButtonImage::Visualizer(bar_count) => {
                // Audio visualizer bars
                let vis = &widgets.visualizer;
                let bot = y_shift + height as f64 * 0.2;
                let top = y_shift + height as f64 * 0.8;
                let bar_height = top - bot;
                let padding = 2.0;
                let total_width = button_width as f64 - padding * 2.0;
                let num_bars = (*bar_count).min(vis.bar_count());
                let bar_gap = 1.0;
                let bar_width = (total_width - bar_gap * (num_bars - 1) as f64) / num_bars as f64;

                for i in 0..num_bars {
                    let x = button_left_edge + padding + i as f64 * (bar_width + bar_gap);
                    let level = vis.bars.get(i).copied().unwrap_or(0.0) as f64;
                    let fill_height = level * bar_height;

                    // Bar background
                    c.set_source_rgb(0.1, 0.1, 0.15);
                    c.rectangle(x, bot, bar_width, bar_height);
                    c.fill().unwrap();

                    // Bar fill with gradient color (blue to purple to pink)
                    let hue = 0.6 + level as f64 * 0.3;
                    let (r, g, b) = hsv_to_rgb(hue, 0.8, 0.9);
                    c.set_source_rgb(r, g, b);
                    c.rectangle(x, top - fill_height, bar_width, fill_height);
                    c.fill().unwrap();
                }
            }
        }
    }
    fn render_svg_with_color(&self, c: &Context, svg: &Handle, x: f64, y: f64, config: &crate::config::Config, is_active: bool) {
        // Save the current Cairo state
        c.save().unwrap();
        
        // Get button-specific colors
        let (_, _, icon_color, icon_color_active, _) = config.colors.get_button_colors(&self.get_text());
        
        // Get the configured color
        let color = if is_active {
            icon_color_active
        } else {
            icon_color
        };
        
        // Create a temporary surface to render the SVG
        let surface = cairo::ImageSurface::create(cairo::Format::ARgb32, ICON_SIZE, ICON_SIZE).unwrap();
        let temp_context = cairo::Context::new(&surface).unwrap();
        
        // Render SVG to the temporary surface
        svg.render_document(&temp_context, &Rectangle::new(0.0, 0.0, ICON_SIZE as f64, ICON_SIZE as f64))
            .unwrap();
        
        // Set our color as the source
        c.set_source_rgba(color[0], color[1], color[2], 1.0);
        
        // Use the SVG as a mask (this will apply our color to the SVG shape)
        let _ = c.mask_surface(&surface, x, y);
        
        // Restore the Cairo state
        c.restore().unwrap();
    }
    fn set_active<F>(&mut self, uinput: &mut UInputHandle<F>, active: bool)
    where
        F: AsRawFd,
    {
        if self.active != active {
            self.active = active;
            self.changed = true;

            toggle_key(uinput, self.action, active as i32);
        }
    }




    fn set_text_color(&self, c: &Context, config: &crate::config::Config) {
        // Get button-specific text color from overrides
        let (_, _, _, _, text_color) = config.colors.get_button_colors(&self.get_text());
        c.set_source_rgb(text_color[0], text_color[1], text_color[2]);
    }

    fn get_text(&self) -> String {
        match &self.image {
            ButtonImage::Text(text) => text.clone(),
            ButtonImage::Time(_, _) => "Time".to_string(),
            ButtonImage::Battery(_, _, _) => "Battery".to_string(),
            ButtonImage::Svg(_) => self.key_to_action_string(),
            ButtonImage::Bitmap(_) => self.key_to_action_string(),
            ButtonImage::Slider(slider_cfg) => match slider_cfg.slider_type {
                SliderType::Volume => "Volume".to_string(),
                SliderType::Brightness => "Brightness".to_string(),
                SliderType::KeyboardBacklight => "KeyboardBacklight".to_string(),
            },
            ButtonImage::SliderText(_, text) => text.clone(),
            ButtonImage::HyprlandWorkspaces(_) => "HyprlandWorkspaces".to_string(),
            ButtonImage::Pomodoro => "Pomodoro".to_string(),
            ButtonImage::SystemStats => "SystemStats".to_string(),
            ButtonImage::Visualizer(_) => "Visualizer".to_string(),
        }
    }

    /// Convert Key enum back to action string for color override lookup
    fn key_to_action_string(&self) -> String {
        match self.action {
            // Function keys
            Key::F1 => "F1".to_string(),
            Key::F2 => "F2".to_string(),
            Key::F3 => "F3".to_string(),
            Key::F4 => "F4".to_string(),
            Key::F5 => "F5".to_string(),
            Key::F6 => "F6".to_string(),
            Key::F7 => "F7".to_string(),
            Key::F8 => "F8".to_string(),
            Key::F9 => "F9".to_string(),
            Key::F10 => "F10".to_string(),
            Key::F11 => "F11".to_string(),
            Key::F12 => "F12".to_string(),
            Key::Esc => "esc".to_string(),
            
            // Media keys
            Key::BrightnessDown => "BrightnessDown".to_string(),
            Key::BrightnessUp => "BrightnessUp".to_string(),
            Key::MicMute => "MicMute".to_string(),
            Key::Search => "Search".to_string(),
            Key::IllumDown => "IllumDown".to_string(),
            Key::IllumUp => "IllumUp".to_string(),
            Key::PreviousSong => "PreviousSong".to_string(),
            Key::PlayPause => "PlayPause".to_string(),
            Key::NextSong => "NextSong".to_string(),
            Key::Mute => "Mute".to_string(),
            Key::VolumeDown => "VolumeDown".to_string(),
            Key::VolumeUp => "VolumeUp".to_string(),
            
            // Fallback for any other keys
            _ => format!("{:?}", self.action),
        }
    }
}

#[derive(Default)]
pub struct FunctionLayer {
    displays_time: bool,
    displays_battery: bool,
    buttons: Vec<(usize, Button)>,
    virtual_button_count: usize,
}

impl FunctionLayer {
    fn with_config(cfg: Vec<ButtonConfig>) -> FunctionLayer {
        if cfg.is_empty() {
            panic!("Invalid configuration, layer has 0 buttons");
        }

        let mut virtual_button_count = 0;
        FunctionLayer {
            displays_time: cfg.iter().any(|cfg| cfg.time.is_some()),
            displays_battery: cfg.iter().any(|cfg| cfg.battery.is_some()),
            buttons: cfg
                .into_iter()
                .scan(&mut virtual_button_count, |state, cfg| {
                    let i = **state;
                    let mut stretch = cfg.stretch.unwrap_or(1);
                    if stretch < 1 {
                        println!("Stretch value must be at least 1, setting to 1.");
                        stretch = 1;
                    }
                    **state += stretch;
                    Some((i, Button::with_config(cfg)))
                })
                .collect(),
            virtual_button_count,
        }
    }
    fn draw(
        &mut self,
        config: &Config,
        width: i32,
        height: i32,
        surface: &Surface,
        pixel_shift: (f64, f64),
        complete_redraw: bool,
        slider_overlay: &SliderOverlay,
        workspace_overlay: &WorkspaceOverlay,
        widgets: &WidgetState,
    ) -> Vec<ClipRect> {
        let c = Context::new(surface).unwrap();
        let mut modified_regions = if complete_redraw {
            vec![ClipRect::new(0, 0, height as u16, width as u16)]
        } else {
            Vec::new()
        };
        c.translate(height as f64, 0.0);
        c.rotate((90.0f64).to_radians());
        let pixel_shift_width = if config.enable_pixel_shift {
            PIXEL_SHIFT_WIDTH_PX
        } else {
            0
        };
        let virtual_button_width = ((width - pixel_shift_width as i32)
            - (BUTTON_SPACING_PX * (self.virtual_button_count - 1) as i32))
            as f64
            / self.virtual_button_count as f64;
        let radius = 8.0f64;
        let bot = (height as f64) * 0.15;
        let top = (height as f64) * 0.85;
        let (pixel_shift_x, pixel_shift_y) = pixel_shift;

        if complete_redraw {
            c.set_source_rgb(0.0, 0.0, 0.0);
            c.paint().unwrap();
        }
        c.set_font_face(&config.font_face);
        c.set_font_size(32.0);

        for i in 0..self.buttons.len() {
            let end = if i + 1 < self.buttons.len() {
                self.buttons[i + 1].0
            } else {
                self.virtual_button_count
            };
            let (start, button) = &mut self.buttons[i];
            let start = *start;

            if !button.changed && !complete_redraw {
                continue;
            };

            let left_edge = (start as f64 * (virtual_button_width + BUTTON_SPACING_PX as f64))
                .floor()
                + pixel_shift_x
                + (pixel_shift_width / 2) as f64;

            let button_width = virtual_button_width
                + ((end - start - 1) as f64 * (virtual_button_width + BUTTON_SPACING_PX as f64))
                    .floor();

            // Get button-specific colors
            let (bg_inactive, bg_active, _, _, _) = 
                config.colors.get_button_colors(&button.get_text());
            
            let (r, g, b) = if button.active {
                (bg_active[0], bg_active[1], bg_active[2])
            } else if config.show_button_outlines {
                (bg_inactive[0], bg_inactive[1], bg_inactive[2])
            } else {
                (0.0, 0.0, 0.0)
            };
            if !complete_redraw {
                c.set_source_rgb(0.0, 0.0, 0.0);
                c.rectangle(
                    left_edge,
                    bot - radius,
                    button_width,
                    top - bot + radius * 2.0,
                );
                c.fill().unwrap();
            }
            // Set the button background color
            c.set_source_rgb(r, g, b);
            
            // draw box with rounded corners
            c.new_sub_path();
            let left = left_edge + radius;
            let right = (left_edge + button_width.ceil()) - radius;
            c.arc(
                right,
                bot,
                radius,
                (-90.0f64).to_radians(),
                (0.0f64).to_radians(),
            );
            c.arc(
                right,
                top,
                radius,
                (0.0f64).to_radians(),
                (90.0f64).to_radians(),
            );
            c.arc(
                left,
                top,
                radius,
                (90.0f64).to_radians(),
                (180.0f64).to_radians(),
            );
            c.arc(
                left,
                bot,
                radius,
                (180.0f64).to_radians(),
                (270.0f64).to_radians(),
            );
            c.close_path();

            c.fill().unwrap();
            button.render(
                &c,
                height,
                left_edge,
                button_width.ceil() as u64,
                pixel_shift_y,
                config,
                widgets,
            );

            button.changed = false;

            if !complete_redraw {
                modified_regions.push(ClipRect::new(
                    height as u16 - top as u16 - radius as u16,
                    left_edge as u16,
                    height as u16 - bot as u16 + radius as u16,
                    left_edge as u16 + button_width as u16,
                ));
            }
        }

        // Render overlays on top if active (workspace overlay takes precedence)
        if workspace_overlay.active {
            render_workspace_overlay(&c, width, height, workspace_overlay, config);
        } else {
            render_slider_overlay(&c, width, height, slider_overlay, config, self);
        }

        modified_regions
    }

    fn hit(&self, width: u16, height: u16, x: f64, y: f64, i: Option<usize>) -> Option<usize> {
        let virtual_button_width =
            (width as i32 - (BUTTON_SPACING_PX * (self.virtual_button_count - 1) as i32)) as f64
                / self.virtual_button_count as f64;

        let i = i.unwrap_or_else(|| {
            let virtual_i = (x / (width as f64 / self.virtual_button_count as f64)) as usize;
            self.buttons
                .iter()
                .position(|(start, _)| *start > virtual_i)
                .unwrap_or(self.buttons.len())
                - 1
        });
        if i >= self.buttons.len() {
            return None;
        }

        let start = self.buttons[i].0;
        let end = if i + 1 < self.buttons.len() {
            self.buttons[i + 1].0
        } else {
            self.virtual_button_count
        };

        let left_edge = (start as f64 * (virtual_button_width + BUTTON_SPACING_PX as f64)).floor();

        let button_width = virtual_button_width
            + ((end - start - 1) as f64 * (virtual_button_width + BUTTON_SPACING_PX as f64))
                .floor();

        if x < left_edge
            || x > (left_edge + button_width)
            || y < 0.1 * height as f64
            || y > 0.9 * height as f64
        {
            return None;
        }

        Some(i)
    }
}

// Touch gesture detection constants
const TAP_HOLD_THRESHOLD_MS: u128 = 300;
const DRAG_THRESHOLD_PX: f64 = 10.0;
const SLIDER_DISMISS_MS: u128 = 2000;
const SLIDER_WIDTH_PX: f64 = 300.0;  // Fixed slider width

// Touch state tracking for slider gestures
struct TouchState {
    down_time: Instant,
    down_x: f64,
    down_y: f64,
    is_dragging: bool,
    layer: usize,
    button: usize,
    last_slider_value: f32,  // Track last slider position for delta calculation
}

// Slider overlay state
struct SliderOverlay {
    active: bool,
    slider_type: SliderType,
    value: f32,  // 0.0 to 1.0
    dismiss_time: Instant,
    button_text: String,  // For color lookup
    button_index: usize,  // Which button to position over
    tracked_volume: f32,  // Track volume internally since we can't reliably read it
}

impl SliderOverlay {
    fn new() -> Self {
        Self {
            active: false,
            slider_type: SliderType::Volume,
            value: 0.0,
            dismiss_time: Instant::now(),
            button_text: String::new(),
            button_index: 0,
            tracked_volume: 0.5,  // Start at 50%, update as we make changes
        }
    }

    fn show(&mut self, slider_type: SliderType, value: f32, button_text: String, button_index: usize) {
        self.active = true;
        self.slider_type = slider_type;
        self.value = value;
        self.dismiss_time = Instant::now();
        self.button_text = button_text;
        self.button_index = button_index;
    }

    fn update_tracked_volume(&mut self, value: f32) {
        self.tracked_volume = value;
    }

    fn get_tracked_volume(&self) -> f32 {
        self.tracked_volume
    }

    fn dismiss(&mut self) {
        self.active = false;
    }

    fn update(&mut self) -> bool {
        if self.active && self.dismiss_time.elapsed().as_millis() > SLIDER_DISMISS_MS {
            self.active = false;
            return true;  // needs redraw
        }
        false
    }

    // Calculate slider bounds - returns (x, y, width, height)
    fn get_bounds(&self, layer: &FunctionLayer, width: i32, height: i32) -> (f64, f64, f64, f64) {
        let virtual_button_width = (width as i32 - (BUTTON_SPACING_PX * (layer.virtual_button_count - 1) as i32)) as f64
            / layer.virtual_button_count as f64;

        let start = layer.buttons[self.button_index].0;
        let end = if self.button_index + 1 < layer.buttons.len() {
            layer.buttons[self.button_index + 1].0
        } else {
            layer.virtual_button_count
        };

        let button_left_edge = (start as f64 * (virtual_button_width + BUTTON_SPACING_PX as f64)).floor();
        let button_width = virtual_button_width
            + ((end - start - 1) as f64 * (virtual_button_width + BUTTON_SPACING_PX as f64)).floor();

        // Fixed slider width, centered on button
        let slider_width = SLIDER_WIDTH_PX;
        let button_center = button_left_edge + button_width / 2.0;
        let slider_x = (button_center - slider_width / 2.0).max(0.0).min(width as f64 - slider_width);

        let slider_height = height as f64 * 0.85;
        let slider_y = (height as f64 - slider_height) / 2.0;

        (slider_x, slider_y, slider_width, slider_height)
    }

    // Check if a touch position is within the slider bounds
    fn contains_point(&self, layer: &FunctionLayer, width: i32, height: i32, x: f64, y: f64) -> bool {
        let (slider_x, slider_y, slider_width, slider_height) = self.get_bounds(layer, width, height);
        x >= slider_x && x <= slider_x + slider_width &&
        y >= slider_y && y <= slider_y + slider_height
    }

    // Convert touch x position to slider value (0.0 to 1.0)
    fn position_to_value(&self, layer: &FunctionLayer, width: i32, height: i32, x: f64) -> f32 {
        let (slider_x, _, slider_width, _) = self.get_bounds(layer, width, height);
        ((x - slider_x) / slider_width).clamp(0.0, 1.0) as f32
    }
}

// Workspace overlay constants
const WORKSPACE_OVERLAY_TIMEOUT_MS: u128 = 10000;
const WORKSPACE_THUMBNAIL_SPACING_PX: f64 = 4.0;
const WORKSPACE_THUMBNAIL_HEIGHT_RATIO: f64 = 0.85;

// Workspace overlay state
struct WorkspaceOverlay {
    active: bool,
    workspaces: Vec<WorkspaceInfo>,
    dismiss_time: Instant,
    selected_workspace: Option<i32>,
    hyprland: HyprlandClient,
    screenshot_cache: ScreenshotCache,
}

impl WorkspaceOverlay {
    fn new(config: &Config, hyprland_socket: Option<String>) -> Self {
        let refresh_interval = config.hyprland.refresh_interval_ms.unwrap_or(5000);
        let screenshot_command = config.hyprland.screenshot_command.clone();

        Self {
            active: false,
            workspaces: Vec::new(),
            dismiss_time: Instant::now(),
            selected_workspace: None,
            hyprland: HyprlandClient::new(hyprland_socket),
            screenshot_cache: ScreenshotCache::new(refresh_interval, screenshot_command),
        }
    }

    fn is_enabled(&self, config: &Config) -> bool {
        config.hyprland.enabled.unwrap_or(true) && self.hyprland.is_available()
    }

    fn show(&mut self, height: i32) {
        self.active = true;
        self.dismiss_time = Instant::now();
        self.selected_workspace = None;

        // Refresh workspace list
        self.workspaces = self.hyprland.get_workspaces();

        // Refresh screenshots if needed
        let target_height = (height as f64 * WORKSPACE_THUMBNAIL_HEIGHT_RATIO) as i32;
        if self.screenshot_cache.needs_refresh() || self.screenshot_cache.get(0).is_none() {
            self.screenshot_cache.refresh(&self.workspaces, target_height);
        }
    }

    fn dismiss(&mut self) {
        self.active = false;
        self.selected_workspace = None;
    }

    fn update(&mut self) -> bool {
        if self.active && self.dismiss_time.elapsed().as_millis() > WORKSPACE_OVERLAY_TIMEOUT_MS {
            self.active = false;
            return true; // needs redraw
        }
        false
    }

    fn hit_test(&self, x: f64, width: i32, height: i32) -> Option<i32> {
        if !self.active || self.workspaces.is_empty() {
            return None;
        }

        let workspace_count = self.workspaces.len();
        let total_spacing = WORKSPACE_THUMBNAIL_SPACING_PX * (workspace_count - 1) as f64;
        let thumbnail_width = (width as f64 - total_spacing) / workspace_count as f64;

        // Calculate dimensions
        let thumbnail_height = height as f64 * WORKSPACE_THUMBNAIL_HEIGHT_RATIO;
        let _y_offset = (height as f64 - thumbnail_height) / 2.0;

        for (i, workspace) in self.workspaces.iter().enumerate() {
            let thumb_x = i as f64 * (thumbnail_width + WORKSPACE_THUMBNAIL_SPACING_PX);

            if x >= thumb_x && x <= thumb_x + thumbnail_width {
                return Some(workspace.id);
            }
        }

        None
    }

    fn select_workspace(&mut self, id: i32, height: i32) -> bool {
        self.selected_workspace = Some(id);
        let success = self.hyprland.switch_workspace(id);
        self.dismiss();

        // After switching, capture the new workspace's screenshot for next time
        if success {
            // Find the monitor for this workspace
            if let Some(workspace) = self.workspaces.iter().find(|w| w.id == id) {
                let target_height = (height as f64 * WORKSPACE_THUMBNAIL_HEIGHT_RATIO) as i32;
                if let Some(surface) = self.screenshot_cache.capture_screenshot(&workspace.monitor, target_height) {
                    self.screenshot_cache.screenshots.insert(id, surface);
                }
            }
        }

        success
    }
}

// Helper functions for sysfs operations
fn read_sysfs_u32(path: &str) -> Result<u32> {
    let content = fs::read_to_string(path)?;
    content.trim().parse::<u32>()
        .map_err(|e| anyhow!("Failed to parse {}: {}", path, e))
}

fn write_sysfs_u32(path: &str, value: u32) -> Result<()> {
    fs::write(path, value.to_string())?;
    Ok(())
}

// Get current slider value (0.0 to 1.0)
fn get_slider_value(slider_type: &SliderType, get_command: Option<&String>) -> f32 {
    use std::process::Command;

    // Use custom command if provided
    if let Some(cmd) = get_command {
        match Command::new("sh").args(&["-c", cmd]).output() {
            Ok(output) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                if let Ok(value) = stdout.trim().parse::<f32>() {
                    return (value / 100.0).clamp(0.0, 1.0);
                }
            }
            Err(_) => {}
        }
    }

    // Fall back to hardcoded behavior
    match slider_type {
        SliderType::Brightness => {
            let max_path = "/sys/class/backlight/acpi_video0/max_brightness";
            let cur_path = "/sys/class/backlight/acpi_video0/brightness";
            match (read_sysfs_u32(max_path), read_sysfs_u32(cur_path)) {
                (Ok(max), Ok(cur)) if max > 0 => cur as f32 / max as f32,
                _ => 0.5,
            }
        }
        SliderType::KeyboardBacklight => {
            let max_path = "/sys/class/leds/apple::kbd_backlight/max_brightness";
            let cur_path = "/sys/class/leds/apple::kbd_backlight/brightness";
            match (read_sysfs_u32(max_path), read_sysfs_u32(cur_path)) {
                (Ok(max), Ok(cur)) if max > 0 => cur as f32 / max as f32,
                _ => 0.5,
            }
        }
        SliderType::Volume => {
            // Read volume with wpctl (works when running as user)
            match Command::new("/usr/bin/wpctl")
                .args(&["get-volume", "@DEFAULT_AUDIO_SINK@"])
                .output() {
                Ok(output) if output.status.success() => {
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    // Output format: "Volume: 0.50" or "Volume: 0.50 [MUTED]"
                    if let Some(vol_str) = stdout.strip_prefix("Volume: ") {
                        // Take just the number part (before any space for [MUTED])
                        let vol_num = vol_str.split_whitespace().next().unwrap_or("0.5");
                        if let Ok(value) = vol_num.parse::<f32>() {
                            return value.clamp(0.0, 1.0);
                        }
                    }
                    0.5  // Default if parse fails
                }
                _ => 0.5,  // Default if command fails
            }
        }
    }
}

// Set slider value (0.0 to 1.0)
// For volume, uses delta-based key events; for others, uses direct sysfs writes
fn set_slider_value<F>(slider_type: &SliderType, value: f32, prev_value: f32, set_command: Option<&String>, uinput: &mut UInputHandle<F>)
where
    F: AsRawFd,
{
    use std::process::Command;
    let clamped_value = value.clamp(0.0, 1.0);
    let percent = (clamped_value * 100.0) as u32;

    // Use custom command if provided
    if let Some(cmd) = set_command {
        let cmd_with_value = cmd.replace("{}", &percent.to_string());
        let _ = Command::new("sh").args(&["-c", &cmd_with_value]).output();
        return;
    }

    // Fall back to hardcoded behavior
    match slider_type {
        SliderType::Brightness => {
            let max_path = "/sys/class/backlight/acpi_video0/max_brightness";
            let cur_path = "/sys/class/backlight/acpi_video0/brightness";
            if let Ok(max) = read_sysfs_u32(max_path) {
                let new_value = (clamped_value * max as f32) as u32;
                let _ = write_sysfs_u32(cur_path, new_value);
            }
        }
        SliderType::KeyboardBacklight => {
            let max_path = "/sys/class/leds/apple::kbd_backlight/max_brightness";
            let cur_path = "/sys/class/leds/apple::kbd_backlight/brightness";
            if let Ok(max) = read_sysfs_u32(max_path) {
                let new_value = (clamped_value * max as f32) as u32;
                let _ = write_sysfs_u32(cur_path, new_value);
            }
        }
        SliderType::Volume => {
            // Use wpctl to set volume directly (works when running as user)
            let _ = Command::new("/usr/bin/wpctl")
                .args(&["set-volume", "@DEFAULT_AUDIO_SINK@", &format!("{}%", percent)])
                .output();
        }
    }
}

struct LayerManager {
    layers: Vec<FunctionLayer>,
    active_layer: usize,
}

impl LayerManager {
    fn new(layers: Vec<FunctionLayer>) -> Self {
        assert!(!layers.is_empty(), "LayerManager requires at least one layer");
        Self {
            layers,
            active_layer: 0,
        }
    }

    fn cycle_layer(&mut self) {
        self.active_layer = (self.active_layer + 1) % self.layers.len();
    }

    fn get_active(&self) -> &FunctionLayer {
        &self.layers[self.active_layer]
    }

    fn get_active_mut(&mut self) -> &mut FunctionLayer {
        &mut self.layers[self.active_layer]
    }

    fn active_index(&self) -> usize {
        self.active_layer
    }
}

// Render slider overlay on top of the display
fn render_slider_overlay(
    c: &Context,
    width: i32,
    height: i32,
    overlay: &SliderOverlay,
    config: &Config,
    layer: &FunctionLayer,
) {
    if !overlay.active {
        return;
    }

    // Get button colors for this slider
    let (bg_inactive, bg_active, _, _, text_color) =
        config.colors.get_button_colors(&overlay.button_text);

    // Get slider bounds
    let (slider_x, slider_y, slider_width, slider_height) = overlay.get_bounds(layer, width, height);

    // Draw track background (inactive button color) with rounded corners
    c.set_source_rgb(bg_inactive[0], bg_inactive[1], bg_inactive[2]);
    draw_rounded_rectangle(c, slider_x, slider_y, slider_width, slider_height, slider_height / 2.0);
    c.fill().unwrap();

    // Draw filled portion (active button color) with rounded end
    let fill_width = slider_width * overlay.value as f64;
    if fill_width > 0.0 {
        c.set_source_rgb(bg_active[0], bg_active[1], bg_active[2]);
        draw_rounded_rectangle(c, slider_x, slider_y, fill_width, slider_height, slider_height / 2.0);
        c.fill().unwrap();
    }

    // Draw percentage text
    let percent = (overlay.value * 100.0) as i32;
    let text = format!("{}%", percent);
    c.set_source_rgb(text_color[0], text_color[1], text_color[2]);
    c.set_font_size(20.0);

    let extents = c.text_extents(&text).unwrap();
    let text_x = slider_x + (slider_width - extents.width()) / 2.0;
    let text_y = slider_y + (slider_height + extents.height()) / 2.0;

    c.move_to(text_x, text_y);
    c.show_text(&text).unwrap();
}

// Helper to draw rounded rectangles
fn draw_rounded_rectangle(c: &Context, x: f64, y: f64, width: f64, height: f64, radius: f64) {
    let radius = radius.min(width / 2.0).min(height / 2.0);

    c.new_path();
    c.arc(x + radius, y + radius, radius, std::f64::consts::PI, 1.5 * std::f64::consts::PI);
    c.arc(x + width - radius, y + radius, radius, 1.5 * std::f64::consts::PI, 2.0 * std::f64::consts::PI);
    c.arc(x + width - radius, y + height - radius, radius, 0.0, 0.5 * std::f64::consts::PI);
    c.arc(x + radius, y + height - radius, radius, 0.5 * std::f64::consts::PI, std::f64::consts::PI);
    c.close_path();
}

// Render workspace overlay on top of the display
fn render_workspace_overlay(
    c: &Context,
    width: i32,
    height: i32,
    overlay: &WorkspaceOverlay,
    config: &Config,
) {
    if !overlay.active || overlay.workspaces.is_empty() {
        return;
    }

    // Draw black background over entire bar
    c.set_source_rgb(0.0, 0.0, 0.0);
    c.rectangle(0.0, 0.0, width as f64, height as f64);
    c.fill().unwrap();

    let workspace_count = overlay.workspaces.len();
    let total_spacing = WORKSPACE_THUMBNAIL_SPACING_PX * (workspace_count - 1) as f64;
    let thumbnail_width = (width as f64 - total_spacing) / workspace_count as f64;
    let thumbnail_height = height as f64 * WORKSPACE_THUMBNAIL_HEIGHT_RATIO;
    let y_offset = (height as f64 - thumbnail_height) / 2.0;
    let radius = 6.0;

    // Get colors for styling
    let (bg_inactive, bg_active, _, _, text_color) =
        config.colors.get_button_colors("HyprlandWorkspaces");

    for (i, workspace) in overlay.workspaces.iter().enumerate() {
        let thumb_x = i as f64 * (thumbnail_width + WORKSPACE_THUMBNAIL_SPACING_PX);

        // Draw background (highlighted for active workspace)
        if workspace.is_active {
            c.set_source_rgb(bg_active[0], bg_active[1], bg_active[2]);
        } else {
            c.set_source_rgb(bg_inactive[0], bg_inactive[1], bg_inactive[2]);
        }
        draw_rounded_rectangle(c, thumb_x, y_offset, thumbnail_width, thumbnail_height, radius);
        c.fill().unwrap();

        // Try to draw screenshot thumbnail
        if let Some(screenshot) = overlay.screenshot_cache.get(workspace.id) {
            let screenshot_width = screenshot.width() as f64;
            let screenshot_height = screenshot.height() as f64;

            // Calculate scaling to fit within thumbnail bounds with padding
            let padding = 2.0;
            let available_width = thumbnail_width - 2.0 * padding;
            let available_height = thumbnail_height - 2.0 * padding;

            let scale_x = available_width / screenshot_width;
            let scale_y = available_height / screenshot_height;
            let scale = scale_x.min(scale_y);

            let scaled_width = screenshot_width * scale;
            let scaled_height = screenshot_height * scale;

            // Center the screenshot
            let img_x = thumb_x + (thumbnail_width - scaled_width) / 2.0;
            let img_y = y_offset + (thumbnail_height - scaled_height) / 2.0;

            c.save().unwrap();
            c.translate(img_x, img_y);
            c.scale(scale, scale);
            c.set_source_surface(screenshot, 0.0, 0.0).unwrap();
            c.paint().unwrap();
            c.restore().unwrap();
        } else {
            // Fallback: draw workspace number
            c.set_source_rgb(text_color[0], text_color[1], text_color[2]);
            c.set_font_size(24.0);

            let text = workspace.id.to_string();
            let extents = c.text_extents(&text).unwrap();
            let text_x = thumb_x + (thumbnail_width - extents.width()) / 2.0;
            let text_y = y_offset + (thumbnail_height + extents.height()) / 2.0;

            c.move_to(text_x, text_y);
            c.show_text(&text).unwrap();
        }
    }
}

struct Interface;

impl LibinputInterface for Interface {
    fn open_restricted(&mut self, path: &Path, flags: i32) -> Result<OwnedFd, i32> {
        let mode = flags & O_ACCMODE;

        OpenOptions::new()
            .custom_flags(flags)
            .read(mode == O_RDONLY || mode == O_RDWR)
            .write(mode == O_WRONLY || mode == O_RDWR)
            .open(path)
            .map(|file| file.into())
            .map_err(|err| err.raw_os_error().unwrap())
    }
    fn close_restricted(&mut self, fd: OwnedFd) {
        _ = File::from(fd);
    }
}

fn emit<F>(uinput: &mut UInputHandle<F>, ty: EventKind, code: u16, value: i32)
where
    F: AsRawFd,
{
    uinput
        .write(&[input_event {
            value,
            type_: ty as u16,
            code,
            time: timeval {
                tv_sec: 0,
                tv_usec: 0,
            },
        }])
        .unwrap();
}

fn toggle_key<F>(uinput: &mut UInputHandle<F>, code: Key, value: i32)
where
    F: AsRawFd,
{
    emit(uinput, EventKind::Key, code as u16, value);
    emit(
        uinput,
        EventKind::Synchronize,
        SynchronizeKind::Report as u16,
        0,
    );
}

fn main() {
    let mut drm = DrmBackend::open_card().unwrap();
    let (height, width) = drm.mode().size();
    let _ = panic::catch_unwind(AssertUnwindSafe(|| real_main(&mut drm)));
    let crash_bitmap = include_bytes!("crash_bitmap.raw");
    let mut map = drm.map().unwrap();
    let data = map.as_mut();
    let mut wptr = 0;
    for byte in crash_bitmap {
        for i in 0..8 {
            let bit = ((byte >> i) & 0x1) == 0;
            let color = if bit { 0xFF } else { 0x0 };
            data[wptr] = color;
            data[wptr + 1] = color;
            data[wptr + 2] = color;
            data[wptr + 3] = color;
            wptr += 4;
        }
    }
    drop(map);
    drm.dirty(&[ClipRect::new(0, 0, height, width)]).unwrap();
    let mut sigset = SigSet::empty();
    sigset.add(Signal::SIGTERM);
    sigset.wait().unwrap();
}

fn real_main(drm: &mut DrmBackend) {
    let (height, width) = drm.mode().size();
    let (db_width, db_height) = drm.fb_info().unwrap().size();
    let mut uinput = UInputHandle::new(OpenOptions::new().write(true).open("/dev/uinput").unwrap());
    let mut backlight = BacklightManager::new();
    let mut last_redraw_minute = Local::now().minute();
    let mut cfg_mgr = ConfigManager::new();
    let (mut cfg, layers) = cfg_mgr.load_config(width);
    let mut pixel_shift = PixelShiftManager::new();

    // Detect Hyprland socket BEFORE dropping privileges
    // (after privdrop, we can't access /run/user/1000/)
    let hyprland_socket = HyprlandClient::detect_socket_path();

    // drop privileges to input and video groups (only if running as root)
    if geteuid().is_root() {
        let groups = ["input", "video"];
        PrivDrop::default()
            .user("nobody")
            .group_list(&groups)
            .apply()
            .unwrap_or_else(|e| panic!("Failed to drop privileges: {}", e));
    }

    let mut surface =
        ImageSurface::create(Format::ARgb32, db_width as i32, db_height as i32).unwrap();
    let mut layer_manager = LayerManager::new(layers);
    let mut needs_complete_redraw = true;

    let mut input_tb = Libinput::new_with_udev(Interface);
    let mut input_main = Libinput::new_with_udev(Interface);
    input_tb.udev_assign_seat("seat-touchbar").unwrap();
    input_main.udev_assign_seat("seat0").unwrap();
    let udev_monitor = MonitorBuilder::new()
        .unwrap()
        .match_subsystem("power_supply")
        .unwrap()
        .listen()
        .unwrap();
    let epoll = Epoll::new(EpollCreateFlags::empty()).unwrap();
    epoll
        .add(input_main.as_fd(), EpollEvent::new(EpollFlags::EPOLLIN, 0))
        .unwrap();
    epoll
        .add(input_tb.as_fd(), EpollEvent::new(EpollFlags::EPOLLIN, 1))
        .unwrap();
    epoll
        .add(cfg_mgr.fd(), EpollEvent::new(EpollFlags::EPOLLIN, 2))
        .unwrap();
    epoll
        .add(&udev_monitor, EpollEvent::new(EpollFlags::EPOLLIN, 3))
        .unwrap();
    uinput.set_evbit(EventKind::Key).unwrap();
    for layer in &layer_manager.layers {
        for button in &layer.buttons {
            uinput.set_keybit(button.1.action).unwrap();
        }
    }
    let mut dev_name_c = [0 as c_char; 80];
    let dev_name = "Dynamic Function Row Virtual Input Device".as_bytes();
    for i in 0..dev_name.len() {
        dev_name_c[i] = dev_name[i] as c_char;
    }
    uinput
        .dev_setup(&uinput_setup {
            id: input_id {
                bustype: 0x19,
                vendor: 0x1209,
                product: 0x316E,
                version: 1,
            },
            ff_effects_max: 0,
            name: dev_name_c,
        })
        .unwrap();
    uinput.dev_create().unwrap();

    let mut digitizer: Option<InputDevice> = None;
    let mut touches: HashMap<i32, TouchState> = HashMap::new();
    let mut slider_overlay = SliderOverlay::new();
    let mut workspace_overlay = WorkspaceOverlay::new(&cfg, hyprland_socket);
    let mut widgets = WidgetState::new();
    loop {
        if cfg_mgr.update_config(&mut cfg, &mut layer_manager.layers, width) {
            layer_manager.active_layer = 0;
            needs_complete_redraw = true;
        }

        let now = Local::now();
        let ms_left = ((60 - now.second()) * 1000) as i32;
        let mut next_timeout_ms = min(ms_left, TIMEOUT_MS);

        if cfg.enable_pixel_shift {
            let (pixel_shift_needs_redraw, pixel_shift_next_timeout_ms) = pixel_shift.update();
            if pixel_shift_needs_redraw {
                needs_complete_redraw = true;
            }
            next_timeout_ms = min(next_timeout_ms, pixel_shift_next_timeout_ms);
        }

        // Update slider overlay auto-dismiss
        if slider_overlay.update() {
            needs_complete_redraw = true;
        }

        // Update widget states (pomodoro, sysinfo, visualizer)
        if widgets.update() {
            needs_complete_redraw = true;
        }

        // Update workspace overlay auto-dismiss
        if workspace_overlay.update() {
            needs_complete_redraw = true;
        }

        let current_minute = now.minute();
        if layer_manager.get_active().displays_time && (current_minute != last_redraw_minute) {
            needs_complete_redraw = true;
            last_redraw_minute = current_minute;
        }
        if layer_manager.get_active().displays_battery {
            for button in &mut layer_manager.get_active_mut().buttons {
                if let ButtonImage::Battery(_, _, _) = button.1.image {
                    button.1.changed = true;
                }
            }
        }

        if needs_complete_redraw || layer_manager.get_active().buttons.iter().any(|b| b.1.changed) {
            let shift = if cfg.enable_pixel_shift {
                pixel_shift.get()
            } else {
                (0.0, 0.0)
            };
            let clips = layer_manager.get_active_mut().draw(
                &cfg,
                width as i32,
                height as i32,
                &surface,
                shift,
                needs_complete_redraw,
                &slider_overlay,
                &workspace_overlay,
                &widgets,
            );
            let data = surface.data().unwrap();
            drm.map().unwrap().as_mut()[..data.len()].copy_from_slice(&data);
            drm.dirty(&clips).unwrap();
            needs_complete_redraw = false;
        }

        match epoll.wait(
            &mut [EpollEvent::new(EpollFlags::EPOLLIN, 0)],
            next_timeout_ms as u16,
        ) {
            Err(Errno::EINTR) | Ok(_) => 0,
            e => e.unwrap(),
        };

        _ = udev_monitor.iter().last();

        input_tb.dispatch().unwrap();
        input_main.dispatch().unwrap();
        for event in &mut input_tb.clone().chain(input_main.clone()) {
            backlight.process_event(&event);
            match event {
                Event::Device(DeviceEvent::Added(evt)) => {
                    let dev = evt.device();
                    if dev.name().contains(" Touch Bar") {
                        digitizer = Some(dev);
                    }
                }
                Event::Keyboard(KeyboardEvent::Key(key)) => {
                    if key.key_state() == KeyState::Pressed {
                        if key.key() == Key::Fn as u32 {
                            // Dismiss overlays if active
                            if workspace_overlay.active {
                                workspace_overlay.dismiss();
                            } else if slider_overlay.active {
                                slider_overlay.dismiss();
                            }
                            layer_manager.cycle_layer();
                            needs_complete_redraw = true;
                        } else if key.key() == Key::Esc as u32 && workspace_overlay.active {
                            // ESC dismisses workspace overlay
                            workspace_overlay.dismiss();
                            needs_complete_redraw = true;
                        }
                    }
                }
                Event::Touch(te) => {
                    if Some(te.device()) != digitizer || backlight.current_bl() == 0 {
                        continue;
                    }
                    match te {
                        TouchEvent::Down(dn) => {
                            let x = dn.x_transformed(width as u32);
                            let y = dn.y_transformed(height as u32);

                            // Handle workspace overlay touch
                            if workspace_overlay.active {
                                if let Some(workspace_id) = workspace_overlay.hit_test(x, width as i32, height as i32) {
                                    workspace_overlay.select_workspace(workspace_id, height as i32);
                                    needs_complete_redraw = true;
                                } else {
                                    // Touch outside thumbnails - dismiss overlay
                                    workspace_overlay.dismiss();
                                    needs_complete_redraw = true;
                                }
                                continue;
                            }

                            if let Some(btn) = layer_manager.get_active().hit(width, height, x, y, None) {
                                touches.insert(dn.seat_slot() as i32, TouchState {
                                    down_time: Instant::now(),
                                    down_x: x,
                                    down_y: y,
                                    is_dragging: false,
                                    layer: layer_manager.active_index(),
                                    button: btn,
                                    last_slider_value: (x / width as f64).clamp(0.0, 1.0) as f32,
                                });

                                // Don't show active state for special widget buttons
                                let is_special = matches!(
                                    layer_manager.get_active().buttons[btn].1.image,
                                    ButtonImage::Slider(_) | ButtonImage::SliderText(_, _) | ButtonImage::HyprlandWorkspaces(_)
                                    | ButtonImage::Pomodoro | ButtonImage::SystemStats | ButtonImage::Visualizer(_)
                                );
                                if !is_special {
                                    layer_manager.get_active_mut().buttons[btn]
                                        .1
                                        .set_active(&mut uinput, true);
                                }
                            }
                        }
                        TouchEvent::Motion(mtn) => {
                            let slot = mtn.seat_slot() as i32;
                            if !touches.contains_key(&slot) {
                                continue;
                            }

                            let x = mtn.x_transformed(width as u32);
                            let y = mtn.y_transformed(height as u32);
                            let touch_state = touches.get_mut(&slot).unwrap();
                            let layer = touch_state.layer;
                            let btn = touch_state.button;

                            // Check if this is a slider button and handle gesture detection
                            let is_slider = matches!(
                                layer_manager.layers[layer].buttons[btn].1.image,
                                ButtonImage::Slider(_) | ButtonImage::SliderText(_, _)
                            );

                            if is_slider && !touch_state.is_dragging {
                                // Check if we should start dragging
                                let elapsed = touch_state.down_time.elapsed().as_millis();
                                let dx = (x - touch_state.down_x).abs();
                                let dy = (y - touch_state.down_y).abs();
                                let distance = (dx * dx + dy * dy).sqrt();

                                if elapsed > TAP_HOLD_THRESHOLD_MS || distance > DRAG_THRESHOLD_PX {
                                    touch_state.is_dragging = true;
                                }
                            }

                            if is_slider && touch_state.is_dragging {
                                // Get slider type and commands from button
                                let (slider_type, get_cmd, set_cmd) = match &layer_manager.layers[layer].buttons[btn].1.image {
                                    ButtonImage::Slider(ref cfg) => (Some(cfg.slider_type), cfg.get_command.as_ref(), cfg.set_command.as_ref()),
                                    ButtonImage::SliderText(ref typ, _) => (Some(*typ), None, None),
                                    _ => (None, None, None),
                                };

                                if let Some(slider_type) = slider_type {
                                    // Show overlay if not already shown
                                    if !slider_overlay.active {
                                        let button_text = layer_manager.layers[layer].buttons[btn].1.get_text();
                                        // For volume, use tracked value; for others, read from system
                                        let current_value = if matches!(slider_type, SliderType::Volume) {
                                            slider_overlay.get_tracked_volume()
                                        } else {
                                            get_slider_value(&slider_type, get_cmd)
                                        };
                                        // Initialize last_slider_value to actual current value
                                        touch_state.last_slider_value = current_value;
                                        slider_overlay.show(slider_type, current_value, button_text, btn);
                                    }

                                    // Only process touch if within slider bounds
                                    if slider_overlay.contains_point(&layer_manager.layers[layer], width as i32, height as i32, x, y) {
                                        // Calculate slider value based on position within slider bounds
                                        let slider_value = slider_overlay.position_to_value(&layer_manager.layers[layer], width as i32, height as i32, x);

                                        // Set the system value based on delta from last position
                                        let prev_value = touch_state.last_slider_value;
                                        set_slider_value(&slider_type, slider_value, prev_value, set_cmd, &mut uinput);
                                        touch_state.last_slider_value = slider_value;

                                        // Update tracked volume for next time
                                        if matches!(slider_type, SliderType::Volume) {
                                            slider_overlay.update_tracked_volume(slider_value);
                                        }

                                        let button_text = layer_manager.layers[layer].buttons[btn].1.get_text();
                                        slider_overlay.show(slider_type, slider_value, button_text, btn);
                                        needs_complete_redraw = true;
                                    }
                                }
                            } else {
                                // Regular button hit detection (only for non-sliders)
                                let hit = layer_manager.get_active()
                                    .hit(width, height, x, y, Some(btn))
                                    .is_some();
                                if !is_slider {
                                    layer_manager.layers[layer].buttons[btn].1.set_active(&mut uinput, hit);
                                }
                            }
                        }
                        TouchEvent::Up(up) => {
                            let slot = up.seat_slot() as i32;
                            if !touches.contains_key(&slot) {
                                continue;
                            }
                            let touch_state = touches.remove(&slot).unwrap();
                            let layer = touch_state.layer;
                            let btn = touch_state.button;

                            // Check button type
                            let is_slider = matches!(
                                layer_manager.layers[layer].buttons[btn].1.image,
                                ButtonImage::Slider(_) | ButtonImage::SliderText(_, _)
                            );
                            let is_hyprland = matches!(
                                layer_manager.layers[layer].buttons[btn].1.image,
                                ButtonImage::HyprlandWorkspaces(_)
                            );
                            let is_pomodoro = matches!(
                                layer_manager.layers[layer].buttons[btn].1.image,
                                ButtonImage::Pomodoro
                            );
                            let is_visualizer = matches!(
                                layer_manager.layers[layer].buttons[btn].1.image,
                                ButtonImage::Visualizer(_)
                            );
                            let is_sysinfo = matches!(
                                layer_manager.layers[layer].buttons[btn].1.image,
                                ButtonImage::SystemStats
                            );

                            if is_slider && !touch_state.is_dragging {
                                let elapsed = touch_state.down_time.elapsed().as_millis();
                                if elapsed < TAP_HOLD_THRESHOLD_MS {
                                    // Short tap - show current value in overlay
                                    let (slider_type, get_cmd, _set_cmd) = match &layer_manager.layers[layer].buttons[btn].1.image {
                                        ButtonImage::Slider(ref cfg) => (Some(cfg.slider_type), cfg.get_command.as_ref(), cfg.set_command.as_ref()),
                                        ButtonImage::SliderText(ref typ, _) => (Some(*typ), None, None),
                                        _ => (None, None, None),
                                    };

                                    if let Some(slider_type) = slider_type {
                                        let current_value = get_slider_value(&slider_type, get_cmd);
                                        let button_text = layer_manager.layers[layer].buttons[btn].1.get_text();
                                        slider_overlay.show(slider_type, current_value, button_text, btn);
                                        needs_complete_redraw = true;
                                    }
                                }
                            } else if is_hyprland && workspace_overlay.is_enabled(&cfg) {
                                // Tap on Hyprland workspaces button - show workspace overlay
                                workspace_overlay.show(height as i32);
                                needs_complete_redraw = true;
                            } else if is_pomodoro {
                                let elapsed = touch_state.down_time.elapsed().as_millis();
                                if elapsed >= TAP_HOLD_THRESHOLD_MS {
                                    // Long press - reset timer
                                    widgets.pomodoro.reset();
                                } else if widgets.pomodoro.state == PomodoroState::Idle {
                                    // Short tap when idle - start timer
                                    widgets.pomodoro.start();
                                } else {
                                    // Short tap when running - pause/resume
                                    widgets.pomodoro.pause();
                                }
                                needs_complete_redraw = true;
                            } else if is_visualizer {
                                // Toggle visualizer on/off
                                if widgets.visualizer.is_running() {
                                    widgets.visualizer.stop();
                                } else {
                                    let _ = widgets.visualizer.start();
                                }
                                needs_complete_redraw = true;
                            } else if is_sysinfo {
                                // Force refresh sysinfo
                                widgets.sysinfo.sample();
                                needs_complete_redraw = true;
                            }

                            // Don't call set_active for special buttons
                            if !is_slider && !is_hyprland && !is_pomodoro && !is_visualizer && !is_sysinfo {
                                layer_manager.layers[layer].buttons[btn].1.set_active(&mut uinput, false);
                            }
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
        backlight.update_backlight(&cfg);
    }
}
