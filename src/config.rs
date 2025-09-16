use crate::fonts::{FontConfig, Pattern};
use crate::FunctionLayer;
use anyhow::Error;
use cairo::FontFace;
use freetype::Library as FtLibrary;
use input_linux::Key;
use nix::{
    errno::Errno,
    sys::inotify::{AddWatchFlags, InitFlags, Inotify, InotifyEvent, WatchDescriptor},
};
use serde::Deserialize;
use std::{fs::read_to_string, os::fd::AsFd};
use std::collections::HashMap;

const USER_CFG_PATH: &str = "/etc/tiny-dfr/config.toml";

#[derive(Debug, Clone)]
pub struct ColorConfig {
    pub button_background_inactive: [f64; 3],
    pub button_background_active: [f64; 3],
    pub icon_color: [f64; 3],
    pub icon_color_active: [f64; 3],
    pub text_color: [f64; 3],
    pub button_overrides: Option<HashMap<String, ButtonColorOverride>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ButtonColorOverride {
    pub button_background_inactive: Option<[f64; 3]>,
    pub button_background_active: Option<[f64; 3]>,
    pub icon_color: Option<[f64; 3]>,
    pub icon_color_active: Option<[f64; 3]>,
    pub text_color: Option<[f64; 3]>,
}

impl Default for ColorConfig {
    fn default() -> Self {
        Self {
            button_background_inactive: [0.2, 0.2, 0.2],
            button_background_active: [0.4, 0.4, 0.4],
            icon_color: [1.0, 1.0, 1.0],
            icon_color_active: [1.0, 1.0, 1.0],
            text_color: [1.0, 1.0, 1.0],
            button_overrides: None,
        }
    }
}

impl ColorConfig {
    pub fn from_theme(theme: &str) -> Self {
        match theme.to_lowercase().as_str() {
            "light" => Self {
                button_background_inactive: [0.9, 0.9, 0.9],
                button_background_active: [0.7, 0.7, 0.7],
                icon_color: [0.1, 0.1, 0.1],
                icon_color_active: [0.0, 0.0, 0.0],
                text_color: [0.1, 0.1, 0.1],
                button_overrides: None,
            },
            "colorful" => Self {
                button_background_inactive: [0.15, 0.15, 0.15],
                button_background_active: [0.35, 0.35, 0.35],
                icon_color: [1.0, 0.8, 0.6],
                icon_color_active: [1.0, 1.0, 0.8],
                text_color: [1.0, 1.0, 1.0],
                button_overrides: None,
            },
            "minimal" => Self {
                button_background_inactive: [0.1, 0.1, 0.1],
                button_background_active: [0.2, 0.2, 0.2],
                icon_color: [0.9, 0.9, 0.9],
                icon_color_active: [1.0, 1.0, 1.0],
                text_color: [0.9, 0.9, 0.9],
                button_overrides: None,
            },
            _ => Self::default(), // "dark" theme or unknown theme
        }
    }

