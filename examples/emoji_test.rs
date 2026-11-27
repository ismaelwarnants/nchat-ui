//! Renders a set of emoji sequences in a real window and saves a screenshot after it has
//! been drawn (dev tool: cargo run --release --example emoji_test -- out.png).
slint::slint! {
    export component T inherits Window {
        width: 900px; height: 300px; background: white;
        VerticalLayout { padding: 16px; spacing: 8px;
            Text { color: black; font-size: 26px; text: "basic: 😀 👍 ❤️ 🎉 ✍️ ✍ ☕"; }
            Text { color: black; font-size: 26px; text: "keycaps: 3️⃣ #️⃣ 1️⃣0️⃣  flags: 🇪🇸 🇧🇪 🇳🇱"; }
            Text { color: black; font-size: 26px; text: "skin tones: 👍🏽 👋🏿 🙏🏻  ZWJ: 👨‍👩‍👧 🧑‍💻 🏳️‍🌈 🚴🏿‍♂️"; }
            Text { color: black; font-size: 26px; text: "new: 🫠 🫶 🪿 🫎 🙂‍↕️ 🐦‍🔥"; }
            Text { color: black; font-size: 26px; text: "ADDIM - 3️⃣✍️ core 3"; }
        }
    }
}
fn main() {
    let out = std::env::args().nth(1).unwrap_or("emoji.png".into());
    let t = T::new().unwrap();
    let w = t.as_weak();
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::SingleShot, std::time::Duration::from_millis(1200), move || {
        let t = w.upgrade().unwrap();
        let buf = t.window().take_snapshot().unwrap();
        let f = std::fs::File::create(&out).unwrap();
        let mut e = png::Encoder::new(std::io::BufWriter::new(f), buf.width(), buf.height());
        e.set_color(png::ColorType::Rgba); e.set_depth(png::BitDepth::Eight);
        e.write_header().unwrap().write_image_data(buf.as_bytes()).unwrap();
        let _ = slint::quit_event_loop();
    });
    t.run().unwrap();
}
