//! Running-app metadata: parse `.desktop` files, resolve icons,
//! cache textures for the prefs Privacy picker.
//!
//! The picker stores window-class strings (the on-disk blocklist's
//! identifier); the registry maps those to display names + icons
//! pulled from the system's freedesktop application database, so
//! the user sees "Chromium" with a logo instead of
//! `chrome-discord.com_channels_@me-Default`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use eframe::egui;

const ICON_SIZE_PX: u32 = 48;

/// Resolved metadata for a single app: what to show in the picker
/// and what to store in the config blocklist.
#[derive(Clone)]
pub struct AppMeta {
    /// The window class (or whatever identifier the platform uses) —
    /// the actual blocklist key.
    pub identifier: String,
    /// Friendly display name; falls back to the identifier when no
    /// `.desktop` entry matches.
    pub display_name: String,
    /// 48×48 RGBA texture for the icon, when one was resolvable.
    /// Built lazily by [`AppRegistry::lookup`].
    pub icon: Option<egui::TextureHandle>,
}

impl std::fmt::Debug for AppMeta {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // TextureHandle isn't Debug; we don't need to print it anyway.
        f.debug_struct("AppMeta")
            .field("identifier", &self.identifier)
            .field("display_name", &self.display_name)
            .field("icon", &self.icon.is_some())
            .finish()
    }
}

/// One parsed `[Desktop Entry]` block — only the fields we care
/// about for the picker.
#[derive(Debug, Clone, Default)]
struct DesktopEntry {
    name: Option<String>,
    icon: Option<String>,
    wm_class: Option<String>,
    /// `.desktop` filename stem (e.g. `firefox.desktop` → `firefox`).
    stem: String,
    no_display: bool,
}

/// Pre-parsed system app database, plus an on-the-fly icon cache.
pub struct AppRegistry {
    /// Indexed by lowercase identifier — both the `StartupWMClass`
    /// and the filename stem land here so a hyprctl class like
    /// `firefox` matches a `firefox.desktop` whether or not it
    /// declares `StartupWMClass=firefox`.
    by_identifier: HashMap<String, Arc<DesktopEntry>>,
    /// Cached icon textures, keyed by lowercase identifier. `None`
    /// means "we tried and didn't find one" — avoids retrying every
    /// frame.
    icon_cache: HashMap<String, Option<egui::TextureHandle>>,
    /// Standard freedesktop icon directories, resolved at startup.
    icon_dirs: Vec<PathBuf>,
}

impl AppRegistry {
    /// Build the registry by walking the standard application dirs.
    /// Idempotent; safe to call again to pick up newly-installed
    /// apps (we re-read every time).
    pub fn discover() -> Self {
        let mut by_identifier: HashMap<String, Arc<DesktopEntry>> = HashMap::new();
        for dir in desktop_dirs() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("desktop") {
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let Some(parsed) = parse_desktop(&text, &path) else {
                    continue;
                };
                if parsed.no_display {
                    continue;
                }
                let arc = Arc::new(parsed);
                if let Some(wm) = &arc.wm_class {
                    by_identifier
                        .entry(wm.to_ascii_lowercase())
                        .or_insert_with(|| arc.clone());
                }
                by_identifier
                    .entry(arc.stem.to_ascii_lowercase())
                    .or_insert(arc);
            }
        }
        Self {
            by_identifier,
            icon_cache: HashMap::new(),
            icon_dirs: icon_dirs(),
        }
    }

    /// Look up display name + icon for the given identifier. Always
    /// returns something — falls back to the identifier itself when
    /// no `.desktop` entry matches.
    pub fn lookup(&mut self, ctx: &egui::Context, identifier: &str) -> AppMeta {
        let key = identifier.to_ascii_lowercase();
        let entry = self.by_identifier.get(&key).cloned();
        let display_name = entry
            .as_ref()
            .and_then(|e| e.name.clone())
            .unwrap_or_else(|| identifier.to_string());
        let icon = self.icon_for(ctx, &key, entry.as_deref());
        AppMeta {
            identifier: identifier.to_string(),
            display_name,
            icon,
        }
    }

    fn icon_for(
        &mut self,
        ctx: &egui::Context,
        key: &str,
        entry: Option<&DesktopEntry>,
    ) -> Option<egui::TextureHandle> {
        if let Some(slot) = self.icon_cache.get(key) {
            return slot.clone();
        }
        let icon_name = entry.and_then(|e| e.icon.clone()).unwrap_or_default();
        let texture = if icon_name.is_empty() {
            None
        } else {
            resolve_icon_path(&icon_name, &self.icon_dirs).and_then(|p| load_icon(ctx, &p, key))
        };
        self.icon_cache.insert(key.to_string(), texture.clone());
        texture
    }
}

