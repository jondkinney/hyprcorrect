//! Menu-bar item via `NSStatusItem`.
//!
//! The menu mirrors the Linux ksni tray: Pause/Resume, separator, "Open
//! Preferences…", separator, "Quit". Menu activations are funnelled
//! through one target/action and an id stored in each item's
//! `representedObject`, then sent on the [`TrayEvent`] channel. Pause is
//! conveyed by an icon + label swap (the item stays visible), matching
//! the Linux behaviour. [`TrayHandle::refresh`] re-reads the shared
//! `paused` flag and updates the icon / Pause-item title.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock};

use objc2::rc::Retained;
use objc2::runtime::NSObject;
use objc2::{AnyThread, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{NSImage, NSMenu, NSMenuItem, NSStatusBar, NSStatusItem};
use objc2_foundation::{MainThreadMarker, NSSize, NSString};

/// A pre-rasterized icon, ARGB32 in network (big-endian) byte order —
/// the exact type the daemon's `build_tray_pixmaps` produces, shared
/// with the Linux SNI path.
pub struct IconPixmap {
    pub width: i32,
    pub height: i32,
    pub argb: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayEvent {
    TogglePause,
    OpenPrefs,
    Quit,
}

#[derive(Debug, thiserror::Error)]
pub enum TrayError {
    #[error("could not create the macOS status item: {0}")]
    StatusItem(String),
}

/// Sender the menu target funnels activations into. Set by [`start`].
static TRAY_TX: OnceLock<Mutex<Option<Sender<TrayEvent>>>> = OnceLock::new();

fn tray_tx() -> Option<Sender<TrayEvent>> {
    TRAY_TX
        .get()
        .and_then(|m| m.lock().ok())
        .and_then(|g| g.clone())
}

/// Live status-item resources. Held in the main-thread [`MainState`],
/// so the retained AppKit handles never cross a thread boundary.
pub(crate) struct TrayResources {
    status_item: Retained<NSStatusItem>,
    pause_item: Retained<NSMenuItem>,
    active_image: Option<Retained<NSImage>>,
    paused_image: Option<Retained<NSImage>>,
    paused: Arc<AtomicBool>,
    // Kept alive so the menu items' target survives.
    _target: Retained<TrayTarget>,
}

/// Live handle; holding it keeps the menu-bar item registered.
pub struct TrayHandle {
    _private: (),
}

impl TrayHandle {
    /// Re-publish icon + Pause-item title from the shared `paused` flag.
    pub fn refresh(&self) {
        super::app::run_on_main_async(|| {
            super::with_main_state(|s| {
                if let Some(t) = s.tray.as_ref() {
                    apply_paused_state(t);
                }
            });
        });
    }
}

impl Drop for TrayHandle {
    fn drop(&mut self) {
        // Tear the status item down on exit so it leaves the menu bar.
        super::app::run_on_main_async(|| {
            super::with_main_state(|s| {
                if let Some(t) = s.tray.take() {
                    NSStatusBar::systemStatusBar().removeStatusItem(&t.status_item);
                }
            });
        });
    }
}

fn apply_paused_state(t: &TrayResources) {
    let paused = t.paused.load(Ordering::Relaxed);
    let mtm = MainThreadMarker::new().expect("tray refresh off-main");
    if let Some(button) = t.status_item.button(mtm) {
        let image = if paused {
            t.paused_image.as_ref()
        } else {
            t.active_image.as_ref()
        };
        button.setImage(image.map(|i| i.as_ref()));
        let tooltip = if paused {
            "Paused. Click the tray icon to resume."
        } else {
            "Press the trigger chord to fix the last word."
        };
        button.setToolTip(Some(&NSString::from_str(tooltip)));
    }
    t.pause_item
        .setTitle(&NSString::from_str(if paused { "Resume" } else { "Pause" }));
}

pub fn start(
    paused: Arc<AtomicBool>,
    active_icon: Vec<IconPixmap>,
    paused_icon: Vec<IconPixmap>,
) -> Result<(TrayHandle, Receiver<TrayEvent>), TrayError> {
    let (tx, rx) = mpsc::channel::<TrayEvent>();
    *TRAY_TX.get_or_init(|| Mutex::new(None)).lock().unwrap() = Some(tx);

    super::app::run_on_main_sync(
        move || -> Result<(TrayHandle, Receiver<TrayEvent>), TrayError> {
            let mtm = MainThreadMarker::new().expect("tray start off-main");

            if super::with_main_state(|s| s.tray.is_some()) {
                return Err(TrayError::StatusItem(
                    "a status item already exists for this process".into(),
                ));
            }

            let bar = NSStatusBar::systemStatusBar();
            let status_item = bar.statusItemWithLength(-1.0); // NSVariableStatusItemLength
            let button = status_item
                .button(mtm)
                .ok_or_else(|| TrayError::StatusItem("NSStatusItem.button was nil".into()))?;

            let active_image = pixmaps_to_image(&active_icon);
            let paused_image = pixmaps_to_image(&paused_icon);
            match active_image.as_ref() {
                Some(img) => button.setImage(Some(img)),
                None => button.setTitle(&NSString::from_str("hc")),
            }

            let target = TrayTarget::new(mtm);
            let menu = NSMenu::new(mtm);
            let pause_item = add_action_item(&menu, "Pause", "pause", &target, mtm);
            menu.addItem(&NSMenuItem::separatorItem(mtm));
            let _ = add_action_item(&menu, "Open Preferences…", "prefs", &target, mtm);
            menu.addItem(&NSMenuItem::separatorItem(mtm));
            let _ = add_action_item(&menu, "Quit", "quit", &target, mtm);
            status_item.setMenu(Some(&menu));
            status_item.setVisible(true);

            let resources = TrayResources {
                status_item,
                pause_item,
                active_image,
                paused_image,
                paused,
                _target: target,
            };
            apply_paused_state(&resources);
            super::with_main_state(|s| s.tray = Some(resources));

            Ok((TrayHandle { _private: () }, rx))
        },
    )
}

fn add_action_item(
    menu: &NSMenu,
    label: &str,
    id: &str,
    target: &TrayTarget,
    mtm: MainThreadMarker,
) -> Retained<NSMenuItem> {
    unsafe {
        let item = NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str(label),
            Some(sel!(onMenuItem:)),
            &NSString::from_str(""),
        );
        item.setTarget(Some(target));
        item.setRepresentedObject(Some(&NSString::from_str(id)));
        menu.addItem(&item);
        item
    }
}

/// Convert the daemon's ARGB32-BE pixmaps (largest first) into a colored
/// `NSImage` for the menu bar. ARGB-network bytes `[A,R,G,B]` are
/// swizzled to straight RGBA so the `CGImageAlphaInfo::Last` layout is
/// correct.
fn pixmaps_to_image(pixmaps: &[IconPixmap]) -> Option<Retained<NSImage>> {
    use objc2_core_foundation::CFData;
    use objc2_core_graphics::{
        CGBitmapInfo, CGColorRenderingIntent, CGColorSpace, CGDataProvider, CGImage,
        CGImageAlphaInfo,
    };

    let pm = pixmaps
        .iter()
        .max_by_key(|p| p.width.max(0) * p.height.max(0))?;
    if pm.width <= 0 || pm.height <= 0 {
        return None;
    }
    let (w, h) = (pm.width as usize, pm.height as usize);
    if pm.argb.len() != w * h * 4 {
        return None;
    }
    let mut rgba = Vec::with_capacity(pm.argb.len());
    for px in pm.argb.chunks_exact(4) {
        // [A,R,G,B] → [R,G,B,A]
        rgba.extend_from_slice(&[px[1], px[2], px[3], px[0]]);
    }

    let data = unsafe { CFData::new(None, rgba.as_ptr(), rgba.len() as isize) }?;
    let provider = CGDataProvider::with_cf_data(Some(&data))?;
    let colorspace = CGColorSpace::new_device_rgb()?;
    // The daemon's pixmaps come from tiny-skia, whose RGBA is
    // premultiplied — so the CGImage must declare PremultipliedLast, not
    // Last, or anti-aliased edges render slightly dark.
    let bitmap_info = CGBitmapInfo(CGImageAlphaInfo::PremultipliedLast.0);
    let cg = unsafe {
        CGImage::new(
            w,
            h,
            8,
            32,
            w * 4,
            Some(&colorspace),
            bitmap_info,
            Some(&provider),
            std::ptr::null(),
            false,
            CGColorRenderingIntent::RenderingIntentDefault,
        )
    }?;

    // 18 logical points high in the menu bar; AppKit scales the backing.
    let ns_size = NSSize {
        width: 18.0,
        height: 18.0,
    };
    Some(NSImage::initWithCGImage_size(
        NSImage::alloc(),
        &cg,
        ns_size,
    ))
}

// --- Target/action delegate -------------------------------------------------

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "HyprcorrectTrayTarget"]
    pub(crate) struct TrayTarget;

    impl TrayTarget {
        #[unsafe(method(onMenuItem:))]
        fn on_menu_item(&self, sender: Option<&NSMenuItem>) {
            let Some(item) = sender else { return };
            let Some(obj) = item.representedObject() else {
                return;
            };
            let Ok(id) = obj.downcast::<NSString>() else {
                return;
            };
            let event = match id.to_string().as_str() {
                "pause" => TrayEvent::TogglePause,
                "prefs" => TrayEvent::OpenPrefs,
                "quit" => TrayEvent::Quit,
                _ => return,
            };
            if let Some(tx) = tray_tx() {
                let _ = tx.send(event);
            }
        }
    }
);

impl TrayTarget {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        unsafe { msg_send![mtm.alloc::<Self>(), init] }
    }
}
