//! nchat-ui: desktop chat client. Runs upstream nchat in-process (its main() on a
//! background thread, our Ui class in place of the ncurses one) and shows a Slint window.

mod audio;
mod core;
mod desktop;
mod emoji;
mod ffi;
mod headless;
mod notify;
mod openwith;
mod record;
mod richtext;
mod settings;
mod setup;
mod term;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use slint::ComponentHandle;

slint::include_modules!();

fn print_help() {
    println!(
        "nchat-ui {} - desktop UI for nchat ({})

Usage: nchat-ui [OPTIONS]

  -d, --confdir <DIR>   use a different config dir (default ~/.config/nchat)
  -s, --setup           open the \"add account\" wizard
  -b, --background      start without a window (tray icon / notifications only)
  -e, --verbose         verbose logging to <confdir>/log.txt
  -ee, --extra-verbose  extra verbose logging
  -r, --remove          remove an account (prompts in this terminal), then exit
  -x, --export <DIR>    export message cache to DIR, then exit
  -v, --version         show nchat version info
  -h, --help            show this help

Accounts, history and settings are shared with the terminal nchat. Only one of
nchat / nchat-ui can run on the same config dir at a time.",
        env!("CARGO_PKG_VERSION"),
        env!("NCHAT_UI_FEATURES"),
    );
}

/// Account types compiled into this binary: (nchat protocol name, display name).
fn protocols() -> Vec<(String, String)> {
    let features = env!("NCHAT_UI_FEATURES");
    let mut list = Vec::new();
    for (key, id, name) in [("whatsapp", "WhatsAppMd", "WhatsApp"), ("telegram", "Telegram", "Telegram"), ("signal", "Signal", "Signal")] {
        if features.contains(&format!("{key}=true")) {
            list.push((id.to_string(), name.to_string()));
        }
    }
    if std::env::var_os("NCHAT_UI_SHOW_DEMO").is_some() || list.is_empty() {
        list.push(("Dummy".into(), "Demo".into()));
    }
    list
}

