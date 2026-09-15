//! `POST /api/v1/query`, through HTTP, against both databases.
//!
//! Every component below this has its own tests. What only these can settle is that the
//! chain holds when it is assembled: a session proves a role, a selector expands against
//! `PostgreSQL`, the compiler writes a tenant predicate from the scope, `ClickHouse`
//! executes it, and the access log records that it happened.
//!
//! The assertion that matters is the third one down. Everything else in this file is
//! setup for it.
//!
//! ```bash
//! docker compose -f deploy/docker-compose.yml up -d
//! bash scripts/db.sh migrate && bash scripts/ch.sh apply
//! DATABASE_URL=postgres://uops:uops@localhost:5432/uops \
//!   CLICKHOUSE_USER=uops CLICKHOUSE_PASSWORD=uops \
//!   cargo test -p uops-api --test query_route
//! ```

use std::collections::BTreeMap;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use chrono::{Duration, TimeZone, Utc};
use tower::ServiceExt as _;
use uops_api::{AppState, CSRF_COOKIE, CSRF_HEADER, SESSION_COOKIE, TENANT_HEADER};
use uops_core::{OrgId, ResourceId, Role, Secret, SiteId, TenantId};
use uops_secrets::password;
use uops_store_ch::{ChClient, ChConfig, ChStore, LogRow, LogStore};
use uops_store_pg::{Config, NewResource, PgStore};

fn telemetry() -> ChStore {
    ChStore::new(ChClient::new(ChConfig {
        user: std::env::var("CLICKHOUSE_USER").unwrap_or_else(|_| "uops".into()),
        password: std::env::var("CLICKHOUSE_PASSWORD").unwrap_or_else(|_| "uops".into()),
        ..ChConfig::from_env()
    }))
}

async fn control_plane() -> PgStore {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://uops:uops@localhost:5432/uops".into());
    PgStore::connect(&Config {
        url,
        ..Config::default()
    })
    .await
    .expect("connect")
}

/// The five-minute boundary `secs_ago` seconds in the past.
///
/// # Why these fixtures are not at a fixed instant
///
/// They used to be, at `1_700_000_000` — 2023-11-14. Every table these tests write to
/// carries a retention TTL (`logs` and the pre-aggregates 365 days, `metrics` 30), so by
/// the time this was found the fixtures were nearly three years past retention: the rows
/// were inserted and then removed by a background TTL merge. Whether a test passed
/// depended on whether that merge had run against its part yet, which made
/// `the_explorer_histogram_is_served_from_the_pre_aggregate` fail about one run in
/// fifteen with an empty result. A `SELECT` at the time of the diagnosis found 335 rows
/// still in `logs` for that window and **zero** in `logs_counts_5m` — the aggregate had
/// already been swept.
///
/// Anchored to now instead, and truncated to a five-minute boundary so
/// `toStartOfFiveMinute` puts a fixture in a predictable bucket rather than one that
/// depends on when the suite ran. Backdated far enough that the whole window is in the
/// past: a row in the future is not a retention problem but it is not a measurement
/// either.
fn bucket_aligned(secs_ago: i64) -> chrono::DateTime<Utc> {
    let secs = Utc::now().timestamp() - secs_ago;
    Utc.timestamp_opt(secs - secs.rem_euclid(300), 0)
        .single()
        .expect("a truncated unix timestamp is a valid instant")
}

/// A window the fixtures fall inside, aligned so a pre-aggregate query covers them too.
fn window() -> (chrono::DateTime<Utc>, chrono::DateTime<Utc>) {
    let start = window_start();
    (start, start + Duration::hours(1))
}

/// Where the window begins.
///
/// Computed once per process. Every call must return the same instant, because the
/// fixtures are written relative to it and then queried relative to it — if a run
/// crosses a five-minute boundary between the `INSERT` and the `SELECT`, a recomputed
/// start lands in the next bucket and the query looks in the wrong place. That is how
/// three of these tests failed on the run right after the window stopped being a
/// constant.
fn window_start() -> chrono::DateTime<Utc> {
    static START: std::sync::OnceLock<chrono::DateTime<Utc>> = std::sync::OnceLock::new();
    *START.get_or_init(|| bucket_aligned(2 * 3600))
}

/// The shortest retention any table these fixtures write to has.
///
/// `metrics` is 30 days; `logs`, `events` and the pre-aggregates are 365 or more. The
/// shortest one is what binds, because a fixture outside it is removed from that table
/// and left in the others — which is precisely the half-present state that made this
/// hard to see. Keep it in step with `ch-migrations/`.
const SHORTEST_RETENTION_DAYS: i64 = 30;

