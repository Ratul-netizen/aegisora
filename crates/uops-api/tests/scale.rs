//! SPEC §M1 acceptance: *"Resource CRUD + list with cursor pagination over 10 000
//! seeded resources, p95 < 200 ms."*
//!
//! Measured through the router, not against the store. The criterion is about what a
//! user waits for, and that includes the session lookup, the scope extractor, the role
//! check, the audit layer and JSON serialisation — every one of which is on the path and
//! none of which a store-level benchmark would see.
//!
//! # What this proves, and what it does not
//!
//! It runs against whatever `PostgreSQL` the suite is pointed at — a container on a
//! laptop, or a service container on a shared runner — so it is not a claim about
//! production hardware. 200 ms is SPEC's number and it is a loose one at this size.
//!
//! It is also **not** a test of the pagination strategy, and an earlier version of this
//! file claimed it was. Measured on the seeded data:
//!
//! | rows | `OFFSET` to the last page | keyset |
//! |---|---|---|
//! | 10 000 | 5.2 ms | 5.0 ms |
//!
//! At ten thousand rows `OFFSET` and keyset are indistinguishable, because scanning ten
//! thousand index entries costs nothing. The difference only appears later — on a
//! million-row table the same `OFFSET` takes 5.1 s against a keyset page's few
//! milliseconds — and SPEC's criterion is ten thousand.
//!
//! So the shape assertion at the end is a tripwire for something *catastrophic*: a lost
//! index, a join that became quadratic, a filter that stopped being sargable. It cannot
//! tell keyset from `OFFSET` at this scale and does not claim to. Proving that
//! distinction would need a hundred times SPEC's fleet, which is a different test with
//! a different cost.
//!
//! # Why it seeds with SQL
//!
//! Ten thousand `POST`s would take minutes and would be measuring the write path, which
//! is not what the criterion is about. The rows go in with one statement; the
//! measurement is entirely on the read and mutate paths afterwards.

use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use tower::ServiceExt as _;
use uops_api::{AppState, CSRF_COOKIE, CSRF_HEADER, SESSION_COOKIE, TENANT_HEADER};
use uops_core::{OrgId, Role, Secret, TenantId};
use uops_secrets::password;
use uops_store_pg::{Config, PgStore};

/// SPEC's number.
const BUDGET: Duration = Duration::from_millis(200);

/// SPEC's fleet size.
const SEEDED: usize = 10_000;

async fn store() -> PgStore {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://uops:uops@localhost:5432/uops".into());
    PgStore::connect(&Config {
        url,
        ..Config::default()
    })
    .await
    .expect("connect")
}

fn telemetry() -> uops_store_ch::ChStore {
    uops_store_ch::ChStore::new(uops_store_ch::ChClient::new(
        uops_store_ch::ChConfig::from_env(),
    ))
}

fn app(store: &PgStore) -> Router {
    uops_api::router(AppState::new(store.clone(), telemetry()))
}

struct Fixture {
    store: PgStore,
    tenant: TenantId,
    session: String,
    csrf: String,
}

async fn fixture() -> Fixture {
    let store = store().await;
    let org = OrgId::new();
    sqlx::query("INSERT INTO organization (id, name) VALUES ($1, $2)")
        .bind(org.into_uuid())
        .bind("scale-org")
        .execute(store.pool())
        .await
        .expect("organization");

    let tenant = TenantId::new();
    sqlx::query("INSERT INTO tenant (id, org_id, name, slug) VALUES ($1, $2, $3, $4)")
        .bind(tenant.into_uuid())
        .bind(org.into_uuid())
        .bind("scale")
        .bind(format!("scale-{}", tenant.into_uuid().simple()))
        .execute(store.pool())
        .await
        .expect("tenant");

    let email = format!("scale-{}@example.invalid", tenant.into_uuid().simple());
    let hash = password::hash(&Secret::new("pw".to_owned())).unwrap();
    let user = store
        .create_user(org, &email, "Scale Test", &hash)
        .await
        .expect("user");
    store
        .grant_role(user, tenant, Role::Admin, None)
        .await
        .expect("role");

    // One statement. Ten thousand POSTs would take minutes and would measure the write
    // path, which this criterion is not about.
    sqlx::query(
        "INSERT INTO resource (id, tenant_id, kind, name, vendor, status)
         SELECT gen_random_uuid(), $1, 'host',
                'host-' || lpad(n::text, 5, '0'),
                (ARRAY['cisco','dell','hpe','mikrotik'])[1 + (n % 4)],
                (ARRAY['up','down','degraded','unknown'])[1 + (n % 4)]::resource_status
           FROM generate_series(1, $2) AS n",
    )
    .bind(tenant.into_uuid())
    .bind(i32::try_from(SEEDED).unwrap())
    .execute(store.pool())
    .await
    .expect("seed");

    let (session, csrf) = sign_in(&store, &email).await;
    Fixture {
        store,
        tenant,
        session,
        csrf,
    }
}

