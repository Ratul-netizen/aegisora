//! Recording who read what, and who changed what — SPEC §M0.8.
//!
//! # Where the hook lives, and why it is there
//!
//! SPEC says "implemented once as Axum middleware over the query and resource routes".
//! A layer wrapped around a chosen list of routes has one failure mode: someone adds a
//! twenty-first route and does not add it to the list. So the hook is attached to
//! [`crate::Caller`] instead — the extractor a handler *must* use to obtain a
//! `TenantScope`, and therefore the only way to reach tenant data at all.
//!
//! A handler cannot read a customer's data without a scope, cannot get a scope without
//! the extractor, and cannot use the extractor without registering here. Authorisation
//! and auditing share a chokepoint, which is what makes "did we log that read"
//! answerable by reading one file rather than every route.
//!
//! # What the handler adds
//!
//! The extractor knows the actor, the tenant and the address. Only the handler knows
//! what was actually read — which resource, how many rows, what shape of query — so it
//! fills that in through [`Audit`], and the middleware writes the row once the response
//! is known.
//!
//! # A failed write does not fail the request
//!
//! An audit-log outage must not become a platform outage. It is reported where an
//! operator will see it and the response goes out regardless — the same position
//! `uops-secrets` takes for credential access, and for the same reason.

use std::net::IpAddr;
use std::sync::{Arc, Mutex};

use axum::extract::Request;
use axum::middleware::Next;
use axum::response::Response;
use uops_core::TenantId;
use uops_store_pg::{AccessEntry, AuditEntry};

use crate::state::AppState;

/// The slot the middleware owns and the request borrows.
///
/// Created per request, put into the request's extensions, filled in by the extractor
/// and then by the handler, and read back afterwards by the middleware — which kept its
/// own clone, because a response's extensions are not the request's.
#[derive(Clone, Debug, Default)]
pub(crate) struct Recorder {
    inner: Arc<Mutex<Inner>>,
}

#[derive(Debug, Default)]
struct Inner {
    /// Set by the extractor. Absent on a request that never established a scope.
    context: Option<Context>,
    detail: Detail,
}

#[derive(Clone, Debug)]
struct Context {
    tenant_id: TenantId,
    actor: String,
}

/// What only the handler can know.
#[derive(Clone, Debug, Default)]
struct Detail {
    target: Option<String>,
    fingerprint: Option<String>,
    row_count: Option<i64>,
    /// Set for a mutation. Its presence is what makes this an audit entry rather than
    /// an access entry.
    action: Option<String>,
    before: Option<serde_json::Value>,
    after: Option<serde_json::Value>,
}

impl Recorder {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Called by the extractor, which is the only thing that knows both.
    pub(crate) fn set_context(&self, tenant_id: TenantId, actor: String) {
        self.lock().context = Some(Context { tenant_id, actor });
    }
}

/// The handle a handler uses to say what it did.
///
/// Obtained from [`crate::Caller`], so a handler that never established a scope cannot
/// hold one — and a handler that did cannot avoid having somewhere to record.
#[derive(Clone, Debug, Default)]
pub struct Audit {
    recorder: Recorder,
}

impl Audit {
    pub(crate) const fn new(recorder: Recorder) -> Self {
        Self { recorder }
    }

    /// Record a read: what was looked at, and how much came back.
    ///
    /// `rows` is part of the record because "ran a query" and "exported forty thousand
    /// rows about a customer's network" are different events, and only the second is
    /// what an investigation is looking for.
    pub fn read(&self, target: impl Into<String>, rows: Option<i64>) {
        let mut inner = self.recorder.lock();
        inner.detail.target = Some(target.into());
        inner.detail.row_count = rows;
    }

    /// Record a telemetry read, keeping the query's shape.
    ///
    /// The fingerprint is the shape, never the parameters: those carry the customer's
    /// hostnames and addresses, and handing an auditor a second copy of the data is not
    /// auditing.
    pub fn read_query(&self, fingerprint: impl Into<String>, rows: Option<i64>) {
        let mut inner = self.recorder.lock();
        inner.detail.target = Some("query".to_owned());
        inner.detail.fingerprint = Some(fingerprint.into());
        inner.detail.row_count = rows;
    }

