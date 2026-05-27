//! System tray (ksni-based) for the hyprcorrect daemon.
//!
//! Publishes a StatusNotifierItem with a small menu: Pause/Resume,
//! Open Preferences…, Quit. Menu activations arrive on the returned
//! channel. The pause state is shared with the daemon via an
//! `Arc<AtomicBool>` — the tray reads it live to choose its icon,
//! label, and SNI status — and [`TrayHandle::refresh`] pushes a
//! property-change so SNI hosts pick up the new state immediately.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};

/// A menu event from the tray.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayEvent {
    /// The user picked "Pause" or "Resume" — toggle the pause flag.
    TogglePause,
    /// The user picked "Open Preferences…".
    OpenPrefs,
    /// The user picked "Quit".
    Quit,
}

/// An error starting the tray.
#[derive(Debug, thiserror::Error)]
pub enum TrayError {
    /// The ksni service (D-Bus / SNI publication) could not start.
    #[error("ksni: {0}")]
    Ksni(String),
}

/// A live handle to the running tray. Holding it keeps the SNI
/// service registered; dropping it tears it down.
pub struct TrayHandle {
    inner: ksni::blocking::Handle<HyprcorrectTray>,
}

impl TrayHandle {
    /// Re-publish the tray's properties so SNI hosts pick up changes
    /// to pause state immediately. Cheap: the closure is a no-op —
    /// state lives in the shared `Arc<AtomicBool>`.
    pub fn refresh(&self) {
        self.inner.update(|_| {});
    }
}

/// A pre-rasterized icon pixmap to publish via SNI. The data is
/// ARGB32 in network (big-endian) byte order — that's the format
/// the StatusNotifierItem spec requires.
pub struct IconPixmap {
    pub width: i32,
    pub height: i32,
    pub argb: Vec<u8>,
}

/// Start the tray. Returns a [`TrayHandle`] (the caller must hold
/// it for the life of the daemon) and a receiver of menu activations.
///
/// `paused` is the shared pause flag; the tray reads it live and
/// switches between `active_icon` and `paused_icon` to reflect it.
/// Each icon is a list of pre-rasterized pixmaps the platform layer
/// publishes as-is; the caller (the daemon) owns rasterization so
/// this crate doesn't have to drag in resvg/tiny-skia.
///
/// # Errors
///
/// See [`TrayError`].
pub fn start(
    paused: Arc<AtomicBool>,
    active_icon: Vec<IconPixmap>,
    paused_icon: Vec<IconPixmap>,
) -> Result<(TrayHandle, Receiver<TrayEvent>), TrayError> {
    use ksni::blocking::TrayMethods;

    let (events_tx, events_rx) = mpsc::channel();
    let tray = HyprcorrectTray {
        events_tx,
        paused,
        active_icon,
        paused_icon,
    };
    let inner = tray.spawn().map_err(|e| TrayError::Ksni(e.to_string()))?;
    Ok((TrayHandle { inner }, events_rx))
}

struct HyprcorrectTray {
    events_tx: Sender<TrayEvent>,
    paused: Arc<AtomicBool>,
    active_icon: Vec<IconPixmap>,
    paused_icon: Vec<IconPixmap>,
}

impl HyprcorrectTray {
    fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }
}

impl ksni::Tray for HyprcorrectTray {
    fn id(&self) -> String {
        "hyprcorrect".to_string()
    }

    fn title(&self) -> String {
        "hyprcorrect".to_string()
    }

    fn category(&self) -> ksni::Category {
        ksni::Category::ApplicationStatus
    }

    fn status(&self) -> ksni::Status {
        // Stay `Active` even while paused — many SNI hosts (Waybar,
        // for example) hide `Passive` items entirely, which would make
        // the menu unreachable. Pause is conveyed by the icon swap.
        ksni::Status::Active
    }

    fn icon_name(&self) -> String {
        // Empty string forces SNI hosts to skip the icon-theme
        // lookup and use [`icon_pixmap`] below — themed-name
        // resolution is inconsistent across hosts (waybar can pick
        // a small pre-rasterized PNG variant from the theme even
        // when we publish a pixmap, and that variant is often a
        // different drawing). Mirrors vernier's tray approach.
        String::new()
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        // Publish the daemon-rasterized bundled icon directly so we
        // don't depend on the user's icon theme. SNI hosts downscale
        // a single large pixmap (64×64) crisply to whatever bar
        // slot they draw.
        let src = if self.is_paused() {
            &self.paused_icon
        } else {
            &self.active_icon
        };
        src.iter()
            .map(|p| ksni::Icon {
                width: p.width,
                height: p.height,
                data: p.argb.clone(),
            })
            .collect()
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        let description = if self.is_paused() {
            "Paused. Click the tray icon to resume.".to_string()
        } else {
            "Press the trigger chord to fix the last word.".to_string()
        };
        ksni::ToolTip {
            title: "hyprcorrect".into(),
            description,
            icon_name: String::new(),
            icon_pixmap: Vec::new(),
        }
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::MenuItem;
        use ksni::menu::StandardItem;
        let pause_label = if self.is_paused() { "Resume" } else { "Pause" };
        vec![
            StandardItem {
                label: pause_label.into(),
                activate: Box::new(|this: &mut HyprcorrectTray| {
                    let _ = this.events_tx.send(TrayEvent::TogglePause);
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Open Preferences…".into(),
                activate: Box::new(|this: &mut HyprcorrectTray| {
                    let _ = this.events_tx.send(TrayEvent::OpenPrefs);
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Quit".into(),
                activate: Box::new(|this: &mut HyprcorrectTray| {
                    let _ = this.events_tx.send(TrayEvent::Quit);
                }),
                ..Default::default()
            }
            .into(),
        ]
    }
}
