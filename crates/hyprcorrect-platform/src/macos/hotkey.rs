//! Global trigger via Carbon `RegisterEventHotKey` + Unix signals.
//!
//! Carbon's `RegisterEventHotKey` is the one macOS API that registers a
//! global chord *without* any TCC permission, and — crucially — it
//! *intercepts* the chord, so terminals and other focused apps never see
//! the raw key (the same property Hyprland's bind gives us on Linux).
//!
//! To keep the daemon's main loop byte-for-byte identical across
//! platforms, the Carbon callback does exactly what the Linux bind's
//! `exec` does: write the action label to the runtime action file, then
//! `raise(SIGUSR1)`. So [`signal_channel`] is the same signal-hook
//! listener as Linux, and the daemon's `Trigger` arm reads the action
//! the same way. `SIGUSR2`/`SIGHUP`/`SIGTERM`/`SIGINT` carry
//! Release/Reload/Shutdown exactly as on Linux (the prefs subprocess
//! signals the daemon by PID).

use std::os::raw::{c_int, c_void};
use std::sync::mpsc::{self, Receiver, Sender};

use hyprcorrect_core::{Chord, runtime};
use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM, SIGUSR1, SIGUSR2};
use signal_hook::iterator::Signals;

use super::keymap::chord_to_carbon;

/// A daemon-level event driven by the OS signal stream (Trigger arrives
/// via the Carbon callback's `raise(SIGUSR1)`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyEvent {
    /// `SIGUSR1` — the trigger chord fired. Run the recorded action.
    Trigger,
    /// `SIGHUP` — the user saved the config. Reload and rebind.
    Reload,
    /// `SIGUSR2` — the prefs window entered chord-capture; release the
    /// chord so prefs can record it. Re-bound on `Reload`.
    Release,
    /// `SIGTERM` / `SIGINT` — shut down cleanly.
    Shutdown,
}

#[derive(Debug, thiserror::Error)]
pub enum HotkeyError {
    /// Could not register the Carbon hotkey for the chord.
    #[error("could not register the global hotkey: {0}")]
    Register(String),
    /// Could not unregister a Carbon hotkey.
    #[error("could not unregister the global hotkey: {0}")]
    Unregister(String),
    /// Could not install the signal handler.
    #[error("could not install signal handler: {0}")]
    Signal(String),
    /// Could not spawn the signal-listener thread.
    #[error("could not spawn the signal-listener thread: {0}")]
    Thread(String),
}

// --- Carbon FFI -------------------------------------------------------------

#[repr(C)]
#[derive(Clone, Copy)]
struct EventHotKeyID {
    signature: u32,
    id: u32,
}

const FOUR_CC_HYPR: u32 = u32::from_be_bytes(*b"HYPR");

#[repr(C)]
#[derive(Clone, Copy)]
struct EventTypeSpec {
    event_class: u32,
    event_kind: u32,
}

const K_EVENT_CLASS_KEYBOARD: u32 = u32::from_be_bytes(*b"keyb");
const K_EVENT_HOT_KEY_PRESSED: u32 = 5;
const K_EVENT_PARAM_DIRECT_OBJECT: u32 = u32::from_be_bytes(*b"----");
const TYPE_EVENT_HOT_KEY_ID: u32 = u32::from_be_bytes(*b"hkid");

