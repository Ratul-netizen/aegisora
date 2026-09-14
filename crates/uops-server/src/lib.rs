//! The server, as a library.
//!
//! `main.rs` is the order these pieces happen in; this is the pieces. The split exists
//! so that the first-run path can be tested — a boot sequence whose only output is a
//! terminal is a boot sequence nothing can assert on, and "can the administrator we
//! just created actually log in" is the one question worth asking about it.

pub mod config;
pub mod firstrun;
pub mod shutdown;
