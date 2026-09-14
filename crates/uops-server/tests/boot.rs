//! Booting an empty installation and logging into it over a real socket.
//!
//! Every layer below this has its own tests. What none of them can assert is the thing
//! an operator actually does on day one: read a password off a terminal and type it into
//! a login form. A first run that creates an account nobody can authenticate as passes
//! every structural check in `uops-store-pg` and leaves an installation nobody can enter.
//!
//! So this test does the whole sequence — migrate, bootstrap, serve on a real port, and
//! `POST /api/v1/auth/login` with the generated credential. It does not go through
//! `main.rs`, which is process-shaped and would want a subprocess to test; it goes
//! through the same functions in the same order.
//!
//! Like the bootstrap tests in `uops-store-pg`, each case gets a database of its own:
//! "is this installation empty" is a property of the whole installation, and the shared
//! test database is one where the answer is always no.

use uops_api::AppState;
use uops_api::routes::router;
use uops_core::OrgId;
use uops_server::config::FirstRunNames;
use uops_server::firstrun;
use uops_store_ch::{ChClient, ChConfig, ChStore};
use uops_store_pg::{Config as PgConfig, PgStore};

fn admin_url() -> String {
    std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://uops:uops@localhost:5432/uops".into())
}

fn names() -> FirstRunNames {
    FirstRunNames {
        org: "Acme".into(),
        tenant: "Production".into(),
        tenant_slug: "production".into(),
        admin_email: "admin@example.invalid".into(),
        admin_name: "Administrator".into(),
    }
}

struct Scratch {
    store: PgStore,
    name: String,
}

impl Scratch {
    async fn new() -> Self {
        let admin = PgStore::connect(&PgConfig {
            url: admin_url(),
            ..PgConfig::default()
        })
        .await
        .expect("connect to admin database");

        let name = format!("uops_boot_{}", OrgId::new().into_uuid().simple());
        // An identifier, which cannot be a bind parameter. A literal prefix and a uuid.
        sqlx::query(&format!(r#"CREATE DATABASE "{name}""#))
            .execute(admin.pool())
            .await
            .expect("create scratch database");

        let base = admin_url()
            .rsplit_once('/')
            .expect("database url has a path")
            .0
            .to_owned();
        let store = PgStore::connect(&PgConfig {
            url: format!("{base}/{name}"),
            ..PgConfig::default()
        })
        .await
        .expect("connect to scratch database");

        sqlx::migrate!("../../migrations")
            .run(store.pool())
            .await
            .expect("migrate scratch database");

        Self { store, name }
    }

    async fn drop_database(self) {
        let Self { store, name } = self;
        store.pool().close().await;
        let admin = PgStore::connect(&PgConfig {
            url: admin_url(),
            ..PgConfig::default()
        })
        .await
        .expect("connect to admin database");
        sqlx::query(&format!(r#"DROP DATABASE IF EXISTS "{name}" WITH (FORCE)"#))
            .execute(admin.pool())
            .await
            .expect("drop scratch database");
    }
}

fn telemetry() -> ChStore {
    ChStore::new(ChClient::new(ChConfig::from_env()))
}

/// Serve the router on an ephemeral port and return its address.
///
/// Port 0, so parallel tests cannot collide on a hard-coded one, and so nothing here
/// depends on a port being free on the machine it runs on.
async fn serve(state: AppState) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind an ephemeral port");
    let addr = listener.local_addr().expect("local address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, router(state)).await;
    });
    addr
}

#[tokio::test]
async fn the_printed_password_logs_in() {
    let scratch = Scratch::new().await;
    let names = names();

    let password = firstrun::bootstrap(&scratch.store, &names)
        .await
        .expect("bootstrap")
        .expect("an empty installation must bootstrap");

    let addr = serve(AppState::new(scratch.store.clone(), telemetry())).await;

    // Exactly what the operator does: the address from the banner, the password from
    // the banner. `expose` is the test standing in for their keyboard.
    let body = serde_json::json!({
        "email": names.admin_email,
        "password": password.expose(),
    })
    .to_string();
    let response = post(&addr, "/api/v1/auth/login", &body).await;

    assert!(
        response.starts_with("HTTP/1.1 204"),
        "the generated password must be accepted:\n{response}"
    );
    assert!(
        response.contains("uops_session="),
        "a successful login must set a session cookie:\n{response}"
    );

    scratch.drop_database().await;
}

#[tokio::test]
async fn a_wrong_password_is_refused_by_the_same_server() {
    let scratch = Scratch::new().await;
    let names = names();

    firstrun::bootstrap(&scratch.store, &names)
        .await
        .expect("bootstrap")
        .expect("first run");

    let addr = serve(AppState::new(scratch.store.clone(), telemetry())).await;

    // The negative control for the test above. Without it, a login handler that
    // accepted anything would pass `the_printed_password_logs_in` perfectly.
    let body = serde_json::json!({
        "email": names.admin_email,
        "password": "not-the-generated-password",
    })
    .to_string();
    let response = post(&addr, "/api/v1/auth/login", &body).await;

    assert!(
        response.starts_with("HTTP/1.1 401"),
        "a wrong password must be refused:\n{response}"
    );
    assert!(
        !response.contains("uops_session="),
        "a refused login must not set a session cookie:\n{response}"
    );

    scratch.drop_database().await;
}

#[tokio::test]
async fn a_second_boot_announces_nothing() {
    let scratch = Scratch::new().await;
    let names = names();

    assert!(
        firstrun::bootstrap(&scratch.store, &names)
            .await
            .unwrap()
            .is_some(),
        "the first boot of an empty installation must produce a credential"
    );
    assert!(
        firstrun::bootstrap(&scratch.store, &names)
            .await
            .unwrap()
            .is_none(),
        "a second boot must produce no credential — announcing one that belongs to no \
         account is worse than announcing nothing, because someone will type it"
    );

    scratch.drop_database().await;
}

/// A minimal HTTP/1.1 POST, returning the raw response.
///
/// Hand-rolled rather than pulling in a client: the point of this test is that the
/// server speaks HTTP on a socket, and a client library that shares a stack with the
/// server would be testing rather less than it appears to.
async fn post(addr: &std::net::SocketAddr, path: &str, body: &str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(request.as_bytes())
        .await
        .expect("write request");

    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .await
        .expect("read response");
    String::from_utf8_lossy(&response).into_owned()
}