type EventRef = *mut c_void;
type EventHandlerRef = *mut c_void;
type EventTargetRef = *mut c_void;
type EventHandlerCallRef = *mut c_void;
type EventHandlerUPP = unsafe extern "C" fn(
    next: EventHandlerCallRef,
    event: EventRef,
    user_data: *mut c_void,
) -> c_int;

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn GetApplicationEventTarget() -> EventTargetRef;
    fn RegisterEventHotKey(
        in_hot_key_code: u32,
        in_hot_key_modifiers: u32,
        in_hot_key_id: EventHotKeyID,
        in_target: EventTargetRef,
        in_options: u32,
        out_ref: *mut *mut c_void,
    ) -> c_int;
    fn UnregisterEventHotKey(in_hot_key_ref: *mut c_void) -> c_int;
    fn InstallEventHandler(
        in_target: EventTargetRef,
        in_handler: EventHandlerUPP,
        // ItemCount == `unsigned long` (64-bit under LP64), NOT u32.
        in_num_types: usize,
        in_list: *const EventTypeSpec,
        in_user_data: *mut c_void,
        out_handler: *mut EventHandlerRef,
    ) -> c_int;
    fn GetEventParameter(
        in_event: EventRef,
        in_name: u32,
        in_desired_type: u32,
        // EventParamType is UInt32 (4-byte) — this one stays u32.
        out_actual_type: *mut u32,
        // inBufferSize / outActualSize are `ByteCount` == `unsigned long`
        // (64-bit under LP64). Declaring them u32 made GetEventParameter
        // write 8 bytes through a 4-byte slot — an out-of-bounds stack
        // write on every hotkey press.
        in_buffer_size: usize,
        out_actual_size: *mut usize,
        out_data: *mut c_void,
    ) -> c_int;
}

/// One registered Carbon hotkey: the `UnregisterEventHotKey` handle, the
/// chord (to find it again for `uninstall_bind`), and the action label
/// the callback writes before signalling.
pub(crate) struct HotkeyResources {
    carbon_ref: *mut c_void,
    chord: Chord,
    action: String,
}

// Carbon handles are main-thread-only by convention; we never touch them
// off-main (all access is inside `run_on_main_sync` or the main-dispatched
// Carbon callback).
unsafe impl Send for HotkeyResources {}

/// Register the global hotkey for `chord`, tagged with `action`
/// (`"word"` / `"sentence"` / `"review"` / `"review-llm"`). Idempotent:
/// any existing binding for the same chord is removed first.
pub fn install_bind(chord: &Chord, action: &str) -> Result<(), HotkeyError> {
    let _ = uninstall_bind(chord);

    let (vkey, mods) = chord_to_carbon(chord)
        .ok_or_else(|| HotkeyError::Register(format!("no macOS keycode for chord '{chord}'")))?;
    let id = super::next_id();
    let chord = chord.clone();
    let action = action.to_string();

    super::app::run_on_main_sync(move || -> Result<(), HotkeyError> {
        ensure_handler_installed()?;
        let mut carbon_ref: *mut c_void = std::ptr::null_mut();
        let status = unsafe {
            RegisterEventHotKey(
                vkey,
                mods,
                EventHotKeyID {
                    signature: FOUR_CC_HYPR,
                    id,
                },
                GetApplicationEventTarget(),
                0,
                &mut carbon_ref,
            )
        };
        if status != 0 || carbon_ref.is_null() {
            return Err(HotkeyError::Register(format!(
                "RegisterEventHotKey returned OSStatus {status}"
            )));
        }
        log::info!(
            "macos hotkey: registered '{chord}' (vkey={vkey:#x} mods={mods:#x}) as id={id} action='{action}'"
        );
        super::with_main_state(|s| {
            s.hotkeys.insert(
                id,
                HotkeyResources {
                    carbon_ref,
                    chord,
                    action,
                },
            );
        });
        Ok(())
    })
}

/// Unregister every Carbon hotkey bound to `chord`. Calling for an
/// unbound chord is silently fine.
pub fn uninstall_bind(chord: &Chord) -> Result<(), HotkeyError> {
    let chord = chord.clone();
    super::app::run_on_main_sync(move || -> Result<(), HotkeyError> {
        let ids: Vec<u32> = super::with_main_state(|s| {
            s.hotkeys
                .iter()
                .filter(|(_, r)| r.chord == chord)
                .map(|(id, _)| *id)
                .collect()
        });
        for id in ids {
            let res = super::with_main_state(|s| s.hotkeys.remove(&id));
            if let Some(res) = res {
                let status = unsafe { UnregisterEventHotKey(res.carbon_ref) };
                if status != 0 {
                    return Err(HotkeyError::Unregister(format!(
                        "UnregisterEventHotKey returned OSStatus {status}"
                    )));
                }
            }
        }
        Ok(())
    })
}

