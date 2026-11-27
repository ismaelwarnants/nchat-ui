//! UI settings, stored in nchat-ui's own file ($XDG_CONFIG_HOME/nchat-ui/settings.conf,
//! simple `key=value` lines). nchat's own config files are left alone.

use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    /// "system" | "light" | "dark"
    pub theme: String,
    /// accent colour, "#rrggbb"
    pub accent: String,
    /// empty = toolkit default font
    pub font_family: String,
    /// base text size in px (default 14)
    pub font_size: f32,
    /// smaller chat list rows
    pub compact: bool,
    /// message bubble corner radius in px
    pub bubble_radius: f32,
    /// conversation background, empty = theme default
    pub chat_background: String,
    /// own bubbles colour, empty = derived from accent
    pub bubble_color: String,
    pub enter_sends: bool,
    pub time_24h: bool,
    /// "none" | "images" | "media" (images + audio) | "all"
    pub auto_download: String,
    pub mark_read_on_view: bool,
    pub share_online: bool,
    pub share_typing: bool,
    pub notifications: bool,
    pub notification_preview: bool,
    pub show_archived_in_list: bool,
    /// colour emoji font family, empty = automatic (applies at next start)
    pub emoji_font: String,
    /// most recently used emoji first
    pub recent_emojis: Vec<String>,
    /// closing the window keeps nchat-ui running (tray icon, notifications)
    pub run_in_background: bool,
    /// start (in the background) when logging in to the desktop
    pub start_at_login: bool,
    /// the "still running in the background" hint was shown once
    pub background_hint_shown: bool,
    /// run the window through X11/XWayland so files can be dropped on it (Linux)
    pub x11_window: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            theme: "system".into(),
            accent: "#3a76f0".into(),
            font_family: String::new(),
            font_size: 14.0,
            compact: false,
            bubble_radius: 14.0,
            chat_background: String::new(),
            bubble_color: String::new(),
            enter_sends: true,
            time_24h: true,
            auto_download: "media".into(),
            mark_read_on_view: true,
            share_online: true,
            share_typing: true,
            notifications: true,
            notification_preview: true,
            show_archived_in_list: false,
            emoji_font: String::new(),
            recent_emojis: Vec::new(),
            run_in_background: false,
            start_at_login: false,
            background_hint_shown: false,
            x11_window: true,
        }
    }
}

pub fn path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".config"));
    base.join("nchat-ui").join("settings.conf")
}

