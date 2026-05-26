//! Chord recording from the keystroke capture loop.
//!
//! The prefs UI cannot reliably record chords through egui on Linux —
//! egui-winit drops the Super modifier in its translation from
//! `winit::keyboard::ModifiersState` to `egui::Modifiers`. So when the
//! user wants to record a Super-containing chord, prefs asks the
//! daemon: the daemon already reads evdev + tracks xkb modifier state
//! and sees Super correctly, so it can report the next non-modifier
//! key press together with the exact modifier mask Wayland is
//! delivering.
//!
//! This module is the bridge: capture threads check the shared
//! [`ChordCaptureSlot`] on every key press; when the slot is armed
//! (i.e. prefs has asked for a chord), the next non-modifier press is
//! delivered to the slot's one-shot sender instead of going through
//! the normal buffer path.
//!
//! Linux only. The corresponding Wayland-side capture for vernier
//! lives in a different crate.
//!
//! See `DESIGN.md` § "Configuration & GUI" → chord recorder.
//!
//! ```text
//! prefs ── unix socket ──▶ daemon
//!                          │
//!                          ▼
//!                  arm(slot) ──▶ ChordCaptureSlot
//!                                      ▲
//!                                      │ try_emit on next key
//!                                      │
//!                          capture thread (evdev + xkb)
//! ```

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{
    Arc, Mutex,
    mpsc::{self, RecvTimeoutError, SyncSender},
};
use std::thread;
use std::time::Duration;

use hyprcorrect_core::runtime;

/// Shared, thread-safe state for chord recording. One slot per daemon
/// process — created at daemon startup, cloned into each capture
/// thread.
///
/// "Armed" means a sender is parked inside; the next non-modifier key
/// press in any capture thread takes the sender and delivers the
/// chord string. After delivery (or timeout / cancel), the slot
/// becomes unarmed.
#[derive(Default)]
pub struct ChordCaptureSlot {
    sender: Mutex<Option<SyncSender<String>>>,
}

impl ChordCaptureSlot {
    /// A fresh, unarmed slot. Wrap in `Arc` to share with capture
    /// threads.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Arm the slot and block (up to `timeout`) for the next chord
    /// string a capture thread sends. Cancels itself on timeout —
    /// any later key press is delivered through the normal buffer
    /// path again.
    ///
    /// Only one caller may be armed at a time; concurrent arms
    /// replace the prior sender (the prior caller will receive a
    /// timeout instead of a chord).
    pub fn record(&self, timeout: Duration) -> Result<String, ChordCaptureError> {
        let (tx, rx) = mpsc::sync_channel::<String>(1);
        {
            let mut slot = self.sender.lock().expect("chord-capture slot poisoned");
            *slot = Some(tx);
        }
        let result = match rx.recv_timeout(timeout) {
            Ok(chord) => Ok(chord),
            Err(RecvTimeoutError::Timeout) => Err(ChordCaptureError::Timeout),
            Err(RecvTimeoutError::Disconnected) => Err(ChordCaptureError::Cancelled),
        };
        // Either we got our chord (and the capture thread already
        // took the sender), or we timed out / were preempted: in
        // both cases, ensure the slot is back to a known state.
        if result.is_err() {
            self.cancel();
        }
        result
    }

    /// Clear any armed sender. Called when the IPC client disconnects
    /// mid-record without us hearing back.
    pub fn cancel(&self) {
        let mut slot = self.sender.lock().expect("chord-capture slot poisoned");
        *slot = None;
    }

    /// Capture-thread side: if the slot is armed, take the sender,
    /// deliver `chord`, and return `true` so the capture thread
    /// suppresses the press from the normal buffer path. Returns
    /// `false` (and leaves the slot alone) when not armed.
    pub fn try_emit(&self, chord: String) -> bool {
        let sender = {
            let mut slot = self.sender.lock().expect("chord-capture slot poisoned");
            slot.take()
        };
        match sender {
            Some(tx) => {
                let _ = tx.try_send(chord);
                true
            }
            None => false,
        }
    }

    /// Cheap arm check used on the capture-thread hot path so we can
    /// skip the chord-string formatting work when no one's listening.
    pub fn is_armed(&self) -> bool {
        self.sender
            .lock()
            .expect("chord-capture slot poisoned")
            .is_some()
    }
}

/// Why a `record()` call did not return a chord.
#[derive(Debug, thiserror::Error)]
pub enum ChordCaptureError {
    /// No key was pressed within the timeout.
    #[error("chord-capture timed out")]
    Timeout,
    /// Another caller armed the slot and pre-empted ours.
    #[error("chord-capture was cancelled before a key arrived")]
    Cancelled,
}

/// Errors starting the chord-capture IPC listener.
#[derive(Debug, thiserror::Error)]
pub enum ListenerError {
    #[error("chord-capture socket bind: {0}")]
    Bind(String),
}

/// Default per-request timeout. 30 s is long enough that the user can
/// click "Record" and then walk back to the keyboard, but short
/// enough that a forgotten armed slot doesn't pin the daemon forever.
pub const DEFAULT_RECORD_TIMEOUT: Duration = Duration::from_secs(30);