    /// Record a mutation, with what it changed.
    pub fn wrote(
        &self,
        action: impl Into<String>,
        target: impl Into<String>,
        before: Option<serde_json::Value>,
        after: Option<serde_json::Value>,
    ) {
        let mut inner = self.recorder.lock();
        inner.detail.action = Some(action.into());
        inner.detail.target = Some(target.into());
        inner.detail.before = before;
        inner.detail.after = after;
    }
}

/// The client address, as reported.
///
/// `X-Forwarded-For` first, because every deployment shape puts a reverse proxy in front
/// — TLS is terminated there, see the workspace manifest. The value is therefore only as
/// trustworthy as that proxy, and the column records an address rather than a claim
/// about one.
///
/// The socket's own peer address is the fallback, for a server exposed directly. It is
/// the *more* trustworthy of the two — nobody can forge it by setting a header — but it
/// is second, because behind a proxy every request would otherwise be attributed to the
/// proxy and the column would be uniformly useless. Header first is right where there is
/// a proxy, and the fallback is right where there is not.
///
/// It is `None` only when there is neither: a request synthesised in a test, or a
/// transport with no peer.
fn reported_ip(request: &Request) -> Option<IpAddr> {
    // The first *parseable* hop, not simply the first. A client that sends
    // `X-Forwarded-For: garbage` before a proxy that appends would otherwise erase its
    // own address from the audit log — the header is attacker-controlled and the socket
    // is not, so junk in the header must never be able to suppress the fallback.
    let forwarded = request
        .headers()
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').find_map(|hop| hop.trim().parse().ok()));

    forwarded.or_else(|| {
        request
            .extensions()
            .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
            .map(|peer| peer.0.ip())
    })
}

/// Write the row once the response is known.
///
/// Applied to the whole router rather than to a chosen list of routes. A request that
/// never established a scope leaves no context and is skipped, so wrapping everything
/// costs nothing — and cannot be forgotten when the twenty-first route is added.
pub async fn layer(
    axum::extract::State(state): axum::extract::State<AppState>,
    mut request: Request,
    next: Next,
) -> Response {
    let recorder = Recorder::default();
    let ip = reported_ip(&request);
    request.extensions_mut().insert(recorder.clone());

    let response = next.run(request).await;

    // A refused request is not a read. Recording one would fill the log with whoever is
    // probing, and bury the successful access that actually matters.
    if !response.status().is_success() {
        return response;
    }

    let (context, detail) = {
        let inner = recorder.lock();
        (inner.context.clone(), inner.detail.clone())
    };

    let (Some(context), Some(target)) = (context, detail.target) else {
        // Either no scope was established, or the handler read nothing worth recording.
        return response;
    };

    let outcome = if let Some(action) = detail.action {
        state
            .store
            .record_audit(&AuditEntry {
                tenant_id: context.tenant_id,
                actor: context.actor,
                action,
                target,
                before: detail.before,
                after: detail.after,
                ip,
            })
            .await
    } else {
        state
            .store
            .record_access(&AccessEntry {
                tenant_id: context.tenant_id,
                actor: context.actor,
                target,
                fingerprint: detail.fingerprint,
                row_count: detail.row_count,
                ip,
            })
            .await
    };

    if let Err(e) = outcome {
        // Loud, and not fatal. A logging outage must not become a platform outage, and
        // an operator needs to know the trail has a hole in it.
        eprintln!("audit write failed: {e}");
    }

    response
}

#[cfg(test)]
mod tests {
    use super::*;

    fn audit() -> (Audit, Recorder) {
        let recorder = Recorder::default();
        (Audit::new(recorder.clone()), recorder)
    }

    #[test]
    fn a_read_records_what_and_how_much() {
        let (audit, recorder) = audit();
        audit.read("resource:abc", Some(1));

        let inner = recorder.lock();
        assert_eq!(inner.detail.target.as_deref(), Some("resource:abc"));
        assert_eq!(inner.detail.row_count, Some(1));
        assert!(
            inner.detail.action.is_none(),
            "a read must not become an audit entry"
        );
    }

