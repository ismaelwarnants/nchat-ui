//! Desktop integration: start at login, single instance, tray icon.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------------------
// X11 window (for file drops) without affecting apps we launch
// ---------------------------------------------------------------------------------------

static ORIG_WAYLAND_DISPLAY: std::sync::OnceLock<Option<std::ffi::OsString>> = std::sync::OnceLock::new();

/// Makes the toolkit open an X11 (XWayland) window: winit only reports dropped files on
/// X11. Must run before the first window exists. Returns true if switched.
pub fn use_x11_window() -> bool {
    if !cfg!(target_os = "linux") || std::env::var_os("DISPLAY").is_none() {
        return false;
    }
    let wl = std::env::var_os("WAYLAND_DISPLAY");
    let _ = ORIG_WAYLAND_DISPLAY.set(wl.clone());
    if wl.is_some() {
        // Safe: startup, before other threads exist.
        unsafe { std::env::remove_var("WAYLAND_DISPLAY") };
        return true;
    }
    false
}

/// Gives a child process (file opener, other apps) the original Wayland session back.
pub fn child_env(cmd: &mut std::process::Command) -> &mut std::process::Command {
    if let Some(Some(wl)) = ORIG_WAYLAND_DISPLAY.get() {
        cmd.env("WAYLAND_DISPLAY", wl);
    }
    cmd
}

// ---------------------------------------------------------------------------------------
// Start at login
// ---------------------------------------------------------------------------------------

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
}

fn autostart_file() -> PathBuf {
    if cfg!(target_os = "macos") {
        home().join("Library/LaunchAgents/io.github.nchat-ui.plist")
    } else {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| home().join(".config"));
        base.join("autostart/nchat-ui.desktop")
    }
}

/// The binary to start at login: the AppImage, the per-user install
/// (~/.local/bin/nchat-ui), or this executable (e.g. /usr/bin/nchat-ui from a package).
fn launch_path() -> PathBuf {
    // inside an AppImage, current_exe() is a temporary mount: use the .AppImage itself
    if let Some(appimage) = std::env::var_os("APPIMAGE") {
        return PathBuf::from(appimage);
    }
    let installed = home().join(".local/bin/nchat-ui");
    if installed.exists() {
        installed
    } else {
        std::env::current_exe().unwrap_or(installed)
    }
}

pub fn set_autostart(enable: bool, extra_args: &[String]) -> std::io::Result<()> {
    let file = autostart_file();
    if !enable {
        if file.exists() {
            std::fs::remove_file(&file)?;
        }
        return Ok(());
    }
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let exe = launch_path();
    let content = if cfg!(target_os = "macos") {
        let mut args = format!("<string>{}</string><string>--background</string>", exe.display());
        for a in extra_args {
            args.push_str(&format!("<string>{a}</string>"));
        }
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
             <plist version=\"1.0\"><dict>\n\
             <key>Label</key><string>io.github.nchat-ui</string>\n\
             <key>ProgramArguments</key><array>{args}</array>\n\
             <key>RunAtLoad</key><true/>\n\
             </dict></plist>\n"
        )
    } else {
        let quoted: Vec<String> = extra_args.iter().map(|a| format!("\"{}\"", a.replace('"', "\\\""))).collect();
        format!(
            "[Desktop Entry]\nType=Application\nName=nchat-ui\nComment=Start nchat-ui in the background\n\
             Exec=\"{}\" --background {}\nIcon=nchat-ui\nTerminal=false\nX-GNOME-Autostart-enabled=true\n\
             X-GNOME-Autostart-Delay=3\n",
            exe.display(),
            quoted.join(" ")
        )
    };
    std::fs::write(&file, content)
}

// ---------------------------------------------------------------------------------------
// Single instance: a second start asks the running one to show its window and exits
// ---------------------------------------------------------------------------------------

fn socket_path(confdir: &Path) -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    // one instance per config dir (nchat locks it anyway)
    let mut h: u64 = 1469598103934665603;
    for b in confdir.to_string_lossy().bytes() {
        h = (h ^ b as u64).wrapping_mul(1099511628211);
    }
    base.join(format!("nchat-ui-{:x}.sock", h))
}

