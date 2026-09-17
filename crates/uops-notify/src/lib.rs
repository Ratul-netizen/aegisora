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
//! # Only webhooks, for now
//!
//! Email is the other half of SPEC's v0.1 and is not here yet. It is not a small
//! addition: this workspace carries no TLS by deliberate decision — see the root
//! `Cargo.toml` — so SMTP submission has to be a plain connection to a relay on the
//! customer's own network, with header injection handled by hand. That is a design to
//! write down rather than to slip in beside a webhook, and a webhook is what an
//! on-premise deployment already has an endpoint for.

pub mod notification;
pub mod notifier;
pub mod webhook;

pub use notification::Notification;
pub use notifier::{Delivered, Notifier};
pub use webhook::Webhook;
