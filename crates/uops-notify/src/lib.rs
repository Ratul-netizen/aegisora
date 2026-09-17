//! Delivering an alert to somebody — SPEC §M4.
//!
//! ```text
//!   render    what a firing alert says                    notification.rs
//!   reserve   may this be sent at all                     uops_store_pg::notify
//!   deliver   the transport                               webhook.rs
//!   record    what happened, including the refusals       uops_store_pg::notify
//! ```
//!
//! # The limits come first, and they are the point
//!
//! SPEC: *"per-channel rate limiting and a per-tenant notification budget, because the
//! first misconfigured rule **will** try to send 10 000 emails."* So nothing here sends
//! anything until the database has said it may — and the refusal is written down beside
//! the deliveries, because *"why did nobody get paged"* is a question somebody asks at a
//! bad moment and an unanswerable one is worse than the outage.
//!
//! # Two transports, and what the no-TLS decision means for each
//!
//! This workspace carries no TLS by deliberate decision — see the root `Cargo.toml`. For
//! a **webhook** that means an `https://` endpoint goes through the egress proxy the
//! deployment already runs, and an https URL is refused when the channel is written.
//!
//! For **email** it means something stronger: authenticated submission is not supported at
//! all, because `AUTH PLAIN` over an unencrypted connection sends a password in clear and
//! no amount of documentation makes that safe. What is supported is the shape an
//! on-premise mail setup already has — a **smarthost** that accepts mail from the hosts on
//! its own network. A deployment that must reach an authenticated provider puts a
//! submission proxy in front, which is the same answer syslog-over-TLS got. See
//! [`smtp`].

pub mod notification;
pub mod notifier;
pub mod smtp;
pub mod webhook;

pub use notification::Notification;
pub use notifier::{Delivered, Notifier};
pub use smtp::Smtp;
pub use webhook::Webhook;