/// Returns true if another instance is running (and was asked to show its window).
pub fn signal_running_instance(confdir: &Path, show: bool) -> bool {
    match UnixStream::connect(socket_path(confdir)) {
        Ok(mut s) => {
            let _ = s.write_all(if show { b"show\n" } else { b"ping\n" });
            true
        }
        Err(_) => false,
    }
}

/// Listens for other instances; `on_show` runs (on a background thread) when one asks.
pub fn listen_for_instances(confdir: &Path, on_show: impl Fn() + Send + 'static) {
    let path = socket_path(confdir);
    let _ = std::fs::remove_file(&path); // stale socket from a crashed run
    let Ok(listener) = UnixListener::bind(&path) else { return };
    std::thread::Builder::new()
        .name("nchat-instance".into())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut line = String::new();
                if BufReader::new(stream).read_line(&mut line).is_ok() && line.trim() == "show" {
                    on_show();
                }
            }
        })
        .ok();
}

// ---------------------------------------------------------------------------------------
// Tray icon (StatusNotifierItem: KDE, GNOME with the AppIndicator extension, ...)
// ---------------------------------------------------------------------------------------

pub enum TrayCmd {
    Show,
    MarkAllRead,
    ToggleStartAtLogin,
    Quit,
}

#[cfg(target_os = "linux")]
mod tray_impl {
    use super::TrayCmd;
    use ksni::blocking::TrayMethods;

    type Callback = Box<dyn Fn(TrayCmd) + Send>;

    pub struct NchatTray {
        unread: i32,
        start_at_login: bool,
        status: String,
        icon: Vec<ksni::Icon>,
        icon_unread: Vec<ksni::Icon>,
        on: Callback,
    }

