//! Hyprcorrect's app icon, rasterized from a bundled SVG.
//!
//! The SVG includes a `<text>` element so it relies on usvg's font
//! database — populated from the host's system fonts at first use.
//! A static `OnceLock` keeps that one-time discovery off the prefs
//! window's hot path.

use std::sync::OnceLock;

const APP_ICON_SVG: &[u8] = include_bytes!("../assets/icons/svg/hyprcorrect.svg");

/// Render the app icon to an RGBA8 buffer of `size`×`size` pixels.
/// Returns an all-transparent buffer if the SVG fails to parse — the
/// prefs sidebar gracefully falls back to the bare heading.
pub fn render_app_icon_rgba(size: u32) -> Vec<u8> {
    let opts = usvg::Options {
        fontdb: fontdb().clone(),
        ..usvg::Options::default()
    };
    let Ok(tree) = usvg::Tree::from_data(APP_ICON_SVG, &opts) else {
        return vec![0; (size as usize) * (size as usize) * 4];
    };
    let mut pixmap = tiny_skia::Pixmap::new(size, size)
        .unwrap_or_else(|| tiny_skia::Pixmap::new(1, 1).expect("1x1 pixmap"));
    let svg_size = tree.size();
    let scale_x = size as f32 / svg_size.width();
    let scale_y = size as f32 / svg_size.height();
    let scale = scale_x.min(scale_y);
    let transform = tiny_skia::Transform::from_scale(scale, scale);
    resvg::render(&tree, transform, &mut pixmap.as_mut());
    pixmap.take()
}

fn fontdb() -> &'static std::sync::Arc<usvg::fontdb::Database> {
    static DB: OnceLock<std::sync::Arc<usvg::fontdb::Database>> = OnceLock::new();
    DB.get_or_init(|| {
        let mut db = usvg::fontdb::Database::new();
        db.load_system_fonts();
        std::sync::Arc::new(db)
    })
}
