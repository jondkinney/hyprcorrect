//! Running-application enumeration + icon extraction for the prefs
//! Privacy picker — the macOS analog of Linux's `hyprctl clients` +
//! `.desktop` scan.
//!
//! Enumeration (`list_running_apps`) is cheap: bundle id, display name,
//! and bundle path for every *regular* (user-facing) running app, so it
//! can run on every picker refresh. Icon rasterization
//! (`app_icon_rgba`) is separate and lazy — the UI resolves an icon only
//! when it actually shows that row, exactly like the Linux side loads
//! `.desktop` icons on demand.
//!
//! Both run on the main thread (AppKit drawing requires it).
//! `run_on_main_sync` short-circuits to an inline call when already on
//! main, so this works inside the eframe prefs process (which has no
//! `bootstrap_main`, but does run its UI on the main thread).

use std::path::Path;

use objc2::AnyThread;
use objc2_app_kit::{
    NSApplicationActivationPolicy, NSBitmapFormat, NSBitmapImageRep, NSCalibratedRGBColorSpace,
    NSCompositingOperation, NSGraphicsContext, NSImage, NSWorkspace,
};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};

/// A running app for the blocklist picker. `bundle_id` is the blocklist
/// key (matched against the focus backend's app-level `class`); `name`
/// is the friendly label; `bundle_path` lets the UI lazily extract the
/// icon.
pub struct RunningApp {
    pub bundle_id: String,
    pub name: String,
    pub bundle_path: Option<String>,
}

/// Enumerate running apps with a *regular* activation policy — the ones
/// a user recognises (Safari, Terminal, …), skipping the swarm of
/// background agents/daemons. Deduplicated by bundle id and sorted by
/// name. No icon work here, so it's cheap enough to call on refresh.
pub fn list_running_apps() -> Vec<RunningApp> {
    super::app::run_on_main_sync(|| {
        let workspace = NSWorkspace::sharedWorkspace();
        let apps = workspace.runningApplications();
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for app in apps.iter() {
            if app.activationPolicy() != NSApplicationActivationPolicy::Regular {
                continue;
            }
            let Some(bundle_id) = app.bundleIdentifier().map(|s| s.to_string()) else {
                continue;
            };
            if !seen.insert(bundle_id.clone()) {
                continue; // a second window/instance of an app already listed
            }
            let name = app
                .localizedName()
                .map(|s| s.to_string())
                .unwrap_or_else(|| bundle_id.clone());
            let bundle_path = app
                .bundleURL()
                .and_then(|u| u.path())
                .map(|s| s.to_string());
            out.push(RunningApp {
                bundle_id,
                name,
                bundle_path,
            });
        }
        out.sort_by_key(|a| a.name.to_lowercase());
        out
    })
}

/// Rasterize the `.app` bundle's icon at `bundle_path` to `size × size`
/// RGBA8 **non-premultiplied** bytes (egui's
/// `ColorImage::from_rgba_unmultiplied` layout). `None` on any failure
/// — the picker just shows the row without an icon.
///
/// Uses `NSWorkspace.iconForFile`, Apple's canonical icon resolver, so
/// `.icns`, compiled asset catalogs, and custom icons all work through
/// one path. Adapted from the sibling `vernier` project.
pub fn app_icon_rgba(bundle_path: &str, size: u32) -> Option<Vec<u8>> {
    let bundle_path = bundle_path.to_string();
    super::app::run_on_main_sync(move || icon_rgba_main(Path::new(&bundle_path), size))
}

/// Rasterize the icon of the app with `bundle_id`, resolving its `.app`
/// through LaunchServices — so it works for any *installed* app, whether
/// or not it's currently running and regardless of its activation policy
/// (background agents like 1Password don't appear in `list_running_apps`
/// but still resolve here). `None` if the bundle isn't installed or the
/// icon can't be rendered.
#[allow(deprecated)] // URLForApplicationWithBundleIdentifier: sync form
pub fn icon_rgba_for_bundle_id(bundle_id: &str, size: u32) -> Option<Vec<u8>> {
    let bundle_id = bundle_id.to_string();
    super::app::run_on_main_sync(move || {
        let workspace = NSWorkspace::sharedWorkspace();
        let bid = NSString::from_str(&bundle_id);
        let url = workspace.URLForApplicationWithBundleIdentifier(&bid)?;
        let path = url.path()?;
        icon_rgba_main(Path::new(&path.to_string()), size)
    })
}

fn icon_rgba_main(bundle_path: &Path, size: u32) -> Option<Vec<u8>> {
    let workspace = NSWorkspace::sharedWorkspace();
    let ns_path = NSString::from_str(&bundle_path.to_string_lossy());
    let icon: objc2::rc::Retained<NSImage> = workspace.iconForFile(&ns_path);
    let target = NSSize {
        width: size as f64,
        height: size as f64,
    };
    icon.setSize(target);

    // RGBA8, no row padding, premultiplied (the only format a graphics
    // context accepts as a drawing destination); we demultiply on read.
    let bitmap = unsafe {
        NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bitmapFormat_bytesPerRow_bitsPerPixel(
            NSBitmapImageRep::alloc(),
            std::ptr::null_mut(),
            size as isize,
            size as isize,
            8,
            4,
            true,
            false,
            NSCalibratedRGBColorSpace,
            NSBitmapFormat(0),
            (size * 4) as isize,
            32,
        )?
    };
    let ctx = NSGraphicsContext::graphicsContextWithBitmapImageRep(&bitmap)?;
    NSGraphicsContext::saveGraphicsState_class();
    NSGraphicsContext::setCurrentContext(Some(&ctx));
    let rect = NSRect {
        origin: NSPoint { x: 0.0, y: 0.0 },
        size: target,
    };
    let zero = NSRect {
        origin: NSPoint { x: 0.0, y: 0.0 },
        size: NSSize {
            width: 0.0,
            height: 0.0,
        },
    };
    icon.drawInRect_fromRect_operation_fraction(rect, zero, NSCompositingOperation::Copy, 1.0);
    ctx.flushGraphics();
    NSGraphicsContext::restoreGraphicsState_class();

    let row_bytes = bitmap.bytesPerRow() as usize;
    let expected_row = (size as usize) * 4;
    if row_bytes != expected_row {
        return None;
    }
    let total = expected_row * (size as usize);
    let data_ptr = bitmap.bitmapData();
    if data_ptr.is_null() {
        return None;
    }
    let mut bytes = unsafe { std::slice::from_raw_parts(data_ptr, total) }.to_vec();
    // Demultiply for egui's unpremultiplied convention.
    for px in bytes.chunks_exact_mut(4) {
        let a = px[3];
        if a == 0 {
            continue;
        }
        let inv = 255.0 / a as f32;
        px[0] = ((px[0] as f32 * inv).round() as u32).min(255) as u8;
        px[1] = ((px[1] as f32 * inv).round() as u32).min(255) as u8;
        px[2] = ((px[2] as f32 * inv).round() as u32).min(255) as u8;
    }
    Some(bytes)
}
