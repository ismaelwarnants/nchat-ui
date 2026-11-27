//! Renders ui/icon.svg to PNGs: cargo run --release --example render_icon -- <out dir> [sizes...]
//! (used by scripts/package.sh for the hicolor icon theme sizes)
fn main() {
    let mut args = std::env::args().skip(1);
    let out = std::path::PathBuf::from(args.next().unwrap_or_else(|| "icons".into()));
    let mut sizes: Vec<u32> = args.filter_map(|a| a.parse().ok()).collect();
    if sizes.is_empty() {
        sizes = vec![16, 22, 24, 32, 48, 64, 128, 256, 512];
    }
    let svg = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/ui/icon.svg")).expect("icon.svg");
    let tree = resvg::usvg::Tree::from_data(&svg, &resvg::usvg::Options::default()).expect("parse svg");
    std::fs::create_dir_all(&out).unwrap();
    for size in sizes {
        let mut pixmap = resvg::tiny_skia::Pixmap::new(size, size).unwrap();
        let scale = size as f32 / tree.size().width();
        resvg::render(&tree, resvg::tiny_skia::Transform::from_scale(scale, scale), &mut pixmap.as_mut());
        let path = out.join(format!("{size}x{size}.png"));
        pixmap.save_png(&path).expect("save png");
        println!("{}", path.display());
    }
}