#[test]
fn the_fixtures_are_inside_every_retention_window() {
    // The guard on the bug, checked by the test suite on itself rather than by a CI
    // mutation, because the failure it prevents is not deterministic: TTL is applied by
    // a background merge on ClickHouse's own schedule, so an expired fixture passes
    // until the moment a merge lands between the INSERT and the SELECT. That is a test
    // that fails once a fortnight for a reason nobody can reproduce.
    //
    // This assertion has no such timing in it. It fails immediately, on every run, the
    // day somebody writes a fixed timestamp here again or shortens a TTL past it.
    let age = Utc::now() - window_start();
    assert!(
        age < Duration::days(SHORTEST_RETENTION_DAYS),
        "the fixtures are {} days old and the shortest retention is {SHORTEST_RETENTION_DAYS};          ClickHouse will delete them on its next TTL merge, which makes every test in this          file pass or fail depending on when that merge runs",
        age.num_days()
    );
    assert!(
        age > Duration::zero(),
        "the fixtures are in the future, which is not a measurement"
    );
}

struct Fixture {
    store: PgStore,
    telemetry: ChStore,
    tenant: TenantId,
    session: String,
    csrf: String,
}

async fn fixture(slug: &str, role: Role) -> Fixture {
    let store = control_plane().await;
    let org = OrgId::new();
    sqlx::query("INSERT INTO organization (id, name) VALUES ($1, $2)")
        .bind(org.into_uuid())
        .bind(format!("q-org-{slug}"))
        .execute(store.pool())
        .await
        .expect("organization");

    let tenant = TenantId::new();
    sqlx::query("INSERT INTO tenant (id, org_id, name, slug) VALUES ($1, $2, $3, $4)")
        .bind(tenant.into_uuid())
        .bind(org.into_uuid())
        .bind(format!("q-{slug}"))
        .bind(format!("{slug}-{}", tenant.into_uuid().simple()))
        .execute(store.pool())
        .await
        .expect("tenant");

    let email = format!("{slug}-{}@example.com", tenant.into_uuid().simple());
    let hash = password::hash(&Secret::new("pw".to_owned())).unwrap();
    let user = store
        .create_user(org, &email, "Query Test", &hash)
        .await
        .expect("user");
    store
        .grant_role(user, tenant, role, None)
        .await
        .expect("role");

    let (session, csrf) = sign_in(&store, &email).await;
    Fixture {
        store,
        telemetry: telemetry(),
        tenant,
        session,
        csrf,
    }
}

fn app(f: &Fixture) -> Router {
    uops_api::router(AppState::new(f.store.clone(), f.telemetry.clone()))
}

fn app_for(store: &PgStore) -> Router {
    uops_api::router(AppState::new(store.clone(), telemetry()))
}

async fn sign_in(store: &PgStore, email: &str) -> (String, String) {
    let response = app_for(store)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/auth/login")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({ "email": email, "password": "pw" }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    let mut session = String::new();
    let mut csrf = String::new();
    for value in response.headers().get_all(header::SET_COOKIE) {
        let text = value.to_str().unwrap();
        let (pair, _) = text.split_once("; ").unwrap();
        let (name, v) = pair.split_once('=').unwrap();
        if name == SESSION_COOKIE {
            v.clone_into(&mut session);
        } else if name == CSRF_COOKIE {
            v.clone_into(&mut csrf);
        }
    }
    (session, csrf)
}

/// One log line for a tenant, inserted straight into `ClickHouse` the way the
/// pipeline will.
fn log_row(tenant: TenantId, resource: ResourceId, body: &str, offset: i64) -> LogRow {
    let at = window().0 + Duration::seconds(offset);
    let mut attributes = BTreeMap::new();
    attributes.insert("host.name".to_owned(), "rtr-01".to_owned());

    LogRow {
        tenant_id: tenant,
        resource_id: resource,
        site_id: SiteId::nil(),
        observed_at: at,
        ingested_at: at,
        source_kind: "syslog".to_owned(),
        source_vendor: "cisco".to_owned(),
        severity: "error".to_owned(),
        facility: 23,
        body: body.to_owned(),
        attributes,
        trace_id: String::new(),
        span_id: String::new(),
    }
}

/// The query body the UI would post, with `extra` merged over the defaults.
fn ast(extra: &serde_json::Value) -> serde_json::Value {
    let (start, end) = window();
    let mut body = serde_json::json!({
        "signal": "log",
        "time": { "start": start.to_rfc3339(), "end": end.to_rfc3339() },
        "resources": { "type": "all" },
        "limit": 100
    });
    if let (Some(base), Some(more)) = (body.as_object_mut(), extra.as_object()) {
        for (k, v) in more {
            base.insert(k.clone(), v.clone());
        }
    }
    body
}