fn main() {
    // Telegram API key built in with NCHAT_UI_TG_APIID / NCHAT_UI_TG_APIHASH (nchat reads
    // TG_APIID / TG_APIHASH; a key set in the environment at runtime still wins)
    if let (Some(id), Some(hash)) = (option_env!("NCHAT_UI_TG_APIID"), option_env!("NCHAT_UI_TG_APIHASH")) {
        if std::env::var_os("TG_APIID").is_none() && std::env::var_os("TG_APIHASH").is_none() {
            std::env::set_var("TG_APIID", id);
            std::env::set_var("TG_APIHASH", hash);
        }
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut core_args = vec!["nchat-ui".to_string()];
    let mut restart_args = Vec::new();
    let mut gui = true;
    let mut add_account = false;
    let mut remove_account: Option<String> = None;
    let mut start_hidden = false;
    let mut confdir: Option<PathBuf> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-h" | "--help" => {
                print_help();
                return;
            }
            "-s" | "--setup" | "--add-account" => add_account = true,
            "-b" | "--background" => start_hidden = true,
            // internal: restart target of Settings → Accounts → Remove
            "--remove-account" => remove_account = it.next().cloned(),
            "-v" | "--version" | "-r" | "--remove" | "-x" | "--export" => {
                gui = false;
                core_args.push(a.clone());
            }
            "-d" | "--confdir" => {
                core_args.push(a.clone());
                restart_args.push(a.clone());
                if let Some(dir) = it.next() {
                    confdir = Some(PathBuf::from(dir));
                    core_args.push(dir.clone());
                    restart_args.push(dir.clone());
                }
            }
            "-k" | "--keydump" | "-u" | "--upgrade" | "-t" | "--theme" | "-m" | "--devmode" => {
                eprintln!("{a} is a terminal-nchat option; use the `nchat` binary for it");
                std::process::exit(1);
            }
            _ => {
                core_args.push(a.clone());
                restart_args.push(a.clone());
            }
        }
    }

    if !gui {
        std::process::exit(ffi::run_core(core_args));
    }

    let confdir = confdir.unwrap_or_else(|| PathBuf::from(ffi::default_confdir()));
    let script = std::env::var("NCHAT_UI_SCRIPT").ok();

    // Already running (e.g. in the background)? Bring that window up instead.
    if script.is_none() && !add_account && remove_account.is_none()
        && desktop::signal_running_instance(&confdir, !start_hidden)
    {
        return;
    }
    {
        // colour emoji everywhere (must happen before the first window exists)
        let st = settings::Settings::load();
        emoji::setup_fonts(&st.font_family, &st.emoji_font);
        // file drops only reach X11 windows (winit has no Wayland drag & drop yet)
        if st.x11_window && script.is_none() {
            desktop::use_x11_window();
        }
    }
    if script.is_some() {
        headless::install();
    }

    // window identity, so the desktop shows our icon/name for it (matches nchat-ui.desktop)
    let _ = slint::set_xdg_app_id("nchat-ui");

    let rx = core::channel();

    // nchat's stdin/stdout become a pty the window reads/writes (setup wizard, QR codes).
    {
        let tx = core::sender();
        if let Err(e) = term::install(move |ev| {
            let _ = tx.send(core::AppEvent::Term(ev));
        }) {
            eprintln!("nchat-ui: could not create terminal for nchat: {e}");
        }
    }

    let window = AppWindow::new().expect("failed to create window");
    let confdir_for_instances = confdir.clone();
    core::init(
        &window,
        core::Launch { confdir, core_args, restart_args, protocols: protocols() },
    );

    // ---- UI callbacks -> state ----
    window.on_chat_selected(|key| {
        core::with_state(|s| s.select_chat(&key));
    });
    window.on_send_text(|text| {
        core::with_state(|s| s.send_text(&text));
    });
    window.on_load_older(|| {
        core::with_state(|s| s.load_older());
    });
    window.on_message_action(|id, action| {
        core::with_state(|s| s.message_action(&id, &action));
    });
    window.on_message_react(|id, emoji| {
        core::with_state(|s| s.react(&id, &emoji));
    });
    window.on_cancel_reply(|| {
        core::with_state(|s| s.cancel_reply());
    });
    window.on_search_changed(|text| {
        core::with_state(|s| s.set_search(&text));
    });
    window.on_composer_edited(|text| {
        core::with_state(|s| s.composer_edited(&text));
    });
    window.on_add_account(|| {
        core::with_state(|s| s.add_account());
    });
    window.on_toggle_theme(|| {
        core::with_state(|s| s.toggle_theme());
    });
    window.on_chat_action(|key, action| {
        core::with_state(|s| s.chat_action(&key, &action));
    });
    window.on_filter_changed(|f| {
        core::with_state(|s| s.set_filter(f));
    });
    window.on_new_chat(|| {
        core::with_state(|s| s.new_chat());
    });
    window.on_open_settings(|| {
        core::with_state(|s| s.open_settings());
    });
    window.on_open_info(|| {
        core::with_state(|s| s.open_info());
    });
    window.on_mark_all_read(|| {
        core::with_state(|s| s.mark_all_read());
    });
    window.on_find(|text, older| {
        core::with_state(|s| s.find(&text, older));
    });
    window.on_find_closed(|| {
        core::with_state(|s| s.find_closed());
    });
    window.on_jump_pinned(|| {
        core::with_state(|s| s.jump_pinned());
    });
    window.on_record_start(|| {
        core::with_state(|s| s.record_start());
    });
    window.on_record_cancel(|| {
        core::with_state(|s| s.record_cancel());
    });
    window.on_record_send(|| {
        core::with_state(|s| s.record_send());
    });
    window.on_paste_attachments(|| core::with_state(|s| s.paste_attachments()).unwrap_or(false));
    // files dragged in from a file manager (X11 windows; see use_x11_window)
    {
        use slint::winit_030::{winit::event::WindowEvent, EventResult, WinitWindowAccessor};
        window.window().on_winit_window_event(|_, ev| {
            match ev {
                WindowEvent::HoveredFile(_) => {
                    core::with_state(|s| s.drop_hover(true));
                }
                WindowEvent::HoveredFileCancelled => {
                    core::with_state(|s| s.drop_hover(false));
                }
                WindowEvent::DroppedFile(path) => {
                    let path = path.clone();
                    core::with_state(|s| s.file_dropped(path));
                }
                _ => {}
            }
            EventResult::Propagate
        });
    }
    window.on_emoji_requested(|| {
        core::with_state(|s| s.open_emoji_panel(false, None));
    });
    window.on_jumped_to_bottom(|| {
        core::with_state(|s| s.jumped_to_bottom());
    });
    {
        let em = window.global::<EmojiState>();
        em.on_tab_changed(|t| {
            core::with_state(|s| s.emoji_tab_changed(t));
        });
        em.on_search_changed(|t| {
            core::with_state(|s| s.emoji_search_changed(&t));
        });
        em.on_picked(|e| {
            core::with_state(|s| s.emoji_picked(&e));
        });
    }
    {
        let info = window.global::<InfoState>();
        info.on_close(|| {
            core::with_state(|s| s.close_info());
        });
        info.on_action(|a| {
            core::with_state(|s| s.info_action(&a));
        });
        info.on_member_clicked(|k| {
            core::with_state(|s| s.member_clicked(&k));
        });
        let picker = window.global::<PickerState>();
        picker.on_filter_changed(|f| {
            core::with_state(|s| s.picker_filter(&f));
        });
        picker.on_picked(|k| {
            core::with_state(|s| s.picker_picked(&k));
        });
        picker.on_cancel(|| {
            core::with_state(|s| s.close_picker());
        });
        let confirm = window.global::<ConfirmState>();
        confirm.on_ok(|| {
            core::with_state(|s| s.confirm_result(true));
        });
        confirm.on_cancel(|| {
            core::with_state(|s| s.confirm_result(false));
        });
        let st = window.global::<SettingsState>();
        st.on_changed(|| {
            core::with_state(|s| s.settings_changed());
        });
        st.on_reset(|| {
            core::with_state(|s| s.reset_settings());
        });
        let weak = window.as_weak();
        st.on_close(move || {
            if let Some(w) = weak.upgrade() {
                w.global::<SettingsState>().set_visible(false);
            }
        });
        st.on_add_account(|| {
            core::with_state(|s| s.add_account());
        });
        st.on_remove_account(|id| {
            core::with_state(|s| s.remove_account(&id));
        });
        st.on_open_log(|| {
            core::with_state(|s| s.open_log());
        });
        st.on_open_downloads(|| {
            core::with_state(|s| s.open_downloads());
        });
    }
    // close button: hide (background mode) or quit (asks first while history is syncing)
    window.window().on_close_requested(|| {
        core::with_state(|s| s.close_requested());
        slint::CloseRequestResponse::KeepWindowShown
    });

    window.on_attach_file(|| {
        std::thread::spawn(|| {
            if let Some(files) = rfd::FileDialog::new().set_title("Send file").pick_files() {
                let _ = slint::invoke_from_event_loop(move || {
                    core::with_state(|s| s.send_files(files));
                });
            }
        });
    });
    {
        let setup = window.global::<SetupState>();
        setup.on_choose(|id| {
            core::with_state(|s| s.setup_choose(&id));
        });
        setup.on_submit(|value| {
            core::with_state(|s| s.setup_submit(&value));
        });
        setup.on_cancel(|| {
            core::with_state(|s| s.setup_cancel());
        });
    }

    // a second start of nchat-ui shows this window
    desktop::listen_for_instances(&confdir_for_instances, || {
        let _ = slint::invoke_from_event_loop(|| {
            core::with_state(|s| s.show_window());
        });
    });

    // ---- nchat events -> GUI thread, batched ----
    std::thread::Builder::new()
        .name("nchat-events".into())
        .spawn(move || {
            while let Ok(first) = rx.recv() {
                let mut batch = vec![first];
                while let Ok(ev) = rx.try_recv() {
                    batch.push(ev);
                    if batch.len() >= 1000 {
                        break;
                    }
                }
                let _ = slint::invoke_from_event_loop(move || {
                    let started = std::time::Instant::now();
                    let n = batch.len();
                    core::with_state(|s| {
                        for ev in batch {
                            s.apply(ev);
                        }
                    });
                    core::report_slow("applying events", n, started);
                });
            }
        })
        .expect("spawn event thread");

    // ---- periodic model refresh (coalesces bursts of events) ----
    let refresh = slint::Timer::default();
    refresh.start(slint::TimerMode::Repeated, Duration::from_millis(50), || {
        let started = std::time::Instant::now();
        core::with_state(|s| s.tick());
        core::report_slow("refreshing the window", 0, started);
    });

    // ---- first run / add account -> wizard, otherwise start nchat ----
    core::with_state(|s| {
        let has_profiles = s.has_profiles();
        if let Some(id) = &remove_account {
            s.start_remove(id);
        } else if add_account || !has_profiles {
            s.show_setup_choose(!has_profiles);
        } else {
            s.start_core(false);
        }
    });

    let scripted = script.is_some();
    let _script_timer = script.map(|script| run_script(&window, script));

    // Keep running while the window is hidden (background mode); quit_event_loop() ends it.
    if start_hidden && !scripted {
        core::with_state(|s| s.ensure_tray());
    } else {
        window.show().expect("failed to show window");
    }
    slint::run_event_loop_until_quit().expect("event loop failed");
    let _ = window.hide();

    // "Add account" / "Cancel": another thread is about to exec() a fresh nchat-ui.
    while core::RESTARTING.load(std::sync::atomic::Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(100));
    }

    // Window closed: let nchat log out and clean up like it does on quit in the TUI.
    ffi::request_quit();
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let running = core::with_state(|s| s.core_running()).unwrap_or(false);
        if !running {
            std::process::exit(0);
        }
        if Instant::now() > deadline {
            // nchat is still busy (e.g. setup waiting for a QR scan): running C++ static
            // destructors under its live threads can hang, so leave without them.
            unsafe { libc::_exit(0) };
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Test hook: NCHAT_UI_SCRIPT="cmd;cmd;..." runs one command every 1.5 s, headless.
/// Commands: wait | snap:<file.png> | choose:<protocol> | submit:<text> | chat:<n> |
///           send:<text> | dark | add | cancel | quit | settings | info | newchat |
///           filter:<n> | find:<text> | set:<key>=<value> | close | file:<path>
/// NCHAT_UI_SCRIPT_AFTER_RESTART is the script for the process after a restart.
fn run_script(window: &AppWindow, script: String) -> slint::Timer {
    let mut cmds: std::collections::VecDeque<String> =
        script.split(';').map(|c| c.trim().to_string()).filter(|c| !c.is_empty()).collect();
    let weak = window.as_weak();
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::Repeated, Duration::from_millis(1500), move || {
        let Some(w) = weak.upgrade() else { return };
        let Some(cmd) = cmds.pop_front() else {
            let _ = slint::quit_event_loop();
            return;
        };
        eprintln!("nchat-ui script: {cmd}");
        let (name, arg) = cmd.split_once(':').unwrap_or((cmd.as_str(), ""));
        match name {
            "wait" => {}
            "snap" => match headless::render() {
                Some(rgba) => {
                    let file = std::fs::File::create(arg).expect("snapshot file");
                    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), headless::WIDTH, headless::HEIGHT);
                    enc.set_color(png::ColorType::Rgba);
                    enc.set_depth(png::BitDepth::Eight);
                    enc.write_header().and_then(|mut wr| wr.write_image_data(&rgba)).expect("png");
                    eprintln!("nchat-ui: snapshot saved to {arg}");
                }
                None => eprintln!("nchat-ui: snapshot failed"),
            },
            "choose" => w.global::<SetupState>().invoke_choose(arg.into()),
            "submit" => w.global::<SetupState>().invoke_submit(arg.into()),
            "cancel" => w.global::<SetupState>().invoke_cancel(),
            "chat" => {
                use slint::Model;
                if let Some(item) = arg.parse().ok().and_then(|n| w.get_chats().row_data(n)) {
                    w.invoke_chat_selected(item.key);
                }
            }
            "send" => w.invoke_send_text(arg.into()),
            "file" => {
                core::with_state(|s| s.send_files(vec![PathBuf::from(arg)]));
            }
            "dark" => w.invoke_toggle_theme(),
            "add" => w.invoke_add_account(),
            "settings" => w.invoke_open_settings(),
            "info" => w.invoke_open_info(),
            "newchat" => w.invoke_new_chat(),
            "filter" => {
                let f = arg.parse().unwrap_or(0);
                w.set_filter(f);
                w.invoke_filter_changed(f);
            }
            "emoji" => {
                core::with_state(|s| {
                    s.open_emoji_panel(false, None);
                    if !arg.is_empty() {
                        s.emoji_search_changed(arg);
                    }
                });
                if !arg.is_empty() {
                    w.global::<EmojiState>().set_search(arg.into());
                }
            }
            "pick" => {
                core::with_state(|s| s.emoji_picked(arg));
            }
            "notifytest" => {
                use slint::Model;
                if let Some(item) = w.get_chats().row_data(arg.parse().unwrap_or(0)) {
                    core::with_state(|s| s.test_notification(&item.key));
                }
            }
            "notifyclear" => {
                use slint::Model;
                if let Some(item) = w.get_chats().row_data(arg.parse().unwrap_or(0)) {
                    core::with_state(|s| s.dismiss_notification(&item.key));
                }
            }
            "scrollup" => {
                w.set_scroll_to_index(0);
                w.set_scroll_to_token(w.get_scroll_to_token() + 1);
            }
            "find" => {
                w.set_find_visible(true);
                w.set_find_text(arg.into());
                w.invoke_find(arg.into(), false);
            }
            "set" => {
                // set:<key>=<value> on the settings panel, then apply
                let st = w.global::<SettingsState>();
                if let Some((k, v)) = arg.split_once('=') {
                    match k {
                        "font_size" => st.set_font_size(v.parse().unwrap_or(14.0)),
                        "accent" => st.set_accent(v.into()),
                        "theme" => st.set_theme_mode(v.parse().unwrap_or(0)),
                        "compact" => st.set_compact(v == "1"),
                        "bubble_radius" => st.set_bubble_radius(v.parse().unwrap_or(14.0)),
                        "chat_background" => st.set_chat_background(v.into()),
                        "bubble_color" => st.set_bubble_color(v.into()),
                        "run_in_background" => st.set_run_in_background(v == "1"),
                        "start_at_login" => st.set_start_at_login(v == "1"),
                        _ => eprintln!("nchat-ui script: unknown setting {k}"),
                    }
                    st.invoke_changed();
                }
            }
            "close" => {
                w.invoke_close_emoji_panels();
                w.global::<SettingsState>().set_visible(false);
                w.global::<InfoState>().invoke_close();
                w.global::<PickerState>().invoke_cancel();
            }
            "quit" => {
                let _ = slint::quit_event_loop();
            }
            "msgaction" => {
                // msgaction:<n-th file message from the end>,<action>
                if let Some((n, action)) = arg.split_once(',') {
                    use slint::Model;
                    let n: usize = n.parse().unwrap_or(0);
                    let msgs = w.get_messages();
                    let ids: Vec<_> = (0..msgs.row_count()).rev().filter_map(|i| msgs.row_data(i)).filter(|m| m.has_file).map(|m| m.id).collect();
                    if let Some(id) = ids.get(n) {
                        w.invoke_message_action(id.clone(), action.into());
                    }
                }
            }
            "fakerec" => {
                // show the recording bar without opening the microphone
                w.set_recording(arg != "off");
                w.set_record_time("0:07".into());
                w.set_record_level(0.18);
            }
            "confirm" => w.global::<ConfirmState>().invoke_ok(),
            "incoming" => {
                core::with_state(|s| s.inject_incoming(arg, None));
            }
            "incomingimage" => {
                core::with_state(|s| s.inject_incoming("", Some(arg)));
            }
            "inject" => {
                core::with_state(|s| s.inject_demo_updates());
            }
            "closewindow" => {
                // like clicking the window's close button
                core::with_state(|s| s.close_requested());
            }
            other => eprintln!("nchat-ui script: unknown command {other}"),
        }
    });
    timer
}
