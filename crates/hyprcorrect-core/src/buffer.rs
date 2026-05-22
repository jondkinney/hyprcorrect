//! The keystroke buffer: a bounded, in-memory record of recently typed
//! text in the focused element. It lets hyprcorrect answer "what was the
//! last word / sentence?" without reading back from the focused
//! application — which is what makes correction work in terminals.
//!
//! Implemented in milestone M1. See the "keystroke buffer" section of
//! `DESIGN.md`.
