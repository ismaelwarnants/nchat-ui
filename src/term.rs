//! nchat's "terminal": stdin/stdout of the process are pointed at a pseudo-terminal whose
//! other end is read and written by the window. nchat's own setup code (account wizard,
//! QR login, Telegram code/password prompts, re-authentication) then runs unmodified and
//! the window plays the part of the terminal.
//!
//! QR images: nchat shows them by running `xdg-open <png>` (SetupUtil::ShowImage). We put
//! a tiny `xdg-open` stand-in first on PATH (for this process only) that reports the path
//! back over the pty, so the QR is shown inside the window instead of an external viewer.

use std::ffi::CStr;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

/// What the window receives from nchat's terminal.
pub enum TermEvent {
    /// Plain text (ANSI escapes removed, CRLF normalised).
    Text(String),
    /// nchat asked to show an image (QR code) at this path.
    ShowImage(String),
}

static WRITER: OnceLock<Mutex<File>> = OnceLock::new();
static REAL_XDG_OPEN: OnceLock<Option<PathBuf>> = OnceLock::new();

const OSC_PREFIX: &str = "\u{1b}]777;nchat-ui-image;";

/// The command nchat uses to show images: `xdg-open` on Linux, `open` on macOS.
const OPENER: &str = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };

/// The real opener, for opening files/folders/links from the app (PATH has our stand-in
/// first).
pub fn xdg_open_command() -> std::process::Command {
    let real = REAL_XDG_OPEN.get().cloned().flatten().unwrap_or_else(|| PathBuf::from(OPENER));
    let mut cmd = std::process::Command::new(real);
    crate::desktop::child_env(&mut cmd);
    cmd
}

fn find_in_path(prog: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")?
        .to_string_lossy()
        .split(':')
        .filter(|d| !d.ends_with("nchat-ui-shim")) // our own stand-in (after a restart)
        .map(|d| PathBuf::from(d).join(prog))
        .find(|p| p.is_file())
}

/// Redirects fd 0/1 to a new pty and starts a reader thread delivering TermEvents.
pub fn install(on_event: impl Fn(TermEvent) + Send + 'static) -> std::io::Result<()> {
    let _ = REAL_XDG_OPEN.set(find_in_path(OPENER));

    let (master, slave) = unsafe {
        let mut master: RawFd = -1;
        let mut slave: RawFd = -1;
        let mut ws: libc::winsize = std::mem::zeroed();
        ws.ws_row = 60;
        ws.ws_col = 160;
        if libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null(), &ws) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        // no echo: the window shows what the user typed itself (and hides passwords)
        let mut t: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(slave, &mut t) == 0 {
            t.c_lflag &= !(libc::ECHO | libc::ECHONL);
            libc::tcsetattr(slave, libc::TCSANOW, &t);
        }
        libc::fcntl(master, libc::F_SETFD, libc::FD_CLOEXEC);
        (master, slave)
    };

    let slave_name = unsafe {
        let mut buf = [0 as libc::c_char; 128];
        if libc::ttyname_r(slave, buf.as_mut_ptr(), buf.len()) == 0 {
            CStr::from_ptr(buf.as_ptr()).to_string_lossy().to_string()
        } else {
            String::new()
        }
    };

    install_xdg_open_shim(&slave_name)?;

    unsafe {
        // flush Rust's buffered stdout before swapping the fd underneath it
        let _ = std::io::stdout().flush();
        libc::dup2(slave, 0);
        libc::dup2(slave, 1);
        libc::close(slave);
    }

    let reader = unsafe { File::from_raw_fd(master) };
    let writer = reader.try_clone()?;
    let _ = WRITER.set(Mutex::new(writer));

    std::thread::Builder::new()
        .name("nchat-term".into())
        .spawn(move || read_loop(reader, on_event))?;
    Ok(())
}

/// Sends a line of input to nchat (as if typed in its terminal).
pub fn write_line(text: &str) {
    if let Some(w) = WRITER.get() {
        let mut w = w.lock().unwrap();
        let _ = w.write_all(text.as_bytes());
        let _ = w.write_all(b"\n");
        let _ = w.flush();
    }
}