    impl ksni::Tray for NchatTray {
        fn id(&self) -> String {
            "nchat-ui".into()
        }
        fn title(&self) -> String {
            "nchat-ui".into()
        }
        fn icon_name(&self) -> String {
            String::new() // use the pixmaps below (themes may not have our icon)
        }
        fn icon_pixmap(&self) -> Vec<ksni::Icon> {
            if self.unread > 0 { self.icon_unread.clone() } else { self.icon.clone() }
        }
        fn tool_tip(&self) -> ksni::ToolTip {
            let description = match self.unread {
                0 => "No unread chats".to_string(),
                1 => "1 unread chat".to_string(),
                n => format!("{n} unread chats"),
            };
            ksni::ToolTip { title: "nchat-ui".into(), description, ..Default::default() }
        }
        fn activate(&mut self, _x: i32, _y: i32) {
            (self.on)(TrayCmd::Show);
        }
        fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
            use ksni::menu::{CheckmarkItem, StandardItem};
            let unread_label = match self.unread {
                0 => "No unread chats".to_string(),
                n => format!("{n} unread chat{}", if n == 1 { "" } else { "s" }),
            };
            vec![
                StandardItem {
                    label: "Open nchat-ui".into(),
                    activate: Box::new(|t: &mut Self| (t.on)(TrayCmd::Show)),
                    ..Default::default()
                }
                .into(),
                StandardItem { label: unread_label, enabled: false, ..Default::default() }.into(),
                StandardItem { label: self.status.clone(), enabled: false, visible: !self.status.is_empty(), ..Default::default() }.into(),
                StandardItem {
                    label: "Mark all as read".into(),
                    enabled: self.unread > 0,
                    activate: Box::new(|t: &mut Self| (t.on)(TrayCmd::MarkAllRead)),
                    ..Default::default()
                }
                .into(),
                ksni::MenuItem::Separator,
                CheckmarkItem {
                    label: "Start when I log in".into(),
                    checked: self.start_at_login,
                    activate: Box::new(|t: &mut Self| (t.on)(TrayCmd::ToggleStartAtLogin)),
                    ..Default::default()
                }
                .into(),
                ksni::MenuItem::Separator,
                StandardItem {
                    label: "Quit nchat-ui".into(),
                    activate: Box::new(|t: &mut Self| (t.on)(TrayCmd::Quit)),
                    ..Default::default()
                }
                .into(),
            ]
        }
    }

    /// Renders the app icon (ui/icon.svg) to ARGB32 pixmaps, optionally with an unread dot.
    fn render_icon(size: u32, dot: bool) -> Option<ksni::Icon> {
        use resvg::{tiny_skia, usvg};
        let svg = include_bytes!("../ui/icon.svg");
        let tree = usvg::Tree::from_data(svg, &usvg::Options::default()).ok()?;
        let mut pixmap = tiny_skia::Pixmap::new(size, size)?;
        let scale = size as f32 / tree.size().width();
        resvg::render(&tree, tiny_skia::Transform::from_scale(scale, scale), &mut pixmap.as_mut());
        if dot {
            let r = size as f32 * 0.22;
            let c = size as f32 - r - 1.0;
            if let Some(path) = tiny_skia::PathBuilder::from_circle(c, size as f32 - c, r) {
                let mut paint = tiny_skia::Paint::default();
                paint.set_color_rgba8(0xe5, 0x48, 0x4d, 0xff);
                paint.anti_alias = true;
                pixmap.fill_path(&path, &paint, tiny_skia::FillRule::Winding, tiny_skia::Transform::identity(), None);
            }
        }
        // premultiplied RGBA -> straight ARGB, network byte order
        let mut data = Vec::with_capacity((size * size * 4) as usize);
        for p in pixmap.pixels() {
            let c = p.demultiply();
            data.extend_from_slice(&[c.alpha(), c.red(), c.green(), c.blue()]);
        }
        Some(ksni::Icon { width: size as i32, height: size as i32, data })
    }

    /// The tray runs its own D-Bus service; `ksni`'s update waits until the update has
    /// been sent to the panel (GNOME Shell), which can take seconds while the shell is
    /// busy. Updates therefore go through a thread of their own, never the GUI thread.
    pub struct Tray(std::sync::mpsc::Sender<(i32, bool, String)>);

    impl Tray {
        pub fn start(start_at_login: bool, on: impl Fn(TrayCmd) + Send + 'static) -> Option<Tray> {
            let sizes = [22u32, 32, 48, 64];
            let tray = NchatTray {
                unread: 0,
                start_at_login,
                status: String::new(),
                icon: sizes.iter().filter_map(|s| render_icon(*s, false)).collect(),
                icon_unread: sizes.iter().filter_map(|s| render_icon(*s, true)).collect(),
                on: Box::new(on),
            };
            let handle = tray.spawn().ok()?;
            let (tx, rx) = std::sync::mpsc::channel::<(i32, bool, String)>();
            std::thread::Builder::new()
                .name("tray-update".into())
                .spawn(move || {
                    while let Ok(mut state) = rx.recv() {
                        // only the newest state matters
                        while let Ok(newer) = rx.try_recv() {
                            state = newer;
                        }
                        let (unread, start_at_login, status) = state;
                        handle.update(move |t| {
                            t.unread = unread;
                            t.start_at_login = start_at_login;
                            t.status = status;
                        });
                    }
                })
                .ok()?;
            Some(Tray(tx))
        }

        pub fn update(&self, unread: i32, start_at_login: bool, status: &str) {
            let _ = self.0.send((unread, start_at_login, status.to_string()));
        }
    }
}

#[cfg(target_os = "linux")]
pub use tray_impl::Tray;

/// No tray implementation on this platform yet (macOS: menu bar item, to do).
#[cfg(not(target_os = "linux"))]
pub struct Tray;

#[cfg(not(target_os = "linux"))]
impl Tray {
    pub fn start(_start_at_login: bool, _on: impl Fn(TrayCmd) + Send + 'static) -> Option<Tray> {
        None
    }
    pub fn update(&self, _unread: i32, _start_at_login: bool, _status: &str) {}
}

/// Whether a tray host (StatusNotifierWatcher) is running, e.g. GNOME's AppIndicator
/// extension or KDE Plasma.
pub fn tray_host_available() -> bool {
    if !cfg!(target_os = "linux") {
        return false;
    }
    std::process::Command::new("gdbus")
        .args([
            "call", "--session", "--dest", "org.kde.StatusNotifierWatcher", "--object-path",
            "/StatusNotifierWatcher", "--method", "org.freedesktop.DBus.Properties.Get",
            "org.kde.StatusNotifierWatcher", "IsStatusNotifierHostRegistered",
        ])
        .output()
        .map(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).contains("true"))
        .unwrap_or(false)
}