impl Fixture {
    fn post_query(&self, body: &serde_json::Value, with_csrf: bool) -> Request<Body> {
        let mut builder = Request::builder()
            .method("POST")
            .uri("/api/v1/query")
            .header(
                header::COOKIE,
                format!(
                    "{SESSION_COOKIE}={}; {CSRF_COOKIE}={}",
                    self.session, self.csrf
                ),
            )
            .header(TENANT_HEADER, self.tenant.to_string())
            .header(header::CONTENT_TYPE, "application/json");
        if with_csrf {
            builder = builder.header(CSRF_HEADER, &self.csrf);
        }
        builder.body(Body::from(body.to_string())).unwrap()
    }

    async fn call(&self, request: Request<Body>) -> (StatusCode, serde_json::Value) {
        let response = app(self).oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 22)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    async fn access_log(&self) -> Vec<(String, Option<String>, Option<i64>)> {
        self.store
            .access_entries(self.tenant, 50)
            .await
            .unwrap()
            .into_iter()
            .map(|e| (e.target, e.fingerprint, e.row_count))
            .collect()
    }
}

#[tokio::test]
async fn a_query_returns_this_tenants_telemetry() {
    let f = fixture("basic", Role::Viewer).await;
    f.telemetry
        .insert_logs(&[
            log_row(f.tenant, ResourceId::new(), "link down", 10),
            log_row(f.tenant, ResourceId::new(), "link up", 20),
        ])
        .await
        .unwrap();

    let (status, body) = f
        .call(f.post_query(&ast(&serde_json::json!({})), true))
        .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["rows"].as_array().unwrap().len(), 2);
    assert_eq!(body["table"], "logs");
    // The column types travel with the result: a client guessing from the JSON gets
    // DateTime64 wrong, because it arrives as a string.
    assert!(
        body["columns"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["name"] == "observed_at"),
        "{body}"
    );
}

#[tokio::test]
async fn a_query_cannot_reach_another_tenants_telemetry() {
    // THE test this whole stack exists to pass. A real session, a real role, a real
    // query — and the other customer's rows are not in the answer, because the tenant
    // predicate was written by the compiler from the scope the extractor produced.
    //
    // There is no point in the chain where a handler could have forgotten it: a handler
    // cannot reach telemetry without a scope, and cannot obtain a scope without the
    // extractor having proved a role on that tenant.
    let mine = fixture("iso-mine", Role::Viewer).await;
    let theirs = fixture("iso-theirs", Role::Viewer).await;

    mine.telemetry
        .insert_logs(&[
            log_row(mine.tenant, ResourceId::new(), "mine", 30),
            log_row(theirs.tenant, ResourceId::new(), "theirs", 30),
        ])
        .await
        .unwrap();

    let (status, body) = mine
        .call(mine.post_query(&ast(&serde_json::json!({})), true))
        .await;

    assert_eq!(status, StatusCode::OK);
    let rows = body["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "{body}");

    // And the other tenant's row genuinely exists. Without this the test would pass
    // just as happily if the second insert had silently failed — which is the shape of
    // a tenant-isolation test that proves nothing.
    let (status, theirs_body) = theirs
        .call(theirs.post_query(&ast(&serde_json::json!({})), true))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        theirs_body["rows"].as_array().unwrap().len(),
        1,
        "{theirs_body}"
    );
    assert!(theirs_body.to_string().contains("theirs"));
    let text = body.to_string();
    assert!(
        !text.contains("theirs"),
        "another tenant's row came back: {text}"
    );
    assert!(
        !text.contains(&theirs.tenant.to_string()),
        "another tenant's id came back: {text}"
    );
}