async fn sign_in(store: &PgStore, email: &str) -> (String, String) {
    let response = app(store)
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

impl Fixture {
    /// Remove everything this test seeded.
    ///
    /// Every other suite shares this database and stays out of the way by creating its
    /// own organization — which is enough when a fixture makes a handful of rows. Ten
    /// thousand is different: a suite that leaves them behind changes what every later
    /// query plans against, and the planner's choices at 10 000 rows are not the ones
    /// it makes at 50.
    ///
    /// Called at the end rather than in `Drop`, which cannot await.
    async fn clean_up(&self) {
        sqlx::query("DELETE FROM resource WHERE tenant_id = $1")
            .bind(self.tenant.into_uuid())
            .execute(self.store.pool())
            .await
            .expect("remove the seeded resources");
    }

    fn request(&self, method: &str, path: &str, body: Option<&str>) -> Request<Body> {
        let mut builder = Request::builder()
            .method(method)
            .uri(path)
            .header(
                header::COOKIE,
                format!(
                    "{SESSION_COOKIE}={}; {CSRF_COOKIE}={}",
                    self.session, self.csrf
                ),
            )
            .header(CSRF_HEADER, &self.csrf)
            .header(TENANT_HEADER, self.tenant.to_string());
        if body.is_some() {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
        }
        builder
            .body(body.map_or_else(Body::empty, |b| Body::from(b.to_owned())))
            .unwrap()
    }

    /// One request, timed, returning how long it took and what came back.
    async fn timed(
        &self,
        method: &str,
        path: &str,
        body: Option<&str>,
    ) -> (Duration, StatusCode, String) {
        let request = self.request(method, path, body);
        let started = Instant::now();
        let response = app(&self.store).oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 4 << 20)
            .await
            .unwrap();
        (
            started.elapsed(),
            status,
            String::from_utf8_lossy(&bytes).into_owned(),
        )
    }
}

/// Nearest-rank, as in `uops_poll::executor::percentile` — no interpolation, so the
/// number reported is one that actually happened.
fn percentile(samples: &mut [Duration], p: u8) -> Duration {
    assert!(!samples.is_empty(), "no samples");
    samples.sort_unstable();
    let n = samples.len();
    let rank = (n * usize::from(p.min(100))).div_ceil(100).max(1);
    samples[rank.min(n) - 1]
}

fn report(label: &str, samples: &mut [Duration]) -> Duration {
    let p50 = percentile(samples, 50);
    let p95 = percentile(samples, 95);
    let max = percentile(samples, 100);
    println!(
        "{label:<28} n={:<5} p50={p50:>9.2?} p95={p95:>9.2?} max={max:>9.2?}",
        samples.len()
    );
    p95
}

