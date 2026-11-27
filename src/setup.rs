//! Account setup wizard logic. Interprets the text nchat's own setup code prints on its
//! terminal (see term.rs) and turns it into a form: answers the protocol / auth-method
//! menus automatically, shows prompts (phone number, login code, 2FA password, ...) as an
//! input field, and shows QR codes / pairing codes.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use slint::{Color, ComponentHandle, ModelRc, SharedPixelBuffer, VecModel};

use crate::term::{self, TermEvent};
use crate::{AppWindow, SetupProto, SetupState};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Hidden,
    /// choosing which account type to add
    Choose,
    /// nchat's setup (`main -s`) is running
    Setup,
    /// nchat asked for input / showed a QR outside setup (e.g. session expired at login)
    Reauth,
    /// nchat's `-r` (remove account) is running; `protocol` holds the profile id
    Remove,
}

struct Prompt {
    label: String,
    placeholder: String,
    secret: bool,
    enter_only: bool,
}

pub struct Setup {
    pub mode: Mode,
    protocol: String,
    first_run: bool,
    log: String,
    partial: String,
    info: Vec<String>,
    menu: Vec<(String, String)>, // (index, name) of the last numbered menu
    qr: Option<slint::Image>,
    code: String,
    prompt: Option<Prompt>,
    prompt_token: i32,
    error: String,
    status: String,
    pub succeeded: bool,
    profiles_before: HashSet<String>,
}

impl Setup {
    pub fn new() -> Self {
        Setup {
            mode: Mode::Hidden,
            protocol: String::new(),
            first_run: true,
            log: String::new(),
            partial: String::new(),
            info: Vec::new(),
            menu: Vec::new(),
            qr: None,
            code: String::new(),
            prompt: None,
            prompt_token: 0,
            error: String::new(),
            status: String::new(),
            succeeded: false,
            profiles_before: HashSet::new(),
        }
    }

    pub fn show_choose(&mut self, first_run: bool, confdir: &Path) {
        self.mode = Mode::Choose;
        self.first_run = first_run;
        self.profiles_before = list_profiles(confdir);
    }

    /// Protocol chosen: reset and remember what to answer in nchat's protocol menu.
    pub fn start(&mut self, protocol: &str) {
        let first_run = self.first_run;
        let before = std::mem::take(&mut self.profiles_before);
        *self = Setup::new();
        self.first_run = first_run;
        self.profiles_before = before;
        self.mode = Mode::Setup;
        self.protocol = protocol.to_string();
        self.status = "Starting…".into();
    }

    pub fn start_remove(&mut self, profile: &str) {
        *self = Setup::new();
        self.mode = Mode::Remove;
        self.protocol = profile.to_string();
        self.status = "Removing account…".into();
    }

    /// Profile dirs that appeared since the wizard opened (to clean up on cancel).
    pub fn new_profiles(&self, confdir: &Path) -> Vec<PathBuf> {
        list_profiles(confdir)
            .difference(&self.profiles_before)
            .map(|p| confdir.join("profiles").join(p))
            .collect()
    }

    pub fn hide(&mut self) {
        self.mode = Mode::Hidden;
        self.prompt = None;
        self.qr = None;
        self.code.clear();
    }

    /// Returns true if the wizard state changed visibly.
    pub fn on_term(&mut self, ev: TermEvent, use_code: bool) -> bool {
        match ev {
            TermEvent::ShowImage(path) => {
                if let Some(img) = load_png(&path) {
                    self.qr = Some(img);
                    self.status = "Waiting for you to scan the code…".into();
                    if self.mode == Mode::Hidden {
                        self.mode = Mode::Reauth;
                    }
                }
                true
            }
            TermEvent::Text(text) => {
                self.log.push_str(&text);
                if self.log.len() > 20_000 {
                    let cut = self.log.len() - 15_000;
                    let cut = (cut..self.log.len()).find(|i| self.log.is_char_boundary(*i)).unwrap_or(0);
                    self.log.drain(..cut);
                }
                self.partial.push_str(&text);
                while let Some(nl) = self.partial.find('\n') {
                    let line: String = self.partial[..nl].to_string();
                    self.partial.drain(..=nl);
                    self.on_line(&line);
                }
                self.check_prompt(use_code);
                true
            }
        }
    }

