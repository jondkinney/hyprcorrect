//! Raw Core Graphics / Core Foundation FFI shared by the macOS
//! `capture` (listen-only `CGEventTap`) and `emit` (synthetic
//! `CGEvent`) backends.
//!
//! `objc2-core-graphics` 0.3 wraps `CGImage` and color spaces (used
//! by the tray) but not the Quartz *event* API — `CGEventTapCreate`,
//! the keyboard-unicode accessors, and the TCC pre-flight/request
//! pair — so those are declared here directly against the
//! CoreGraphics / CoreFoundation / ApplicationServices frameworks,
//! the same way `hotkey.rs` declares Carbon's `RegisterEventHotKey`.

#![allow(non_upper_case_globals, non_snake_case, dead_code)]

use std::os::raw::c_void;

// --- Opaque Core Foundation / Core Graphics handle types ---------------------

/// `CGEventRef` — an in-flight or freshly-created Quartz event.
pub(crate) type CGEventRef = *mut c_void;
/// `CGEventSourceRef` — the source stamped onto synthesized events.
pub(crate) type CGEventSourceRef = *mut c_void;
/// `CFMachPortRef` — the mach port backing an event tap.
pub(crate) type CFMachPortRef = *mut c_void;
/// `CFRunLoopSourceRef` — a run-loop source created from the port.
pub(crate) type CFRunLoopSourceRef = *mut c_void;
/// `CFRunLoopRef` — a thread's run loop.
pub(crate) type CFRunLoopRef = *mut c_void;
/// `CGEventTapProxy` — opaque, passed to the tap callback.
pub(crate) type CGEventTapProxy = *mut c_void;

/// The tap callback ABI:
/// `CGEventRef (*)(CGEventTapProxy, CGEventType, CGEventRef, void *)`.
/// Returning the event keeps it; returning null drops it (only
/// meaningful for active taps — our listen-only tap's return value
/// is ignored, but the signature must still match).
pub(crate) type CGEventTapCallBack = unsafe extern "C" fn(
    proxy: CGEventTapProxy,
    etype: u32,
    event: CGEventRef,
    user_info: *mut c_void,
) -> CGEventRef;

// --- Enum constants (uint32 unless noted) ------------------------------------

// CGEventTapLocation
pub(crate) const kCGHIDEventTap: u32 = 0;
pub(crate) const kCGSessionEventTap: u32 = 1;
pub(crate) const kCGAnnotatedSessionEventTap: u32 = 2;

// CGEventTapPlacement
pub(crate) const kCGHeadInsertEventTap: u32 = 0;

// CGEventTapOptions
pub(crate) const kCGEventTapOptionDefault: u32 = 0;
pub(crate) const kCGEventTapOptionListenOnly: u32 = 1;

// CGEventType (the ones we mask / dispatch on).
pub(crate) const kCGEventLeftMouseDown: u32 = 1;
pub(crate) const kCGEventKeyDown: u32 = 10;
pub(crate) const kCGEventKeyUp: u32 = 11;
pub(crate) const kCGEventFlagsChanged: u32 = 12;
// The system disables a tap by delivering one of these two sentinel
// "event types" through the callback rather than a real event.
pub(crate) const kCGEventTapDisabledByTimeout: u32 = 0xFFFF_FFFE;
pub(crate) const kCGEventTapDisabledByUserInput: u32 = 0xFFFF_FFFF;

// CGEventField
pub(crate) const kCGKeyboardEventKeycode: u32 = 9;
/// `kCGEventSourceUserData` — a 64-bit field we stamp on the events we
/// synthesize so the capture tap can recognise and skip its own output.
/// (A session tap *does* see events we `CGEventPost`, unlike Linux evdev
/// vs. wtype — so without this tag every correction would re-capture its
/// own backspaces and retyped text into the buffer.)
pub(crate) const kCGEventSourceUserData: u32 = 42;

/// Magic value stamped into `kCGEventSourceUserData` on synthetic events.
/// `0x68_79_70_72` is ASCII "hypr".
pub(crate) const SYNTHETIC_MARK: i64 = 0x6879_7072;

// CGEventFlags (uint64 bitmask). Same bit positions as NSEvent's
// device-independent modifier flags.
pub(crate) const kCGEventFlagMaskAlphaShift: u64 = 1 << 16;
pub(crate) const kCGEventFlagMaskShift: u64 = 1 << 17;
pub(crate) const kCGEventFlagMaskControl: u64 = 1 << 18;
pub(crate) const kCGEventFlagMaskAlternate: u64 = 1 << 19; // Option
pub(crate) const kCGEventFlagMaskCommand: u64 = 1 << 20;
pub(crate) const kCGEventFlagMaskSecondaryFn: u64 = 1 << 23;
pub(crate) const kCGEventFlagMaskNumericPad: u64 = 1 << 21;