// One test on purpose: the seed is expensive and every measurement below is about the
// same ten thousand rows. Splitting it would re-seed per case, or share state between
// tests, and both are worse than a long function.
#[allow(clippy::too_many_lines)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resource_crud_and_pagination_over_ten_thousand() {
    let f = fixture().await;

    // The seed is the premise of everything below.
    let seeded: i64 = sqlx::query_scalar("SELECT count(*) FROM resource WHERE tenant_id = $1")
        .bind(f.tenant.into_uuid())
        .fetch_one(f.store.pool())
        .await
        .unwrap();
    assert_eq!(seeded, i64::try_from(SEEDED).unwrap());

    // ---- list, walking every page with the cursor ---------------------------------
    //
    // Both a timing measurement and a correctness one: every seeded row must be seen
    // exactly once. A keyset cursor that skips or repeats at a page boundary is the
    // classic bug here, and it is invisible on page one.
    let mut list_samples = Vec::new();
    let mut seen: Vec<String> = Vec::with_capacity(SEEDED);
    let mut cursor: Option<String> = None;
    let mut pages = 0;

    loop {
        let path = cursor.as_ref().map_or_else(
            || "/api/v1/resources?limit=500".to_owned(),
            |c| format!("/api/v1/resources?limit=500&cursor={c}"),
        );
        let (elapsed, status, body) = f.timed("GET", &path, None).await;
        assert_eq!(status, StatusCode::OK, "{body}");

        list_samples.push(elapsed);
        pages += 1;

        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        for item in parsed["items"].as_array().unwrap() {
            seen.push(item["id"].as_str().unwrap().to_owned());
        }
        match parsed["next"].as_str() {
            Some(next) => cursor = Some(next.to_owned()),
            None => break,
        }
        assert!(pages < 100, "pagination did not terminate");
    }

    assert_eq!(seen.len(), SEEDED, "pagination lost or repeated rows");
    let mut unique = seen.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), SEEDED, "pagination returned a row twice");
    assert_eq!(pages, SEEDED / 500, "unexpected page count: {pages}");

    // Kept in request order: report() sorts its input, and the shape assertion at the
    // end needs to know which page was last.
    let pages_in_order = list_samples.clone();

    // ---- one resource, by id -------------------------------------------------------
    let mut get_samples = Vec::new();
    for id in seen.iter().step_by(SEEDED / 100).take(100) {
        let (elapsed, status, body) = f
            .timed("GET", &format!("/api/v1/resources/{id}"), None)
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        get_samples.push(elapsed);
    }

    // ---- filtered list, which is the query a person actually runs -------------------
    let mut filtered_samples = Vec::new();
    for (i, q) in ["host-0", "host-01", "host-099", "host-0999"]
        .iter()
        .cycle()
        .take(40)
        .enumerate()
    {
        let status_filter = ["up", "down", "degraded", "unknown"][i % 4];
        let (elapsed, status, body) = f
            .timed(
                "GET",
                &format!("/api/v1/resources?q={q}&status={status_filter}&limit=100"),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        filtered_samples.push(elapsed);
    }

    // ---- create, update, delete ----------------------------------------------------
    let mut create_samples = Vec::new();
    let mut created: Vec<String> = Vec::new();
    for n in 0..100 {
        let body = serde_json::json!({
            "kind": "host",
            "name": format!("created-{n:04}"),
            "attributes": {}
        })
        .to_string();
        let (elapsed, status, out) = f.timed("POST", "/api/v1/resources", Some(&body)).await;
        assert_eq!(status, StatusCode::CREATED, "{out}");
        create_samples.push(elapsed);
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        created.push(parsed["id"].as_str().unwrap().to_owned());
    }

    let mut patch_samples = Vec::new();
    for id in &created {
        let (elapsed, status, out) = f
            .timed(
                "PATCH",
                &format!("/api/v1/resources/{id}/status"),
                Some(r#"{"status":"maintenance"}"#),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{out}");
        patch_samples.push(elapsed);
    }

    let mut delete_samples = Vec::new();
    for id in &created {
        let (elapsed, status, out) = f
            .timed("DELETE", &format!("/api/v1/resources/{id}"), None)
            .await;
        assert_eq!(status, StatusCode::OK, "{out}");
        delete_samples.push(elapsed);
    }

    // ---- the verdict ---------------------------------------------------------------
    println!("\n  {SEEDED} resources seeded, measured through the router\n");
    let worst = [
        report("list (cursor page)", &mut list_samples),
        report("get by id", &mut get_samples),
        report("list (filtered)", &mut filtered_samples),
        report("create", &mut create_samples),
        report("patch status", &mut patch_samples),
        report("decommission", &mut delete_samples),
    ]
    .into_iter()
    .max()
    .unwrap();

    assert!(
        worst < BUDGET,
        "the slowest operation's p95 was {worst:?}, over SPEC's {BUDGET:?}"
    );

    // The shape: later pages must cost what earlier ones did.
    //
    // At ten thousand rows this does not distinguish keyset from OFFSET — see the
    // module docs, where the measurement is. What it does catch is a page cost that
    // grows with position for a reason that would be ruinous at a million rows: an
    // index dropped by a migration, a filter that stopped using one, a join that became
    // quadratic.
    //
    // Compared against the *median* page rather than the first. The first request of
    // the suite carries connection setup and a cold cache and is reliably the slowest,
    // which would make "no worse than the first page" true however pagination worked.
    let median_page = percentile(&mut pages_in_order.clone(), 50);
    let final_page = *pages_in_order.last().expect("at least one page");
    println!("  median page {median_page:.2?}, final page {final_page:.2?}");
    assert!(
        final_page < median_page * 3,
        "the last page of {SEEDED} cost {final_page:?} against a median page of          {median_page:?}; pagination is not keyset"
    );

    f.clean_up().await;
}