    pub fn get_button_colors(&self, button_text: &str) -> ([f64; 3], [f64; 3], [f64; 3], [f64; 3], [f64; 3]) {
        let mut bg_inactive = self.button_background_inactive;
        let mut bg_active = self.button_background_active;
        let mut icon_color = self.icon_color;
        let mut icon_color_active = self.icon_color_active;
        let mut text_color = self.text_color;

        // Check for button-specific overrides
        if let Some(ref overrides) = self.button_overrides {
            if let Some(override_config) = overrides.get(button_text) {
                if let Some(inactive) = override_config.button_background_inactive {
                    bg_inactive = inactive;
                }
                if let Some(active) = override_config.button_background_active {
                    bg_active = active;
                }
                if let Some(icon) = override_config.icon_color {
                    icon_color = icon;
                }
                if let Some(icon_active) = override_config.icon_color_active {
                    icon_color_active = icon_active;
                }
                if let Some(text) = override_config.text_color {
                    text_color = text;
                }
            }
        }

        (bg_inactive, bg_active, icon_color, icon_color_active, text_color)
    }
}

pub struct Config {
    pub show_button_outlines: bool,
    pub enable_pixel_shift: bool,
    pub font_face: FontFace,
    pub adaptive_brightness: bool,
    pub active_brightness: u32,
    pub colors: ColorConfig,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ConfigProxy {
    media_layer_default: Option<bool>,
    show_button_outlines: Option<bool>,
    enable_pixel_shift: Option<bool>,
    font_template: Option<String>,
    adaptive_brightness: Option<bool>,
    active_brightness: Option<u32>,
    primary_layer_keys: Option<Vec<ButtonConfig>>,
    media_layer_keys: Option<Vec<ButtonConfig>>,
    colors: Option<ColorConfigProxy>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
struct ColorConfigProxy {
    theme: Option<String>,
    button_background_inactive: Option<[f64; 3]>,
    button_background_active: Option<[f64; 3]>,
    icon_color: Option<[f64; 3]>,
    icon_color_active: Option<[f64; 3]>,
    text_color: Option<[f64; 3]>,
    button_overrides: Option<HashMap<String, ButtonColorOverride>>,
}

impl ColorConfigProxy {
    fn to_color_config(&self) -> ColorConfig {
        let mut colors = if let Some(theme) = &self.theme {
            ColorConfig::from_theme(theme)
        } else {
            ColorConfig::default()
        };

        // Override with custom values if provided
        if let Some(inactive) = self.button_background_inactive {
            colors.button_background_inactive = inactive;
        }
        if let Some(active) = self.button_background_active {
            colors.button_background_active = active;
        }
        if let Some(icon_color) = self.icon_color {
            colors.icon_color = icon_color;
        }
        if let Some(icon_color_active) = self.icon_color_active {
            colors.icon_color_active = icon_color_active;
        }
        if let Some(text_color) = self.text_color {
            colors.text_color = text_color;
        }
        if let Some(button_overrides) = &self.button_overrides {
            colors.button_overrides = Some(button_overrides.clone());
        }

        colors
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ButtonConfig {
    #[serde(alias = "Svg")]
    pub icon: Option<String>,
    pub text: Option<String>,
    pub theme: Option<String>,
    pub time: Option<String>,
    pub battery: Option<String>,
    pub locale: Option<String>,
    pub action: Key,
    pub stretch: Option<usize>,
}

fn load_font(name: &str) -> FontFace {
    let fontconfig = FontConfig::new();
    let mut pattern = Pattern::new(name);
    fontconfig.perform_substitutions(&mut pattern);
    let pat_match = match fontconfig.match_pattern(&pattern) {
        Ok(pat) => pat,
        Err(_) => panic!("Unable to find specified font. If you are using the default config, make sure you have at least one font installed")
    };
    let file_name = pat_match.get_file_name();
    let file_idx = pat_match.get_font_index();
    let ft_library = FtLibrary::init().unwrap();
    let face = ft_library.new_face(file_name, file_idx).unwrap();
    FontFace::create_from_ft(&face).unwrap()
}

fn load_config(width: u16) -> (Config, [FunctionLayer; 2]) {
    let mut base =
        toml::from_str::<ConfigProxy>(&read_to_string("/usr/share/tiny-dfr/config.toml").unwrap())
            .unwrap();
    let user = read_to_string(USER_CFG_PATH)
        .map_err::<Error, _>(|e| e.into())
        .and_then(|r| Ok(toml::from_str::<ConfigProxy>(&r)?));
    if let Ok(user) = user {
        base.media_layer_default = user.media_layer_default.or(base.media_layer_default);
        base.show_button_outlines = user.show_button_outlines.or(base.show_button_outlines);
        base.enable_pixel_shift = user.enable_pixel_shift.or(base.enable_pixel_shift);
        base.font_template = user.font_template.or(base.font_template);
        base.adaptive_brightness = user.adaptive_brightness.or(base.adaptive_brightness);
        base.media_layer_keys = user.media_layer_keys.or(base.media_layer_keys);
        base.primary_layer_keys = user.primary_layer_keys.or(base.primary_layer_keys);
        base.active_brightness = user.active_brightness.or(base.active_brightness);
        base.colors = user.colors.or(base.colors);
    };
    let mut media_layer_keys = base.media_layer_keys.unwrap();
    let mut primary_layer_keys = base.primary_layer_keys.unwrap();
    if width >= 2170 {
        for layer in [&mut media_layer_keys, &mut primary_layer_keys] {
            layer.insert(
                0,
                ButtonConfig {
                    icon: None,
                    text: Some("esc".into()),
                    theme: None,
                    action: Key::Esc,
                    stretch: None,
                    time: None,
                    locale: None,
                    battery: None,
                },
            );
        }
    }
    let media_layer = FunctionLayer::with_config(media_layer_keys);
    let fkey_layer = FunctionLayer::with_config(primary_layer_keys);
    let layers = if base.media_layer_default.unwrap() {
        [media_layer, fkey_layer]
    } else {
        [fkey_layer, media_layer]
    };
    let cfg = Config {
        show_button_outlines: base.show_button_outlines.unwrap(),
        enable_pixel_shift: base.enable_pixel_shift.unwrap(),
        adaptive_brightness: base.adaptive_brightness.unwrap(),
        font_face: load_font(&base.font_template.unwrap()),
        active_brightness: base.active_brightness.unwrap(),
        colors: base.colors.unwrap_or_default().to_color_config(),
    };
    (cfg, layers)
}

pub struct ConfigManager {
    inotify_fd: Inotify,
    watch_desc: Option<WatchDescriptor>,
}

fn arm_inotify(inotify_fd: &Inotify) -> Option<WatchDescriptor> {
    let flags = AddWatchFlags::IN_MOVED_TO | AddWatchFlags::IN_CLOSE | AddWatchFlags::IN_ONESHOT;
    match inotify_fd.add_watch(USER_CFG_PATH, flags) {
        Ok(wd) => Some(wd),
        Err(Errno::ENOENT) => None,
        e => Some(e.unwrap()),
    }
}

impl ConfigManager {
    pub fn new() -> ConfigManager {
        let inotify_fd = Inotify::init(InitFlags::IN_NONBLOCK).unwrap();
        let watch_desc = arm_inotify(&inotify_fd);
        ConfigManager {
            inotify_fd,
            watch_desc,
        }
    }
    pub fn load_config(&self, width: u16) -> (Config, [FunctionLayer; 2]) {
        load_config(width)
    }
    pub fn update_config(
        &mut self,
        cfg: &mut Config,
        layers: &mut [FunctionLayer; 2],
        width: u16,
    ) -> bool {
        if self.watch_desc.is_none() {
            self.watch_desc = arm_inotify(&self.inotify_fd);
            return false;
        }
        match self.inotify_fd.read_events() {
            Err(Errno::EAGAIN) => false,
            r => self.handle_events(cfg, layers, width, r),
        }
    }
    #[cold]
    fn handle_events(&mut self, cfg: &mut Config, layers: &mut [FunctionLayer; 2], width: u16, evts: Result<Vec<InotifyEvent>, Errno>) -> bool {
        let mut ret = false;
        for evt in evts.unwrap() {
            if Some(evt.wd) != self.watch_desc {
                continue;
            }
            let parts = load_config(width);
            *cfg = parts.0;
            *layers = parts.1;
            ret = true;
            self.watch_desc = arm_inotify(&self.inotify_fd);
        }
        ret
    }
    pub fn fd(&self) -> &impl AsFd {
        &self.inotify_fd
    }
}
