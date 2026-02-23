use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    pub font: FontConfig,
    pub colors: ColorsConfig,
    pub window: WindowConfig,
    pub terminal: TerminalConfig,
    pub status_bar: StatusBarConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct FontConfig {
    pub family: String,
    pub size: f64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ColorsConfig {
    pub foreground: [f32; 3],
    pub background: [f32; 3],
    pub cursor: [f32; 3],
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct WindowConfig {
    pub width: f64,
    pub height: f64,
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct TerminalConfig {
    pub columns: u16,
    pub rows: u16,
    pub scrollback: usize,
    pub fps: u32,
    pub cursor_blink_frames: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct StatusBarConfig {
    pub enabled: bool,
    pub bg_color: [f32; 3],
    pub fg_color: [f32; 3],
    pub cwd_color: [f32; 3],
    pub branch_color: [f32; 3],
    pub scroll_color: [f32; 3],
    pub time_color: [f32; 3],
}

impl Default for StatusBarConfig {
    fn default() -> Self {
        StatusBarConfig {
            enabled: true,
            bg_color: [0.15, 0.15, 0.18],
            fg_color: [0.6, 0.6, 0.65],
            cwd_color: [0.6, 0.6, 0.65],
            branch_color: [0.4, 0.7, 0.5],
            scroll_color: [0.8, 0.6, 0.3],
            time_color: [0.5, 0.5, 0.55],
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Config {
            font: FontConfig::default(),
            colors: ColorsConfig::default(),
            window: WindowConfig::default(),
            terminal: TerminalConfig::default(),
            status_bar: StatusBarConfig::default(),
        }
    }
}

impl Default for FontConfig {
    fn default() -> Self {
        FontConfig {
            family: "Menlo".to_string(),
            size: 14.0,
        }
    }
}

impl Default for ColorsConfig {
    fn default() -> Self {
        ColorsConfig {
            foreground: [1.0, 1.0, 1.0],
            background: [0.1, 0.1, 0.12],
            cursor: [0.8, 0.8, 0.8],
        }
    }
}

impl Default for WindowConfig {
    fn default() -> Self {
        WindowConfig {
            width: 800.0,
            height: 600.0,
            x: 200.0,
            y: 200.0,
        }
    }
}

impl Default for TerminalConfig {
    fn default() -> Self {
        TerminalConfig {
            columns: 80,
            rows: 24,
            scrollback: 10_000,
            fps: 60,
            cursor_blink_frames: 60,
        }
    }
}

impl Config {
    pub fn load() -> Self {
        let path = config_path();
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                if e.kind() != std::io::ErrorKind::NotFound {
                    log::warn!("Failed to read config at {}: {}", path.display(), e);
                }
                return Config::default();
            }
        };
        match toml::from_str(&content) {
            Ok(config) => {
                log::info!("Loaded config from {}", path.display());
                config
            }
            Err(e) => {
                log::warn!("Invalid config at {}: {}. Using defaults.", path.display(), e);
                Config::default()
            }
        }
    }
}

fn config_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".config/kova/config.toml")
}