#[tokio::test]
async fn a_selector_is_expanded_against_the_control_plane() {
    // The seam between the two databases: a site exists only in PostgreSQL, and the
    // resources at it become the resource_id predicate of a ClickHouse query.
    let f = fixture("selector", Role::Viewer).await;

    let site = SiteId::new();
    sqlx::query("INSERT INTO site (id, tenant_id, name) VALUES ($1, $2, $3)")
        .bind(site.into_uuid())
        .bind(f.tenant.into_uuid())
        .bind("Dhaka DC")
        .execute(f.store.pool())
        .await
        .expect("site");

    let scope = uops_core::TenantScope::system(f.tenant);
    let mut at_site = NewResource::new(uops_core::ResourceKind::Device, "rtr-01");
    at_site.site_id = Some(site);
    let wanted = f.store.create_resource(&scope, &at_site).await.unwrap();
    let elsewhere = f
        .store
        .create_resource(
            &scope,
            &NewResource::new(uops_core::ResourceKind::Device, "rtr-02"),
        )
        .await
        .unwrap();

    f.telemetry
        .insert_logs(&[
            log_row(f.tenant, wanted.id, "from the site", 40),
            log_row(f.tenant, elsewhere.id, "from elsewhere", 40),
        ])
        .await
        .unwrap();

    let (status, body) = f
        .call(f.post_query(
            &ast(&serde_json::json!({
                "resources": { "type": "site", "site": site.to_string() }
            })),
            true,
        ))
        .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["rows"].as_array().unwrap().len(), 1, "{body}");
    assert!(body.to_string().contains("from the site"));
}

#[tokio::test]
async fn a_query_is_recorded_as_a_read_with_its_shape_and_not_its_terms() {
    // SPEC §M0.8: defence and law-enforcement buyers audit who SAW what. The fingerprint
    // has to be enough to recognise a pattern of access and not enough to reconstruct
    // the customer's data — putting the search terms in the audit table would be making
    // a second copy of the thing being protected.
    let f = fixture("audit", Role::Viewer).await;
    f.telemetry
        .insert_logs(&[log_row(
            f.tenant,
            ResourceId::new(),
            "a secret hostname",
            50,
        )])
        .await
        .unwrap();

    let (status, _) = f
        .call(f.post_query(
            &ast(&serde_json::json!({
                "filter": {
                    "op": "text",
                    "field": { "field": "body" },
                    "mode": "any_token",
                    "terms": ["secret"]
                }
            })),
            true,
        ))
        .await;
    assert_eq!(status, StatusCode::OK);

    let log = f.access_log().await;
    let entry = log
        .iter()
        .find(|(target, _, _)| target == "query")
        .expect("the query must be recorded as a read");

    assert_eq!(entry.1.as_deref(), Some("log:logs+filter"));
    assert_eq!(entry.2, Some(1), "how much came back is part of the record");
    assert!(
        !format!("{log:?}").contains("secret"),
        "the search term must not reach the audit log: {log:?}"
    );
}

#[tokio::test]
async fn a_slow_query_still_answers_and_says_it_was_slow() {
    // The warning survives compilation, execution and serialisation, and arrives where
    // the UI can show it before somebody waits two seconds wondering.
    let f = fixture("warn", Role::Viewer).await;
    f.telemetry
        .insert_logs(&[log_row(f.tenant, ResourceId::new(), "interface reset", 60)])
        .await
        .unwrap();

    let (status, body) = f
        .call(f.post_query(
            &ast(&serde_json::json!({
                "filter": {
                    "op": "text",
                    "field": { "field": "body" },
                    "mode": "substring",
                    "terms": ["terface res"]
                }
            })),
            true,
        ))
        .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["rows"].as_array().unwrap().len(), 1);
    let warnings = body["warnings"].as_array().unwrap();
    assert!(
        warnings
            .iter()
            .any(|w| w["warning"] == "not_index_accelerated"),
        "{body}"
    );
}

#[tokio::test]
async fn a_query_without_the_csrf_header_is_refused() {
    // A read expressed as a POST is still a POST. Exempting it would create the one
    // endpoint whose protection is a special case somebody has to remember.
    let f = fixture("csrf", Role::Viewer).await;
    let (status, _) = f
        .call(f.post_query(&ast(&serde_json::json!({})), false))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn a_query_the_compiler_refuses_is_the_callers_fault() {
    // Trace queries are declared in the AST and unimplemented until M8. The caller gets
    // a 400 explaining it, not a 500 blaming the database for a missing table.
    let f = fixture("refused", Role::Viewer).await;
    let (status, problem) = f
        .call(f.post_query(&ast(&serde_json::json!({ "signal": "trace" })), true))
        .await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "{problem}");
    assert_eq!(problem["type"], "invalid-input");
}

#[tokio::test]
async fn a_viewer_may_query_because_it_changes_nothing() {
    // SPEC §M1's RBAC table: reading telemetry is a viewer's whole job. Requiring
    // operator here would make the product useless to the role it was designed for.
    let f = fixture("viewer", Role::Viewer).await;
    let (status, _) = f
        .call(f.post_query(&ast(&serde_json::json!({})), true))
        .await;
    assert_eq!(status, StatusCode::OK);
}