fn parse_bool(v: &str) -> Option<bool> {
    match v.trim() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

impl Settings {
    pub fn load() -> Settings {
        let mut s = Settings::default();
        let Ok(text) = std::fs::read_to_string(path()) else { return s };
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with('#') {
                continue;
            }
            let Some((k, v)) = line.split_once('=') else { continue };
            let (k, v) = (k.trim(), v.trim().to_string());
            let b = parse_bool(&v);
            match k {
                "theme" if ["system", "light", "dark"].contains(&v.as_str()) => s.theme = v,
                "accent" if is_hex_color(&v) => s.accent = v,
                "font_family" => s.font_family = v,
                "font_size" => s.font_size = v.parse().unwrap_or(s.font_size).clamp(10.0, 24.0),
                "compact" => s.compact = b.unwrap_or(s.compact),
                "bubble_radius" => s.bubble_radius = v.parse().unwrap_or(s.bubble_radius).clamp(0.0, 24.0),
                "chat_background" if v.is_empty() || is_hex_color(&v) => s.chat_background = v,
                "bubble_color" if v.is_empty() || is_hex_color(&v) => s.bubble_color = v,
                "enter_sends" => s.enter_sends = b.unwrap_or(s.enter_sends),
                "time_24h" => s.time_24h = b.unwrap_or(s.time_24h),
                "auto_download" if ["none", "images", "media", "all"].contains(&v.as_str()) => s.auto_download = v,
                "mark_read_on_view" => s.mark_read_on_view = b.unwrap_or(s.mark_read_on_view),
                "share_online" => s.share_online = b.unwrap_or(s.share_online),
                "share_typing" => s.share_typing = b.unwrap_or(s.share_typing),
                "notifications" => s.notifications = b.unwrap_or(s.notifications),
                "notification_preview" => s.notification_preview = b.unwrap_or(s.notification_preview),
                "show_archived_in_list" => s.show_archived_in_list = b.unwrap_or(s.show_archived_in_list),
                "emoji_font" => s.emoji_font = v,
                "run_in_background" => s.run_in_background = b.unwrap_or(s.run_in_background),
                "start_at_login" => s.start_at_login = b.unwrap_or(s.start_at_login),
                "background_hint_shown" => s.background_hint_shown = b.unwrap_or(s.background_hint_shown),
                "x11_window" => s.x11_window = b.unwrap_or(s.x11_window),
                "recent_emojis" => s.recent_emojis = v.split(' ').filter(|e| !e.is_empty()).map(String::from).collect(),
                _ => {}
            }
        }
        s
    }

    pub fn save(&self) {
        let text = format!(
            "# nchat-ui settings (edit in the app: ⋮ → Settings)\n\
             theme={}\naccent={}\nfont_family={}\nfont_size={}\ncompact={}\nbubble_radius={}\n\
             chat_background={}\nbubble_color={}\nenter_sends={}\ntime_24h={}\nauto_download={}\n\
             mark_read_on_view={}\nshare_online={}\nshare_typing={}\nnotifications={}\n\
             notification_preview={}\nshow_archived_in_list={}\nemoji_font={}\nrecent_emojis={}\n\
             run_in_background={}\nstart_at_login={}\nbackground_hint_shown={}\nx11_window={}\n",
            self.theme, self.accent, self.font_family, self.font_size, self.compact as u8,
            self.bubble_radius, self.chat_background, self.bubble_color, self.enter_sends as u8,
            self.time_24h as u8, self.auto_download, self.mark_read_on_view as u8,
            self.share_online as u8, self.share_typing as u8, self.notifications as u8,
            self.notification_preview as u8, self.show_archived_in_list as u8, self.emoji_font,
            self.recent_emojis.join(" "),
            self.run_in_background as u8, self.start_at_login as u8, self.background_hint_shown as u8,
            self.x11_window as u8,
        );
        let p = path();
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(e) = std::fs::write(&p, text) {
            eprintln!("nchat-ui: could not save settings to {}: {e}", p.display());
        }
    }
}

pub fn is_hex_color(v: &str) -> bool {
    v.len() == 7 && v.starts_with('#') && v[1..].chars().all(|c| c.is_ascii_hexdigit())
}

pub fn parse_color(v: &str) -> Option<slint::Color> {
    if !is_hex_color(v) {
        return None;
    }
    let n = u32::from_str_radix(&v[1..], 16).ok()?;
    Some(slint::Color::from_rgb_u8((n >> 16) as u8, (n >> 8) as u8, n as u8))
}

pub fn color_hex(c: slint::Color) -> String {
    format!("#{:02x}{:02x}{:02x}", c.red(), c.green(), c.blue())
}

/// Installed font families (via fontconfig's fc-list), sorted, for the font picker.
pub fn font_families() -> Vec<String> {
    let out = std::process::Command::new("fc-list").args([":", "family"]).output();
    let mut fams: Vec<String> = match out {
        Ok(o) => String::from_utf8_lossy(&o.stdout)
            .lines()
            .filter_map(|l| l.split(',').next())
            .map(|f| f.trim().to_string())
            .filter(|f| !f.is_empty() && !f.starts_with('.'))
            .collect(),
        Err(_) => Vec::new(),
    };
    fams.sort_by_key(|f| f.to_lowercase());
    fams.dedup();
    fams
}