fn desktop_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![
        PathBuf::from("/usr/share/applications"),
        PathBuf::from("/usr/local/share/applications"),
    ];
    if let Some(home) = std::env::var_os("HOME") {
        let mut p = PathBuf::from(home);
        p.push(".local/share/applications");
        dirs.push(p);
    }
    // Flatpak user + system app entries
    if let Some(home) = std::env::var_os("HOME") {
        let mut p = PathBuf::from(home);
        p.push(".local/share/flatpak/exports/share/applications");
        dirs.push(p);
    }
    dirs.push(PathBuf::from("/var/lib/flatpak/exports/share/applications"));
    dirs
}

fn icon_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        let mut p = PathBuf::from(&home);
        p.push(".icons");
        dirs.push(p);
        let mut p = PathBuf::from(&home);
        p.push(".local/share/icons");
        dirs.push(p);
        let mut p = PathBuf::from(&home);
        p.push(".local/share/flatpak/exports/share/icons");
        dirs.push(p);
    }
    dirs.push(PathBuf::from("/usr/share/icons"));
    dirs.push(PathBuf::from("/usr/local/share/icons"));
    dirs.push(PathBuf::from("/var/lib/flatpak/exports/share/icons"));
    dirs
}

fn parse_desktop(text: &str, path: &Path) -> Option<DesktopEntry> {
    let stem = path.file_stem().and_then(|s| s.to_str())?.to_string();
    let mut entry = DesktopEntry {
        stem,
        ..Default::default()
    };
    let mut in_main = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_main = line == "[Desktop Entry]";
            continue;
        }
        if !in_main || line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        match key {
            "Name" if entry.name.is_none() => entry.name = Some(value.to_string()),
            "Icon" => entry.icon = Some(value.to_string()),
            "StartupWMClass" => entry.wm_class = Some(value.to_string()),
            "NoDisplay" if value == "true" => entry.no_display = true,
            _ => {}
        }
    }
    Some(entry)
}

/// Resolve an `Icon=` value against the freedesktop icon search
/// path. Absolute paths are used verbatim; bare names are looked up
/// across known theme directories at common sizes.
fn resolve_icon_path(icon: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
    let trimmed = icon.trim();
    if trimmed.is_empty() {
        return None;
    }
    let candidate = Path::new(trimmed);
    if candidate.is_absolute() && candidate.exists() {
        return Some(candidate.to_path_buf());
    }
    let themes = ["hicolor", "Adwaita", "breeze", "Papirus", "gnome", "Yaru"];
    let sizes = [
        "scalable", "256x256", "128x128", "96x96", "64x64", "48x48", "256", "128", "64", "48",
        "symbolic",
    ];
    let exts = ["svg", "png"];
    for dir in dirs {
        for theme in &themes {
            for size in &sizes {
                for ext in &exts {
                    let mut p = dir.clone();
                    p.push(theme);
                    p.push(size);
                    p.push("apps");
                    p.push(format!("{trimmed}.{ext}"));
                    if p.exists() {
                        return Some(p);
                    }
                }
            }
        }
        // `/usr/share/pixmaps/<icon>.{png,svg,xpm}`
        for ext in &exts {
            let mut p = PathBuf::from("/usr/share/pixmaps");
            p.push(format!("{trimmed}.{ext}"));
            if p.exists() {
                return Some(p);
            }
        }
    }
    None
}