    fn on_line(&mut self, line: &str) {
        let t = line.trim();
        if t.is_empty() {
            return;
        }
        // numbered menu entries ("0. Dummy", "1. QR code in image viewer  (~/...)")
        if let Some((idx, name)) = t.split_once(". ") {
            if !idx.is_empty() && idx.chars().all(|c| c.is_ascii_digit()) {
                self.menu.push((idx.to_string(), name.trim().to_string()));
                return;
            }
        }
        if t.ends_with(':') && (t.starts_with("Protocols") || t.starts_with("Authentication methods")) {
            self.menu.clear();
            return;
        }
        if t.contains("uccesfully set up profile") || t.contains("uccessfully set up profile") {
            self.succeeded = true;
            self.prompt = None;
            self.qr = None;
            self.code.clear();
            self.info.clear();
            self.error.clear();
            self.status = "Account added. Connecting…".into();
            return;
        }
        if t.starts_with("Setup aborted") || t.starts_with("Setup failed") || t.contains("failed to connect") {
            self.error = t.to_string();
            self.status.clear();
            return;
        }
        if let Some(code) = t.strip_prefix("Code: ") {
            self.code = code.trim().to_string();
            self.status = "Waiting for confirmation on your phone…".into();
            return;
        }
        if t.contains("Press ENTER") {
            self.info.push(t.to_string());
            self.prompt = Some(Prompt { label: String::new(), placeholder: String::new(), secret: false, enter_only: true });
            self.prompt_token += 1;
            return;
        }
        if t.starts_with("Phone number must") || t.starts_with("Phone number is") || t.starts_with("Invalid") {
            self.error = t.to_string();
            return;
        }
        if t.starts_with("Passkey response sent") {
            self.status = t.to_string();
            return;
        }
        self.info.push(t.to_string());
        if self.info.len() > 10 {
            self.info.remove(0);
        }
    }

    fn check_prompt(&mut self, use_code: bool) {
        let p = self.partial.trim().to_string();
        if p.is_empty() || !(p.ends_with(':') || p.ends_with('?')) {
            return;
        }
        if p.starts_with("Remove profile") && self.mode == Mode::Remove {
            // menu lines are "<n>. <profile id>"
            let answer = self.menu.iter().find(|(_, n)| *n == self.protocol).map(|(i, _)| i.clone());
            term::write_line(&answer.unwrap_or_default()); // empty = cancel removal
            self.partial.clear();
            self.menu.clear();
            return;
        }
        if p.starts_with("Select protocol") {
            let wanted = self.protocol.to_lowercase();
            let answer = self.menu.iter().find(|(_, n)| n.to_lowercase() == wanted).map(|(i, _)| i.clone());
            match answer {
                Some(i) => term::write_line(&i),
                None => {
                    self.error = format!("This build of nchat has no {} support.", self.protocol);
                    term::write_line(""); // default = exit setup
                }
            }
            self.partial.clear();
            self.menu.clear();
            self.status = "Starting…".into();
            return;
        }
        if p.starts_with("Select method") {
            // 1 = QR code image, 3 = code (pairing code / login code) when offered
            let has_code = self.menu.iter().any(|(i, _)| i == "3");
            term::write_line(if use_code && has_code { "3" } else { "1" });
            self.partial.clear();
            self.menu.clear();
            self.info.clear();
            self.status = "Connecting…".into();
            return;
        }
        if self.mode == Mode::Hidden {
            self.mode = Mode::Reauth;
        }
        let label = p.trim_end_matches([':', '?']).trim().to_string();
        let (mut label, placeholder) = match label.split_once(" (ex. ") {
            Some((l, ex)) => (l.to_string(), ex.trim_end_matches(')').to_string()),
            None => (label, String::new()),
        };
        if label == "Phone number" {
            label = "Phone number, with country code".into();
        }
        let lower = label.to_lowercase();
        let secret = lower.contains("password") || lower.contains("passcode");
        let placeholder = if !placeholder.is_empty() {
            placeholder
        } else if lower.contains("login code") {
            "Code sent to your Telegram app or by SMS".into()
        } else {
            String::new()
        };
        self.prompt = Some(Prompt { label, placeholder, secret, enter_only: false });
        self.prompt_token += 1;
        self.status.clear();
        self.partial.clear();
    }

    pub fn submit(&mut self, value: &str) {
        if self.prompt.take().is_none() {
            return;
        }
        term::write_line(value.trim());
        self.error.clear();
        self.info.clear();
        self.status = "Please wait…".into();
    }