/// Start the chord-capture Unix-socket listener in a background
/// thread. The thread runs for the life of the daemon, accepting
/// one prefs client at a time.
///
/// Protocol (newline-terminated text):
/// ```text
/// → capture
/// ← <chord-string>\n   on success
/// ← cancel\n           when no key arrived in time / another client armed first
/// ← err <reason>\n     on internal error
/// ```
///
/// A disconnect from the client (closed stream) cancels an in-flight
/// record on this slot.
///
/// # Errors
///
/// Returns [`ListenerError::Bind`] if the socket path could not be
/// claimed — most commonly a stale socket left behind by a prior
/// daemon crash; the caller can remove it and retry.
pub fn start_listener(slot: Arc<ChordCaptureSlot>) -> Result<(), ListenerError> {
    let path = runtime::chord_socket_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // A previous daemon may have crashed without unlinking the
    // socket; the next bind would EADDRINUSE forever otherwise.
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).map_err(|e| ListenerError::Bind(e.to_string()))?;
    thread::spawn(move || {
        for incoming in listener.incoming() {
            let Ok(stream) = incoming else {
                continue;
            };
            let slot = slot.clone();
            thread::spawn(move || serve_client(stream, slot));
        }
    });
    Ok(())
}

/// Errors connecting to the chord-capture socket from a prefs-like
/// client.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// The daemon is not running or hasn't started its listener yet.
    #[error("daemon not running (no chord-capture socket)")]
    DaemonOffline,
    /// Couldn't connect or talk to the daemon.
    #[error("chord-capture IPC: {0}")]
    Io(String),
    /// The daemon replied that no key arrived (timeout or pre-empted).
    #[error("chord-capture cancelled")]
    Cancelled,
    /// The daemon replied with an error.
    #[error("daemon error: {0}")]
    Daemon(String),
}

/// A client-side handle to a chord-capture in progress. The actual
/// blocking read is on a worker thread the caller doesn't have to
/// manage — poll [`try_recv`](Self::try_recv) each UI frame and
/// call [`abort`](Self::abort) on Esc or window close.
pub struct ChordRecording {
    rx: mpsc::Receiver<Result<String, ClientError>>,
    abort: UnixStream,
}

impl ChordRecording {
    /// Non-blocking poll for the daemon's reply. `Ok(None)` means
    /// "still waiting"; `Ok(Some(chord))` is the recorded chord;
    /// `Err(_)` is an IPC / timeout / cancel.
    pub fn try_recv(&self) -> Result<Option<String>, ClientError> {
        match self.rx.try_recv() {
            Ok(result) => result.map(Some),
            Err(mpsc::TryRecvError::Empty) => Ok(None),
            Err(mpsc::TryRecvError::Disconnected) => Err(ClientError::Cancelled),
        }
    }

    /// Cancel the in-flight recording. Half-closes the socket; the
    /// daemon's disconnect-watcher then cancels the armed slot and
    /// the worker thread exits.
    pub fn abort(&self) {
        let _ = self.abort.shutdown(std::net::Shutdown::Both);
    }
}

/// Connect to the daemon's chord-capture socket, arm a recording,
/// and spawn a worker thread that blocks on the reply. Caller polls
/// the returned [`ChordRecording`] from its UI loop.
///
/// # Errors
///
/// See [`ClientError`].
pub fn record_chord() -> Result<ChordRecording, ClientError> {
    let path = runtime::chord_socket_path();
    let mut stream = UnixStream::connect(&path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound
            || e.kind() == std::io::ErrorKind::ConnectionRefused
        {
            ClientError::DaemonOffline
        } else {
            ClientError::Io(e.to_string())
        }
    })?;
    stream
        .write_all(b"capture\n")
        .map_err(|e| ClientError::Io(e.to_string()))?;
    let abort = stream
        .try_clone()
        .map_err(|e| ClientError::Io(e.to_string()))?;

    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        let result = match reader.read_line(&mut line) {
            Ok(_) => parse_reply(line.trim()),
            Err(e) => Err(ClientError::Io(e.to_string())),
        };
        let _ = tx.send(result);
    });
    Ok(ChordRecording { rx, abort })
}

fn parse_reply(line: &str) -> Result<String, ClientError> {
    if line.is_empty() {
        return Err(ClientError::Cancelled);
    }
    if let Some(rest) = line.strip_prefix("err ") {
        return Err(ClientError::Daemon(rest.to_string()));
    }
    if line == "cancel" {
        return Err(ClientError::Cancelled);
    }
    Ok(line.to_string())
}

fn serve_client(stream: UnixStream, slot: Arc<ChordCaptureSlot>) {
    let Ok(reader_stream) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(reader_stream);
    let mut writer = stream;
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return;
    }
    let request = line.trim();
    match request {
        "capture" => {
            // While we wait on the slot, watch the client's read
            // half: if they shut it down (e.g. user pressed Esc and
            // prefs called PendingCapture::abort), cancel the
            // record so slot.record() returns promptly.
            let watcher_slot = slot.clone();
            let watcher = thread::spawn(move || {
                let mut sink = [0u8; 16];
                use std::io::Read;
                let mut reader = reader;
                loop {
                    match reader.get_mut().read(&mut sink) {
                        Ok(0) | Err(_) => {
                            watcher_slot.cancel();
                            return;
                        }
                        Ok(_) => {
                            // Extra chatter from the client is
                            // ignored — only EOF is meaningful.
                        }
                    }
                }
            });
            let result = slot.record(DEFAULT_RECORD_TIMEOUT);
            match result {
                Ok(chord) => {
                    let _ = writeln!(writer, "{chord}");
                }
                Err(ChordCaptureError::Timeout | ChordCaptureError::Cancelled) => {
                    let _ = writeln!(writer, "cancel");
                }
            }
            // Stop watching: shutting down the stream wakes the
            // watcher out of its blocked read.
            let _ = writer.shutdown(std::net::Shutdown::Both);
            let _ = watcher.join();
        }
        other => {
            let _ = writeln!(writer, "err unknown request: {other}");
        }
    }
}