fn load_icon(ctx: &egui::Context, path: &Path, cache_key: &str) -> Option<egui::TextureHandle> {
    let bytes = std::fs::read(path).ok()?;
    let rgba = match path.extension().and_then(|e| e.to_str()) {
        Some("svg") => rasterize_svg(&bytes, ICON_SIZE_PX),
        Some("png") => rasterize_png(&bytes, ICON_SIZE_PX),
        _ => None,
    }?;
    let image = egui::ColorImage::from_rgba_unmultiplied(
        [ICON_SIZE_PX as usize, ICON_SIZE_PX as usize],
        &rgba,
    );
    Some(ctx.load_texture(
        format!("app-icon:{cache_key}"),
        image,
        egui::TextureOptions::LINEAR,
    ))
}

fn rasterize_svg(bytes: &[u8], size: u32) -> Option<Vec<u8>> {
    let opts = usvg::Options::default();
    let tree = usvg::Tree::from_data(bytes, &opts).ok()?;
    let mut pixmap = tiny_skia::Pixmap::new(size, size)?;
    let svg_size = tree.size();
    let scale = (size as f32 / svg_size.width()).min(size as f32 / svg_size.height());
    let dx = (size as f32 - svg_size.width() * scale) / 2.0;
    let dy = (size as f32 - svg_size.height() * scale) / 2.0;
    let transform = tiny_skia::Transform::from_scale(scale, scale).post_translate(dx, dy);
    resvg::render(&tree, transform, &mut pixmap.as_mut());
    Some(pixmap.take())
}

fn rasterize_png(bytes: &[u8], size: u32) -> Option<Vec<u8>> {
    let img = image::load_from_memory_with_format(bytes, image::ImageFormat::Png).ok()?;
    let scaled = img.resize(size, size, image::imageops::FilterType::Lanczos3);
    let rgba = scaled.to_rgba8();
    // The resized image may be slightly smaller than `size`x`size`
    // (preserved aspect); center it on a transparent canvas.
    let (w, h) = (rgba.width(), rgba.height());
    if w == size && h == size {
        return Some(rgba.into_raw());
    }
    let mut canvas: Vec<u8> = vec![0u8; (size * size * 4) as usize];
    let dx = (size - w) / 2;
    let dy = (size - h) / 2;
    for y in 0..h {
        let dst_row = (dy + y) as usize * size as usize * 4;
        let src_row = y as usize * w as usize * 4;
        let dst_off = dst_row + dx as usize * 4;
        let len = w as usize * 4;
        canvas[dst_off..dst_off + len].copy_from_slice(&rgba.as_raw()[src_row..src_row + len]);
    }
    Some(canvas)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_desktop_picks_main_section_only() {
        let text = "\
[Desktop Action New]
Name=Wrong

[Desktop Entry]
Name=Discord
Icon=discord
StartupWMClass=discord
Type=Application
NoDisplay=false
";
        let entry = parse_desktop(text, Path::new("/x/discord.desktop")).unwrap();
        assert_eq!(entry.name.as_deref(), Some("Discord"));
        assert_eq!(entry.icon.as_deref(), Some("discord"));
        assert_eq!(entry.wm_class.as_deref(), Some("discord"));
        assert_eq!(entry.stem, "discord");
        assert!(!entry.no_display);
    }

    #[test]
    fn parse_desktop_honors_nodisplay() {
        let text = "[Desktop Entry]\nName=Hidden\nNoDisplay=true\n";
        let entry = parse_desktop(text, Path::new("/x/hidden.desktop")).unwrap();
        assert!(entry.no_display);
    }
}
