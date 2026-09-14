//! The first run, against a real and genuinely empty PostgreSQL.
//!
//! Every other integration test in this crate shares one database and stays out of the
//! others' way by creating its own organization. Bootstrap cannot: its whole decision is
//! "does *any* user exist", which is a property of the entire installation. Testing it
//! against the shared database would mean testing it against a database that always has
//! users, where the only reachable answer is `None`.
//!
//! So each test here creates a database, migrates it, and drops it. That is slow — a
//! second or so each — and it is the only way to exercise the branch that matters.

use uops_core::{OrgId, Role};
use uops_secrets::{generate, password};
use uops_store_pg::{Config, FirstRunRequest, PgStore};
use uuid::Uuid;

fn admin_url() -> String {
    std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://uops:uops@localhost:5432/uops".into())
}

/// A migrated, empty database, and its name so the caller can drop it.
struct Scratch {
    store: PgStore,
    name: String,
}

impl Scratch {
    async fn new() -> Self {
        // Not a connection to the scratch database — CREATE DATABASE cannot run inside a
        // transaction and needs some other database to be connected to.
        let admin = PgStore::connect(&Config {
            url: admin_url(),
            ..Config::default()
        })
        .await
        .expect("connect to admin database");

        // Any fresh uuid will do; OrgId is simply the one this workspace already has.
        let name = format!("uops_boot_{}", OrgId::new().into_uuid().simple());
        // Interpolated, not bound: CREATE DATABASE takes an identifier, and identifiers
        // cannot be parameters. The name is a literal prefix plus a UUID with the hyphens
        // removed, so there is nothing here an attacker could reach even in principle.
        sqlx::query(&format!(r#"CREATE DATABASE "{name}""#))
            .execute(admin.pool())
            .await
            .expect("create scratch database");

        let url = admin_url();
        let base = url.rsplit_once('/').expect("database url has a path").0;
        let store = PgStore::connect(&Config {
            url: format!("{base}/{name}"),
            ..Config::default()
        })
        .await
        .expect("connect to scratch database");

        sqlx::migrate!("../../migrations")
            .run(store.pool())
            .await
            .expect("migrate scratch database");

        Self { store, name }
    }

    /// Dropped explicitly rather than in `Drop`, which cannot await. A test that panics
    /// leaves its database behind; `db.sh reset` clears them, and a stray empty database
    /// is a better failure than a test that hangs trying to drop one.
    async fn drop_database(self) {
        let Self { store, name } = self;
        store.pool().close().await;

        let admin = PgStore::connect(&Config {
            url: admin_url(),
            ..Config::default()
        })
        .await
        .expect("connect to admin database");
        sqlx::query(&format!(r#"DROP DATABASE IF EXISTS "{name}" WITH (FORCE)"#))
            .execute(admin.pool())
            .await
            .expect("drop scratch database");
    }
}

fn request(hash: &uops_secrets::PasswordHashString) -> FirstRunRequest<'_> {
    FirstRunRequest {
        org_name: "Acme",
        tenant_name: "Production",
        tenant_slug: "production",
        email: "admin@example.invalid",
        display_name: "Administrator",
        password_hash: hash,
    }
}

#[tokio::test]
async fn an_empty_database_gets_an_admin_who_can_immediately_log_in() {
    let scratch = Scratch::new().await;

    assert!(
        !scratch.store.any_user_exists().await.unwrap(),
        "a freshly migrated database must have no users"
    );

    let plaintext = generate::password().unwrap();
    let hash = password::hash(&plaintext).unwrap();
    let first = scratch
        .store
        .bootstrap_first_run(&request(&hash))
        .await
        .unwrap()
        .expect("an empty database must bootstrap");

    // The point of the whole exercise: the credential printed on the terminal is one
    // the login handler will accept. A bootstrap that produced an account nobody could
    // log into would pass every structural assertion and be completely useless.
    let creds = scratch
        .store
        .user_credentials_by_email("admin@example.invalid")
        .await
        .unwrap()
        .expect("the bootstrapped user must be findable by email");
    assert_eq!(creds.user_id, first.user);
    assert!(
        password::verify(&plaintext, &creds.password_hash),
        "the generated password must verify against the stored hash"
    );

    // And admin on the tenant that was created alongside them, or they can administer
    // nothing.
    assert_eq!(
        scratch
            .store
            .role_for(first.user, first.tenant)
            .await
            .unwrap(),
        Some(Role::Admin)
    );

    scratch.drop_database().await;
}

#[tokio::test]
async fn a_second_run_declines_and_changes_nothing() {
    let scratch = Scratch::new().await;

    let first_hash = password::hash(&generate::password().unwrap()).unwrap();
    let first = scratch
        .store
        .bootstrap_first_run(&request(&first_hash))
        .await
        .unwrap()
        .expect("first run");

    // A different password, to catch the worst possible bug: a second boot silently
    // resetting the admin's credential to something printed in a log nobody read.
    let second_plaintext = generate::password().unwrap();
    let second_hash = password::hash(&second_plaintext).unwrap();
    let again = scratch
        .store
        .bootstrap_first_run(&request(&second_hash))
        .await
        .unwrap();
    assert!(again.is_none(), "a second run must decline");

    let creds = scratch
        .store
        .user_credentials_by_email("admin@example.invalid")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(creds.user_id, first.user, "the admin must not be replaced");
    assert!(
        !password::verify(&second_plaintext, &creds.password_hash),
        "the declined run must not have overwritten the password"
    );

    scratch.drop_database().await;
}

#[tokio::test]
async fn concurrent_first_runs_produce_exactly_one_admin() {
    let scratch = Scratch::new().await;

    // Four replicas of the same server starting at the same second, which is what a
    // container orchestrator does. Without the advisory lock all four see an empty
    // app_user, all four insert, and the operator is shown four passwords of which
    // three are for accounts they will never find again.
    let hashes: Vec<_> = (0..4)
        .map(|_| password::hash(&generate::password().unwrap()).unwrap())
        .collect();
    // tokio::join! rather than four spawned tasks: the futures only need to be polled
    // concurrently, and every one of them blocks inside PostgreSQL waiting on the
    // advisory lock, which is an await point.
    let reqs: Vec<_> = hashes.iter().map(request).collect();
    let (a, b, c, d) = tokio::join!(
        scratch.store.bootstrap_first_run(&reqs[0]),
        scratch.store.bootstrap_first_run(&reqs[1]),
        scratch.store.bootstrap_first_run(&reqs[2]),
        scratch.store.bootstrap_first_run(&reqs[3])
    );

    let winners: Vec<_> = [a, b, c, d]
        .into_iter()
        .filter_map(|r| r.expect("no concurrent run may error"))
        .collect();
    assert_eq!(winners.len(), 1, "exactly one concurrent run may succeed");

    let users: i64 = sqlx::query_scalar("SELECT count(*) FROM app_user")
        .fetch_one(scratch.store.pool())
        .await
        .unwrap();
    assert_eq!(users, 1, "and it must leave exactly one user behind");

    scratch.drop_database().await;
}

#[tokio::test]
async fn a_failed_bootstrap_leaves_no_half_built_organization() {
    let scratch = Scratch::new().await;

    // A NUL byte in the email, which PostgreSQL rejects outright — the shape a value
    // arriving from a truncated environment variable or a mangled config file takes.
    // It fails on the third statement, after the organization and the tenant have
    // already been inserted.
    //
    // Without one transaction that leaves an org and a tenant with no administrator,
    // and — because the next boot still finds no users — the boot after that builds a
    // second org beside the orphan, and so on forever.
    let hash = password::hash(&generate::password().unwrap()).unwrap();
    let bad = FirstRunRequest {
        email: "admin @example.invalid",
        ..request(&hash)
    };
    let outcome = scratch.store.bootstrap_first_run(&bad).await;
    assert!(
        outcome.is_err(),
        "a NUL byte in an email must not be accepted"
    );

    for table in ["organization", "tenant", "app_user", "user_tenant_role"] {
        let rows: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
            .fetch_one(scratch.store.pool())
            .await
            .unwrap();
        assert_eq!(rows, 0, "{table} must be empty after a failed bootstrap");
    }

    // And the installation is still bootstrappable, which is the part that matters.
    let hash = password::hash(&generate::password().unwrap()).unwrap();
    assert!(
        scratch
            .store
            .bootstrap_first_run(&request(&hash))
            .await
            .unwrap()
            .is_some(),
        "a failed bootstrap must not poison the next one"
    );

    scratch.drop_database().await;
}

#[tokio::test]
async fn the_bootstrap_admin_role_names_no_granting_user() {
    let scratch = Scratch::new().await;

    let hash = password::hash(&generate::password().unwrap()).unwrap();
    let first = scratch
        .store
        .bootstrap_first_run(&request(&hash))
        .await
        .unwrap()
        .unwrap();

    // Nobody granted this role. Recording the admin as having granted it to themselves
    // would put a decision in the audit trail that no human made.
    let granted_by: Option<Uuid> =
        sqlx::query_scalar("SELECT granted_by FROM user_tenant_role WHERE user_id = $1")
            .bind(first.user.into_uuid())
            .fetch_one(scratch.store.pool())
            .await
            .unwrap();
    assert_eq!(granted_by, None);

    scratch.drop_database().await;
}
