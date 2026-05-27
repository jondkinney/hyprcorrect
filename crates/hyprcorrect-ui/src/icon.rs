//! Hyprcorrect's app icon, rasterized from a bundled SVG.
//!
//! The SVG includes a `<text>` element so it relies on usvg's font
//! database — populated from the host's system fonts at first use.
//! A static `OnceLock` keeps that one-time discovery off the prefs
//! window's hot path.

use std::sync::OnceLock;

const APP_ICON_SVG: &[u8] = include_bytes!("../assets/icons/svg/hyprcorrect.svg");
/// Brand blue from the bundled SVG. Recolor uses this as the search
/// token so the tray can swap to white without a second asset file.
const BRAND_FILL: &str = "#4a86c0";
/// Color the tray icon uses. Matches the white system-status glyphs
/// on dark Waybar bars (Bluetooth, Wi-Fi, sound, battery). Vernier
/// uses the same convention for its `*-symbolic` tray asset.
const TRAY_FILL: &str = "#ffffff";
/// Single pixmap size we publish to the SNI tray. Mirrors Vernier's
/// approach: one large pixmap lets the SNI host downscale crisply
/// for whatever slot size the user's bar uses, rather than us
/// guessing 22/44 and possibly missing the bar's actual slot.
const TRAY_PIXMAP_SIZE: u32 = 64;

/// Raw bytes of the bundled SVG. Used by the autostart writer to
/// drop a copy at a known path so the generated `.desktop` can
/// reference our icon with an absolute path — Walker / other XDG
/// launchers then show our brand instead of the system theme's
/// `tools-check-spelling` fallback.
pub fn app_icon_svg_bytes() -> &'static [u8] {
    APP_ICON_SVG
}

/// One pixmap for the SNI tray. ARGB32 in network (big-endian) byte
/// order — on a little-endian CPU that means bytes laid out as
/// A, R, G, B per pixel. `paused` halves the alpha channel so the
/// tray icon dims to "I'm here but not listening" without needing
/// a second SVG asset.
pub struct TrayPixmap {
    pub size: u32,
    pub argb: Vec<u8>,
}

/// Rasterize the app icon for the SNI tray as a single large
/// pixmap. The bundled SVG's brand blue is swapped for white at
/// render time so the mark matches the system-status convention
/// most Wayland bars (Bluetooth, Wi-Fi, sound) follow on dark
/// surfaces.
///
/// Returns a `Vec<TrayPixmap>` so the platform layer's existing
/// shape (multiple pixmaps to choose from) stays a one-liner;
/// here it just has a single entry.
pub fn tray_pixmaps(_sizes: &[u32], paused: bool) -> Vec<TrayPixmap> {
    let rgba = render_recolored_rgba(TRAY_PIXMAP_SIZE, TRAY_FILL);
    let argb = rgba_to_argb_with_alpha(&rgba, paused);
    vec![TrayPixmap {
        size: TRAY_PIXMAP_SIZE,
        argb,
    }]
}

/// Render the bundled SVG with [`BRAND_FILL`] swapped for `fill`.
/// Used by [`tray_pixmaps`] to produce a monochrome variant from
/// the one source asset; falls back to the unmodified blue if the
/// SVG bytes aren't valid UTF-8 (they always are, but the guard
/// keeps us from blowing up at render time).
fn render_recolored_rgba(size: u32, fill: &str) -> Vec<u8> {
    let Ok(text) = std::str::from_utf8(APP_ICON_SVG) else {
        return render_app_icon_rgba(size);
    };
    let recolored = text.replace(BRAND_FILL, fill);
    render_svg_bytes_rgba(recolored.as_bytes(), size)
}

/// Convert RGBA8 → ARGB32 big-endian (network byte order). If
/// `paused`, halve each pixel's alpha — gives the tray a muted
/// look without requiring a second SVG asset.
fn rgba_to_argb_with_alpha(rgba: &[u8], paused: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(rgba.len());
    for chunk in rgba.chunks_exact(4) {
        let [r, g, b, a] = [chunk[0], chunk[1], chunk[2], chunk[3]];
        let a = if paused { a / 2 } else { a };
        out.extend_from_slice(&[a, r, g, b]);
    }
    out
}

/// Render the app icon to an RGBA8 buffer of `size`×`size` pixels.
/// Returns an all-transparent buffer if the SVG fails to parse — the
/// prefs sidebar gracefully falls back to the bare heading.
pub fn render_app_icon_rgba(size: u32) -> Vec<u8> {
    render_svg_bytes_rgba(APP_ICON_SVG, size)
}

/// Shared rasterizer used by both the brand-color and recolored
/// (tray) paths. Returns an all-transparent buffer on parse failure
/// so callers degrade gracefully.
fn render_svg_bytes_rgba(svg: &[u8], size: u32) -> Vec<u8> {
    let opts = usvg::Options {
        fontdb: fontdb().clone(),
        ..usvg::Options::default()
    };
    let Ok(tree) = usvg::Tree::from_data(svg, &opts) else {
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
