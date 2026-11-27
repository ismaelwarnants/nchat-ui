//! Emoji: font setup (so every Unicode emoji renders in colour) and picker data.

use slint::{ModelRc, SharedString, VecModel};

use crate::EmojiRow;

pub const COLUMNS: usize = 9;

/// Newest emoji version shown in the picker. Distro emoji fonts lag behind Unicode
/// (Ubuntu 24.04's Noto Color Emoji stops at 15.1, roughly what WhatsApp supports too),
/// and newer emoji would show as empty cells. Override with NCHAT_UI_EMOJI_VERSION=16.0.
fn max_version() -> emojis::UnicodeVersion {
    let v = std::env::var("NCHAT_UI_EMOJI_VERSION").unwrap_or_default();
    let mut it = v.split('.').map(|p| p.parse::<u32>().ok());
    match (it.next().flatten(), it.next().flatten()) {
        (Some(a), b) => emojis::UnicodeVersion::new(a, b.unwrap_or(0)),
        _ => emojis::UnicodeVersion::new(15, 1),
    }
}

/// Picker tabs: (icon, group). `None` = recently used.
pub const TABS: [(&str, Option<emojis::Group>); 10] = [
    ("🕘", None),
    ("😀", Some(emojis::Group::SmileysAndEmotion)),
    ("👋", Some(emojis::Group::PeopleAndBody)),
    ("🐻", Some(emojis::Group::AnimalsAndNature)),
    ("🍔", Some(emojis::Group::FoodAndDrink)),
    ("✈️", Some(emojis::Group::TravelAndPlaces)),
    ("⚽", Some(emojis::Group::Activities)),
    ("💡", Some(emojis::Group::Objects)),
    ("🔣", Some(emojis::Group::Symbols)),
    ("🏁", Some(emojis::Group::Flags)),
];

pub fn tab_icons() -> ModelRc<SharedString> {
    ModelRc::from(std::rc::Rc::new(VecModel::from(TABS.iter().map(|(i, _)| SharedString::from(*i)).collect::<Vec<_>>())))
}

/// Emoji of a tab (recent list for tab 0) or of a search, as rows for the picker grid.
pub fn rows(tab: usize, search: &str, recent: &[String]) -> Vec<EmojiRow> {
    let search = search.trim().to_lowercase();
    let list: Vec<String> = if !search.is_empty() {
        let max = max_version();
        emojis::iter()
            .filter(|e| e.unicode_version() <= max)
            .filter(|e| {
                e.name().to_lowercase().contains(&search) || e.shortcodes().any(|s| s.contains(&search))
            })
            .map(|e| e.as_str().to_string())
            .collect()
    } else {
        match TABS.get(tab).and_then(|(_, g)| *g) {
            Some(group) => {
                let max = max_version();
                group.emojis().filter(|e| e.unicode_version() <= max).map(|e| e.as_str().to_string()).collect()
            }
            None => recent.to_vec(),
        }
    };
    to_rows(list)
}

pub fn to_rows(list: Vec<String>) -> Vec<EmojiRow> {
    list.chunks(COLUMNS)
        .map(|chunk| EmojiRow {
            cells: ModelRc::from(std::rc::Rc::new(VecModel::from(
                chunk.iter().map(|s| SharedString::from(s.as_str())).collect::<Vec<_>>(),
            ))),
        })
        .collect()
}

/// Installed colour emoji fonts (family names), for the "Emoji style" setting.
pub fn emoji_font_families() -> Vec<String> {
    let mut fams: Vec<String> = std::process::Command::new("fc-list")
        .args([":color=true", "family"])
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter_map(|l| l.split(',').next())
                .map(|f| f.trim().to_string())
                .filter(|f| !f.is_empty())
                .collect()
        })
        .unwrap_or_default();
    fams.sort();
    fams.dedup();
    fams
}

fn fc_match_file(pattern: &str) -> Option<String> {
    let out = std::process::Command::new("fc-match").args(["-f", "%{file}", pattern]).output().ok()?;
    let f = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!f.is_empty() && std::path::Path::new(&f).is_file()).then_some(f)
}

/// Slint's text engine falls back to the system "sans-serif" fonts, which on many Linux
/// systems include fonts with black-and-white emoji outlines, and it splits keycaps
/// (3️⃣) and flags. Making the fallback chain "text font, then colour emoji font" fixes
/// both. Must run before the first window is created.
pub fn setup_fonts(text_family: &str, emoji_family: &str) {
    if std::env::var_os("SLINT_DEFAULT_FONT").is_some() || std::env::var_os("SLINT_FONT_PATH").is_some() {
        return; // user override
    }
    if !cfg!(target_os = "linux") {
        return; // macOS picks Apple Color Emoji itself
    }
    let text = if text_family.is_empty() { "sans-serif".to_string() } else { text_family.to_string() };
    let Some(regular) = fc_match_file(&text) else { return };
    let mut chain: Vec<String> = Vec::new();
    for style in [":bold", ":italic", ":bold:italic"] {
        if let Some(f) = fc_match_file(&format!("{text}{style}")) {
            if f != regular && !chain.contains(&f) {
                chain.push(f);
            }
        }
    }
    let emoji_pattern = if emoji_family.is_empty() { "emoji:color=true".to_string() } else { emoji_family.to_string() };
    if let Some(e) = fc_match_file(&emoji_pattern) {
        chain.push(e);
    }
    // Safe: called at startup before any other thread exists.
    unsafe {
        std::env::set_var("SLINT_DEFAULT_FONT", regular);
        std::env::set_var("SLINT_FONT_PATH", chain.join(":"));
    }
}
