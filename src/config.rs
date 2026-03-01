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
    pub tab_bar: TabBarConfig,
    pub splits: SplitsConfig,
    pub global_status_bar: GlobalStatusBarConfig,
    pub keys: KeysConfig,
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
    pub scroll_sensitivity: f64,
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
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct GlobalStatusBarConfig {
    pub bg_color: [f32; 3],
    pub fg_color: [f32; 3],
    pub time_color: [f32; 3],
    pub scroll_indicator_color: [f32; 3],
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SplitsConfig {
    pub min_width: f32,
}

impl Default for SplitsConfig {
    fn default() -> Self {
        SplitsConfig { min_width: 300.0 }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct TabBarConfig {
    pub bg_color: [f32; 3],
    pub fg_color: [f32; 3],
    pub active_bg: [f32; 3],
}

impl Default for TabBarConfig {
    fn default() -> Self {
        TabBarConfig {
            bg_color: [0.12, 0.12, 0.14],
            fg_color: [0.5, 0.5, 0.55],
            active_bg: [0.22, 0.22, 0.26],
        }
    }
}

impl Default for StatusBarConfig {
    fn default() -> Self {
        StatusBarConfig {
            enabled: true,
            bg_color: [0.15, 0.15, 0.18],
            fg_color: [0.8, 0.8, 0.85],
            cwd_color: [0.6, 0.6, 0.65],
            branch_color: [0.4, 0.7, 0.5],
            scroll_color: [0.8, 0.6, 0.3],
        }
    }
}

impl Default for GlobalStatusBarConfig {
    fn default() -> Self {
        GlobalStatusBarConfig {
            bg_color: [0.10, 0.10, 0.12],
            fg_color: [0.8, 0.8, 0.85],
            time_color: [0.65, 0.65, 0.7],
            scroll_indicator_color: [0.8, 0.6, 0.3],
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
            tab_bar: TabBarConfig::default(),
            splits: SplitsConfig::default(),
            global_status_bar: GlobalStatusBarConfig::default(),
            keys: KeysConfig::default(),
        }
    }
}

impl Default for FontConfig {
    fn default() -> Self {
        FontConfig {
            family: "Hack".to_string(),
            size: 13.0,
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
            scroll_sensitivity: 6.0,
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

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct KeysConfig {
    pub new_tab: String,
    pub close_pane_or_tab: String,
    pub vsplit: String,
    pub hsplit: String,
    pub vsplit_root: String,
    pub hsplit_root: String,
    pub new_window: String,
    pub close_window: String,
    pub kill_window: String,
    pub copy: String,
    pub paste: String,
    pub toggle_filter: String,
    pub clear_scrollback: String,
    pub prev_tab: String,
    pub next_tab: String,
    pub rename_tab: String,
    pub detach_tab: String,
    pub merge_window: String,
    pub switch_tab_1: String,
    pub switch_tab_2: String,
    pub switch_tab_3: String,
    pub switch_tab_4: String,
    pub switch_tab_5: String,
    pub switch_tab_6: String,
    pub switch_tab_7: String,
    pub switch_tab_8: String,
    pub switch_tab_9: String,
    pub navigate_up: String,
    pub navigate_down: String,
    pub navigate_left: String,
    pub navigate_right: String,
    pub swap_up: String,
    pub swap_down: String,
    pub swap_left: String,
    pub swap_right: String,
    pub resize_left: String,
    pub resize_right: String,
    pub resize_up: String,
    pub resize_down: String,
    pub terminal: TerminalKeysConfig,
}

impl Default for KeysConfig {
    fn default() -> Self {
        KeysConfig {
            new_tab: "cmd+t".into(),
            close_pane_or_tab: "cmd+w".into(),
            vsplit: "cmd+d".into(),
            hsplit: "cmd+shift+d".into(),
            vsplit_root: "cmd+e".into(),
            hsplit_root: "cmd+shift+e".into(),
            new_window: "cmd+n".into(),
            close_window: "cmd+q".into(),
            kill_window: "cmd+option+q".into(),
            copy: "cmd+c".into(),
            paste: "cmd+v".into(),
            toggle_filter: "cmd+f".into(),
            clear_scrollback: "cmd+k".into(),
            prev_tab: "cmd+shift+[".into(),
            next_tab: "cmd+shift+]".into(),
            rename_tab: "cmd+shift+r".into(),
            detach_tab: "cmd+shift+t".into(),
            merge_window: "cmd+shift+m".into(),
            switch_tab_1: "cmd+1".into(),
            switch_tab_2: "cmd+2".into(),
            switch_tab_3: "cmd+3".into(),
            switch_tab_4: "cmd+4".into(),
            switch_tab_5: "cmd+5".into(),
            switch_tab_6: "cmd+6".into(),
            switch_tab_7: "cmd+7".into(),
            switch_tab_8: "cmd+8".into(),
            switch_tab_9: "cmd+9".into(),
            navigate_up: "cmd+option+up".into(),
            navigate_down: "cmd+option+down".into(),
            navigate_left: "cmd+option+left".into(),
            navigate_right: "cmd+option+right".into(),
            swap_up: "cmd+shift+up".into(),
            swap_down: "cmd+shift+down".into(),
            swap_left: "cmd+shift+left".into(),
            swap_right: "cmd+shift+right".into(),
            resize_left: "cmd+ctrl+left".into(),
            resize_right: "cmd+ctrl+right".into(),
            resize_up: "cmd+ctrl+up".into(),
            resize_down: "cmd+ctrl+down".into(),
            terminal: TerminalKeysConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct TerminalKeysConfig {
    pub kill_line: String,
    pub home: String,
    pub end: String,
    pub word_back: String,
    pub word_forward: String,
    pub shift_enter: String,
}

impl Default for TerminalKeysConfig {
    fn default() -> Self {
        TerminalKeysConfig {
            kill_line: "cmd+backspace".into(),
            home: "cmd+left".into(),
            end: "cmd+right".into(),
            word_back: "option+left".into(),
            word_forward: "option+right".into(),
            shift_enter: "shift+enter".into(),
        }
    }
}