    #[test]
    fn a_query_records_its_shape_and_not_its_parameters() {
        // The fingerprint is what an auditor needs. The parameters are the customer's
        // hostnames and addresses, and copying those into a second table is not
        // auditing — it is making another copy of the thing being protected.
        let (audit, recorder) = audit();
        audit.read_query("logs.tail", Some(1_000));

        let inner = recorder.lock();
        assert_eq!(inner.detail.target.as_deref(), Some("query"));
        assert_eq!(inner.detail.fingerprint.as_deref(), Some("logs.tail"));
        assert_eq!(inner.detail.row_count, Some(1_000));
    }

    #[test]
    fn a_mutation_carries_before_and_after() {
        // So a change is reviewable without replaying the whole history to reach it.
        let (audit, recorder) = audit();
        audit.wrote(
            "resource.status",
            "resource:abc",
            Some(serde_json::json!({"status": "up"})),
            Some(serde_json::json!({"status": "decommissioned"})),
        );

        let inner = recorder.lock();
        assert_eq!(inner.detail.action.as_deref(), Some("resource.status"));
        assert_eq!(inner.detail.before.as_ref().unwrap()["status"], "up");
        assert_eq!(
            inner.detail.after.as_ref().unwrap()["status"],
            "decommissioned"
        );
    }

    #[test]
    fn a_handler_that_records_nothing_leaves_nothing_to_write() {
        // Not every scoped request reads customer data, and the middleware skips those
        // rather than writing an empty row for each.
        let (_audit, recorder) = audit();
        assert!(recorder.lock().detail.target.is_none());
    }

    #[test]
    fn a_request_with_no_scope_records_no_context() {
        // A request that never reached the extractor never touched tenant data, so
        // there is nothing to attribute and the middleware writes nothing.
        let recorder = Recorder::default();
        assert!(recorder.lock().context.is_none());
    }

    /// A request carrying the given header and peer address, if any.
    fn request(forwarded: Option<&str>, peer: Option<&str>) -> Request {
        let mut b = Request::builder().uri("/");
        if let Some(v) = forwarded {
            b = b.header("x-forwarded-for", v);
        }
        let mut r = b.body(axum::body::Body::empty()).unwrap();
        if let Some(p) = peer {
            r.extensions_mut().insert(axum::extract::ConnectInfo(
                p.parse::<std::net::SocketAddr>().unwrap(),
            ));
        }
        r
    }

    #[test]
    fn the_proxy_header_wins_over_the_socket() {
        // Behind a proxy the peer address is the proxy, every time, for every user.
        let r = request(Some("203.0.113.7"), Some("10.0.0.9:443"));
        assert_eq!(reported_ip(&r).unwrap().to_string(), "203.0.113.7");
    }

    #[test]
    fn the_socket_is_used_when_there_is_no_proxy() {
        // A directly exposed server. Without this the column is empty for every
        // request, which is how an audit log quietly stops being evidence.
        let r = request(None, Some("198.51.100.42:51234"));
        assert_eq!(reported_ip(&r).unwrap().to_string(), "198.51.100.42");
    }

    #[test]
    fn the_first_hop_of_a_chain_is_the_client() {
        // X-Forwarded-For accumulates left to right: client, then each proxy.
        let r = request(Some("203.0.113.7, 10.0.0.1, 10.0.0.2"), None);
        assert_eq!(reported_ip(&r).unwrap().to_string(), "203.0.113.7");
    }

    #[test]
    fn a_client_cannot_erase_its_address_with_a_junk_header() {
        // The header is attacker-controlled; the socket is not. If junk in the header
        // suppressed the fallback, anyone could keep themselves out of the audit log by
        // sending one word.
        let r = request(Some("not-an-address"), Some("10.0.0.9:443"));
        assert_eq!(reported_ip(&r).unwrap().to_string(), "10.0.0.9");
    }

    #[test]
    fn a_junk_hop_before_a_real_one_is_skipped() {
        // What a client sending a forged header looks like once a proxy appends the
        // address it actually saw.
        let r = request(Some("junk, 203.0.113.7"), Some("10.0.0.9:443"));
        assert_eq!(reported_ip(&r).unwrap().to_string(), "203.0.113.7");
    }

    #[test]
    fn neither_source_is_none_rather_than_a_guess() {
        assert_eq!(reported_ip(&request(None, None)), None);
    }
}