    pub fn push_to(&self, w: &AppWindow, protocols: &[(String, String)]) {
        let s = w.global::<SetupState>();
        s.set_visible(self.mode != Mode::Hidden);
        s.set_step(if self.mode == Mode::Choose { 0 } else { 1 });
        s.set_first_run(self.first_run);
        let protos: Vec<SetupProto> = protocols
            .iter()
            .map(|(id, name)| proto_card(id, name))
            .collect();
        s.set_protocols(ModelRc::from(std::rc::Rc::new(VecModel::from(protos))));
        // cancel makes no sense before there is any account to go back to
        s.set_can_cancel(!(self.mode == Mode::Choose && self.first_run) && !matches!(self.mode, Mode::Reauth | Mode::Remove));
        let title = match (self.mode, self.protocol.as_str()) {
            (Mode::Reauth, _) => "Sign in again".to_string(),
            (Mode::Remove, _) => "Remove account".to_string(),
            (_, "WhatsAppMd") => "Link WhatsApp".to_string(),
            (_, "Telegram") => "Log in to Telegram".to_string(),
            (_, "Signal") => "Link Signal".to_string(),
            (_, "Dummy") => "Demo account".to_string(),
            (_, other) => format!("Add {other}"),
        };
        s.set_title(title.into());
        s.set_info(paragraphs(&self.info).into());
        s.set_has_qr(self.qr.is_some());
        if let Some(qr) = &self.qr {
            s.set_qr(qr.clone());
        }
        s.set_code(self.code.clone().into());
        match &self.prompt {
            Some(p) => {
                s.set_has_prompt(true);
                s.set_prompt_label(p.label.clone().into());
                s.set_prompt_placeholder(p.placeholder.clone().into());
                s.set_prompt_secret(p.secret);
                s.set_prompt_enter_only(p.enter_only);
            }
            None => s.set_has_prompt(false),
        }
        s.set_prompt_token(self.prompt_token);
        s.set_error(self.error.clone().into());
        s.set_status(self.status.clone().into());
        s.set_log(self.log.clone().into());
    }
}

/// nchat wraps its instructions for an 80-column terminal: re-flow them into paragraphs
/// (a line ending a sentence ends a paragraph) and drop terminal-only hints.
fn paragraphs(lines: &[String]) -> String {
    let mut out = String::new();
    for line in lines {
        let line = line
            .replace(", or press CTRL-C to abort.", ".")
            .replace(" or press CTRL-C to abort.", ".")
            .replace("Qr code", "QR code");
        if line.contains("press CTRL-C") || line == "to abort." {
            continue;
        }
        if !out.is_empty() {
            out.push(if out.ends_with(['.', ':', '!', '?']) { '\n' } else { ' ' });
        }
        out.push_str(line.trim());
    }
    out
}

fn proto_card(id: &str, name: &str) -> SetupProto {
    let (icon, color, hint) = match id {
        "WhatsAppMd" => ("W", Color::from_rgb_u8(0x25, 0xa8, 0x5a), "Scan a QR code with the WhatsApp app on your phone"),
        "Telegram" => ("T", Color::from_rgb_u8(0x2a, 0x9e, 0xd8), "Scan a QR code, or log in with phone number + code"),
        "Signal" => ("S", Color::from_rgb_u8(0x3a, 0x76, 0xf0), "Link as a secondary device"),
        _ => ("D", Color::from_rgb_u8(0x8a, 0x8f, 0x98), "Fake chats to try the app, no account needed"),
    };
    SetupProto { id: id.into(), name: name.into(), icon: icon.into(), color, hint: hint.into() }
}

pub fn list_profiles(confdir: &Path) -> HashSet<String> {
    std::fs::read_dir(confdir.join("profiles"))
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.path().is_dir())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .filter(|n| n.contains('_'))
                .collect()
        })
        .unwrap_or_default()
}

/// Decodes the PNG ourselves (instead of Image::load_from_path) because nchat rewrites the
/// same file for every new QR code and path-based loading may be cached.
fn load_png(path: &str) -> Option<slint::Image> {
    let file = std::fs::File::open(path).ok()?;
    let mut decoder = png::Decoder::new(std::io::BufReader::new(file));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    let (w, h) = (info.width, info.height);
    let bytes = &buf[..info.buffer_size()];
    let mut pixels = SharedPixelBuffer::<slint::Rgba8Pixel>::new(w, h);
    let out = pixels.make_mut_bytes();
    match info.color_type {
        png::ColorType::Rgba => out.copy_from_slice(bytes),
        png::ColorType::Rgb => {
            for (o, i) in out.chunks_mut(4).zip(bytes.chunks(3)) {
                o.copy_from_slice(&[i[0], i[1], i[2], 255]);
            }
        }
        png::ColorType::Grayscale => {
            for (o, g) in out.chunks_mut(4).zip(bytes.iter()) {
                o.copy_from_slice(&[*g, *g, *g, 255]);
            }
        }
        png::ColorType::GrayscaleAlpha => {
            for (o, i) in out.chunks_mut(4).zip(bytes.chunks(2)) {
                o.copy_from_slice(&[i[0], i[0], i[0], i[1]]);
            }
        }
        _ => return None,
    }
    Some(slint::Image::from_rgba8(pixels))
}
