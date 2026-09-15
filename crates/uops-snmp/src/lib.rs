//! The SNMP client — SPEC §M2.
//!
//! # Why `snmp2`
//!
//! The two maintained candidates are `snmp2` and `async-snmp`, and one fact decides it:
//! `async-snmp` depends on `aws-lc-rs`, whose licence is `ISC AND MIT AND OpenSSL` — the
//! one this workspace has avoided everywhere, which is why there is no TLS in it. See
//! the note in the workspace manifest.
//!
//! `snmp2` offers the same choice as a feature and defaults the other way. With
//! `default-features = false, features = ["v3", "crypto-rust", "tokio"]` it pulls
//! `RustCrypto` — `aes`, `sha2`, `hmac`, `cbc`, `cfb-mode`, `des`, `md-5` — all MIT
//! or Apache-2.0, all pure Rust, and the same stack the `crypto-builds` CI job already
//! covers. `cargo deny` passes unchanged.
//!
//! The cost is the one SPEC names in the same breath: *"`async-snmp` does this
//! automatically; `snmp2` needs it written"*. That is [`bulk`], and writing it is better
//! than inheriting it — the halving strategy has two properties that matter operationally
//! and neither is obvious, so owning them beats trusting them.

pub mod bulk;
pub mod credential;
pub mod sim;
pub mod transport;
pub mod walk;

pub use bulk::{Repetitions, Tuning};
pub use credential::{CredentialError, SnmpCredential, Strength};
pub use transport::{Target, Transport, TransportError, Value, VarBind};
pub use walk::{WalkError, walk};