fn install_xdg_open_shim(pty: &str) -> std::io::Result<()> {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let dir = base.join("nchat-ui-shim");
    std::fs::create_dir_all(&dir)?;
    let shim = dir.join(OPENER);
    // ShowImage runs `xdg-open >/dev/null 2>&1 '<path>' &`, so write to the pty by name.
    std::fs::write(
        &shim,
        "#!/bin/sh\n# nchat-ui: forwards images nchat wants to show (QR codes) to the window\n\
         [ -n \"$NCHAT_UI_PTY\" ] && printf '\\033]777;nchat-ui-image;%s\\007' \"$1\" > \"$NCHAT_UI_PTY\"\nexit 0\n",
    )?;
    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755))?;

    let shim_dir = dir.display().to_string();
    let path: Vec<String> = std::env::var("PATH")
        .unwrap_or_default()
        .split(':')
        .filter(|p| *p != shim_dir)
        .map(String::from)
        .collect();
    let path = path.join(":");
    // Safe here: called at startup before nchat's threads exist.
    unsafe {
        std::env::set_var("PATH", format!("{}:{}", dir.display(), path));
        std::env::set_var("NCHAT_UI_PTY", pty);
    }
    Ok(())
}

fn read_loop(mut reader: File, on_event: impl Fn(TermEvent)) {
    let mut buf = [0u8; 4096];
    let mut pending = Vec::<u8>::new();
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        pending.extend_from_slice(&buf[..n]);
        // keep an incomplete UTF-8 sequence / escape sequence for the next read
        let (text, rest) = split_complete(&pending);
        pending = rest;
        if text.is_empty() {
            continue;
        }
        if std::env::var_os("NCHAT_UI_DEBUG").is_some() {
            eprint!("[term] {text}");
        }
        let mut plain = String::new();
        let mut rest = text.as_str();
        while let Some(start) = rest.find(OSC_PREFIX) {
            plain.push_str(&rest[..start]);
            let after = &rest[start + OSC_PREFIX.len()..];
            match after.find('\u{7}') {
                Some(end) => {
                    flush_text(&mut plain, &on_event);
                    on_event(TermEvent::ShowImage(after[..end].to_string()));
                    rest = &after[end + 1..];
                }
                None => {
                    rest = after;
                    break;
                }
            }
        }
        plain.push_str(rest);
        flush_text(&mut plain, &on_event);
    }
}

fn flush_text(plain: &mut String, on_event: &impl Fn(TermEvent)) {
    if !plain.is_empty() {
        on_event(TermEvent::Text(strip_ansi(plain)));
        plain.clear();
    }
}

/// Splits into (complete text, leftover bytes): leftover = trailing partial UTF-8 char or
/// an escape sequence that hasn't finished yet.
fn split_complete(bytes: &[u8]) -> (String, Vec<u8>) {
    let mut cut = bytes.len();
    // unfinished OSC (ESC ] ... BEL) or CSI at the end
    if let Some(esc) = bytes.iter().rposition(|b| *b == 0x1b) {
        let tail = &bytes[esc..];
        let finished = if tail.get(1) == Some(&b']') {
            tail.contains(&0x07)
        } else if tail.get(1) == Some(&b'[') {
            tail.iter().skip(2).any(|b| (0x40..=0x7e).contains(b))
        } else {
            tail.len() >= 2
        };
        if !finished {
            cut = esc;
        }
    }
    let (head, tail) = bytes.split_at(cut);
    match std::str::from_utf8(head) {
        Ok(s) => (s.to_string(), tail.to_vec()),
        Err(e) => {
            let valid = e.valid_up_to();
            let mut rest = head[valid..].to_vec();
            rest.extend_from_slice(tail);
            if rest.len() > 8 {
                // genuinely invalid bytes, don't hold them forever
                (String::from_utf8_lossy(head).to_string(), tail.to_vec())
            } else {
                (String::from_utf8_lossy(&head[..valid]).to_string(), rest)
            }
        }
    }
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => match chars.peek() {
                Some('[') => {
                    chars.next();
                    while let Some(&n) = chars.peek() {
                        chars.next();
                        if ('@'..='~').contains(&n) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    for n in chars.by_ref() {
                        if n == '\u{7}' {
                            break;
                        }
                    }
                }
                _ => {
                    chars.next();
                }
            },
            '\r' => {}
            _ => out.push(c),
        }
    }
    out
}