fn ensure_handler_installed() -> Result<(), HotkeyError> {
    super::with_main_state(|state| {
        if state.carbon_handler_installed {
            return Ok(());
        }
        let spec = EventTypeSpec {
            event_class: K_EVENT_CLASS_KEYBOARD,
            event_kind: K_EVENT_HOT_KEY_PRESSED,
        };
        let mut handler_ref: EventHandlerRef = std::ptr::null_mut();
        let status = unsafe {
            InstallEventHandler(
                GetApplicationEventTarget(),
                hotkey_handler,
                1,
                &spec,
                std::ptr::null_mut(),
                &mut handler_ref,
            )
        };
        if status != 0 {
            return Err(HotkeyError::Register(format!(
                "InstallEventHandler returned OSStatus {status}"
            )));
        }
        state.carbon_handler_installed = true;
        Ok(())
    })
}

/// Carbon dispatches this on the main run loop when a registered chord
/// fires. It writes the chord's action label to the runtime action file
/// and `raise`s `SIGUSR1`, so the daemon's signal listener turns it into
/// `HotkeyEvent::Trigger` and reads the action — identical to Linux.
unsafe extern "C" fn hotkey_handler(
    _next: EventHandlerCallRef,
    event: EventRef,
    _user_data: *mut c_void,
) -> c_int {
    let mut hk_id = EventHotKeyID {
        signature: 0,
        id: 0,
    };
    let mut actual_size: usize = 0;
    let status = unsafe {
        GetEventParameter(
            event,
            K_EVENT_PARAM_DIRECT_OBJECT,
            TYPE_EVENT_HOT_KEY_ID,
            std::ptr::null_mut(),
            std::mem::size_of::<EventHotKeyID>(),
            &mut actual_size,
            (&mut hk_id) as *mut _ as *mut c_void,
        )
    };
    if status != 0 || hk_id.signature != FOUR_CC_HYPR {
        return 0; // noErr — let the event continue.
    }
    let action = super::with_main_state(|s| s.hotkeys.get(&hk_id.id).map(|r| r.action.clone()));
    if let Some(action) = action {
        log::debug!("macos hotkey: fired id={} action='{action}'", hk_id.id);
        if let Err(e) = std::fs::write(runtime::action_path(), action.as_bytes()) {
            log::warn!("macos hotkey: could not write action file: {e}");
        }
        unsafe {
            libc::raise(SIGUSR1);
        }
    } else {
        log::warn!("macos hotkey: no action registered for id={}", hk_id.id);
    }
    0
}

/// Start the signal listener — identical to Linux. Trigger arrives via
/// the Carbon callback's `raise(SIGUSR1)`; Release/Reload/Shutdown come
/// from the prefs subprocess / the OS by PID.
pub fn signal_channel() -> Result<Receiver<HotkeyEvent>, HotkeyError> {
    let mut signals = Signals::new([SIGUSR1, SIGUSR2, SIGHUP, SIGTERM, SIGINT])
        .map_err(|e| HotkeyError::Signal(e.to_string()))?;
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("hyprcorrect-signal".into())
        .spawn(move || forward_signals(&mut signals, &tx))
        .map_err(|e| HotkeyError::Thread(e.to_string()))?;
    Ok(rx)
}

fn forward_signals(signals: &mut Signals, tx: &Sender<HotkeyEvent>) {
    for signal in signals.forever() {
        let event = match signal {
            SIGUSR1 => HotkeyEvent::Trigger,
            SIGUSR2 => HotkeyEvent::Release,
            SIGHUP => HotkeyEvent::Reload,
            SIGTERM | SIGINT => HotkeyEvent::Shutdown,
            _ => continue,
        };
        if tx.send(event).is_err() {
            break;
        }
    }
}
