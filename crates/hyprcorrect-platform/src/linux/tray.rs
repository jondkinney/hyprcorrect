//! System tray (ksni-based) for the hyprcorrect daemon.
//!
//! Publishes a StatusNotifierItem with a small menu. Menu activations
//! arrive on the returned channel.

use std::sync::mpsc::{self, Receiver, Sender, SyncSender};

/// A menu event from the tray.
#[derive(Debug)]
pub enum TrayEvent {
    /// The user picked "Quit" from the tray menu.
    Quit,
}

/// An error starting the tray.
#[derive(Debug, thiserror::Error)]
pub enum TrayError {
    /// Could not spawn the tray thread.
    #[error("could not spawn tray thread: {0}")]
    Thread(String),
    /// Could not start the ksni service (D-Bus / SNI publication).
    #[error("ksni: {0}")]
    Ksni(String),
}

/// Start the tray icon. Returns a receiver of menu activations.
///
/// # Errors
///
/// See [`TrayError`] — thread spawn or ksni service failure.
pub fn start() -> Result<Receiver<TrayEvent>, TrayError> {
    let (tx, rx) = mpsc::channel();
    let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<(), TrayError>>(1);
    let ready_tx_for_thread = ready_tx.clone();

    std::thread::Builder::new()
        .name("hyprcorrect-tray".into())
        .spawn(move || {
            if let Err(e) = run_tray(tx, &ready_tx_for_thread) {
                let _ = ready_tx_for_thread.send(Err(e));
            }
        })
        .map_err(|e| TrayError::Thread(e.to_string()))?;

    ready_rx
        .recv()
        .map_err(|_| TrayError::Thread("tray init failed".into()))??;

    Ok(rx)
}

struct HyprcorrectTray {
    events_tx: Sender<TrayEvent>,
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

    fn icon_name(&self) -> String {
        // A built-in theme icon for now — bundling a proper hyprcorrect
        // icon (SVG rendered via tiny-skia, vernier-style) is M3 polish.
        "tools-check-spelling".to_string()
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: "hyprcorrect".into(),
            description: "Press Super+Ctrl+Shift+Alt+F to correct the last word".into(),
            icon_name: String::new(),
            icon_pixmap: Vec::new(),
        }
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::StandardItem;
        vec![
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

fn run_tray(
    events_tx: Sender<TrayEvent>,
    ready_tx: &SyncSender<Result<(), TrayError>>,
) -> Result<(), TrayError> {
    use ksni::blocking::TrayMethods;

    let tray = HyprcorrectTray { events_tx };
    let _handle = tray.spawn().map_err(|e| TrayError::Ksni(e.to_string()))?;
    let _ = ready_tx.send(Ok(()));

    // Keep the tray handle alive for the life of the process.
    std::thread::park();
    Ok(())
}