// CGEventSourceStateID
pub(crate) const kCGEventSourceStateHIDSystemState: i32 = 1;

/// Build a `CGEventMask` (uint64) bit for a given event type.
pub(crate) const fn event_mask_bit(event_type: u32) -> u64 {
    1u64 << event_type
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    pub(crate) fn CGEventTapCreate(
        tap: u32,
        place: u32,
        options: u32,
        events_of_interest: u64,
        callback: CGEventTapCallBack,
        user_info: *mut c_void,
    ) -> CFMachPortRef;

    pub(crate) fn CGEventTapEnable(tap: CFMachPortRef, enable: bool);

    pub(crate) fn CGEventGetIntegerValueField(event: CGEventRef, field: u32) -> i64;

    pub(crate) fn CGEventSetIntegerValueField(event: CGEventRef, field: u32, value: i64);

    pub(crate) fn CGEventGetFlags(event: CGEventRef) -> u64;

    pub(crate) fn CGEventKeyboardGetUnicodeString(
        event: CGEventRef,
        max_len: usize,
        actual_len: *mut usize,
        unicode_string: *mut u16,
    );

    pub(crate) fn CGEventCreateKeyboardEvent(
        source: CGEventSourceRef,
        virtual_key: u16,
        key_down: bool,
    ) -> CGEventRef;

    pub(crate) fn CGEventKeyboardSetUnicodeString(
        event: CGEventRef,
        length: usize,
        unicode_string: *const u16,
    );

    pub(crate) fn CGEventSetFlags(event: CGEventRef, flags: u64);

    pub(crate) fn CGEventPost(tap: u32, event: CGEventRef);

    pub(crate) fn CGEventSourceCreate(state_id: i32) -> CGEventSourceRef;

    // TCC pre-flight / request (macOS 10.15+). Pre-flight is a silent
    // check; request prompts the user (and registers the binary in the
    // relevant System Settings list) the first time.
    pub(crate) fn CGPreflightListenEventAccess() -> bool;
    pub(crate) fn CGRequestListenEventAccess() -> bool;
    pub(crate) fn CGPreflightPostEventAccess() -> bool;
    pub(crate) fn CGRequestPostEventAccess() -> bool;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    pub(crate) fn CFRelease(cf: *const c_void);

    pub(crate) fn CFRunLoopGetCurrent() -> CFRunLoopRef;

    pub(crate) fn CFMachPortCreateRunLoopSource(
        allocator: *const c_void,
        port: CFMachPortRef,
        order: isize,
    ) -> CFRunLoopSourceRef;

    pub(crate) fn CFRunLoopAddSource(
        rl: CFRunLoopRef,
        source: CFRunLoopSourceRef,
        mode: *const c_void,
    );

    pub(crate) fn CFRunLoopRun();

    pub(crate) fn CFRunLoopStop(rl: CFRunLoopRef);

    /// `kCFRunLoopCommonModes` — a `CFRunLoopMode` (CFString) constant.
    pub(crate) static kCFRunLoopCommonModes: *const c_void;

    /// One-key dictionary builder for the Accessibility prompt option.
    /// Pass a null allocator for the default. The `*CallBacks` args want
    /// the ADDRESS of the global callbacks structs (declared opaque
    /// below), so callers pass `&raw const kCFType…CallBacks`.
    pub(crate) fn CFDictionaryCreate(
        allocator: *const c_void,
        keys: *const *const c_void,
        values: *const *const c_void,
        num_values: isize,
        key_callbacks: *const c_void,
        value_callbacks: *const c_void,
    ) -> *const c_void;

    pub(crate) static kCFTypeDictionaryKeyCallBacks: c_void;
    pub(crate) static kCFTypeDictionaryValueCallBacks: c_void;
    pub(crate) static kCFBooleanTrue: *const c_void;
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    /// Silent check: is this process trusted for Accessibility?
    /// (Synthetic event posting falls under Accessibility on macOS
    /// 13+.) Returns cached-true within a process after a grant.
    pub(crate) fn AXIsProcessTrusted() -> bool;

    /// Like [`AXIsProcessTrusted`], but with an options dictionary —
    /// pass `{kAXTrustedCheckOptionPrompt: true}` to show the system
    /// "would like to control this computer" alert and list the app
    /// under Accessibility when it isn't already trusted.
    pub(crate) fn AXIsProcessTrustedWithOptions(options: *const c_void) -> bool;

    /// `CFStringRef` key for the prompt option above.
    pub(crate) static kAXTrustedCheckOptionPrompt: *const c_void;
}
