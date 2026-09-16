//! The whole poller, against real everything.
//!
//! A real `PostgreSQL`, a real `ClickHouse`, and the containerised `net-snmp` agent.
//! Nothing here is simulated, which is the point: every other test in this crate replaces
//! one end or the other, and the failures that survive both are the ones in the joins —
//! a device that never becomes a `Device`, a credential the vault holds but the transport
//! cannot use, rows written under a tenant nothing will query them by.
//!
//! # Why each test gets its own database
//!
//! Every other integration test in this workspace shares one database and stays out of
//! the others' way by creating its own tenant. The poller cannot: a reload reads *every*
//! tenant, deliberately — see `uops_store_pg::all_tenant_ids` — so on a shared database
//! this test polls whatever every other suite has left lying around. The first version
//! did exactly that, and failed intermittently for a reason worth recording: a hundred
//! and seventy abandoned devices at unroutable addresses saturated the executor's
//! concurrency for five seconds each, and the one device this test cares about never
//! came up. That is not a flake, it is the test measuring the wrong thing.
//!
//! So each test creates a database, migrates it, and drops it — the same approach
//! `uops_store_pg`'s bootstrap tests take, and for the same reason: what is under test is
//! a property of the whole installation.
//!
//! ```bash
//! docker compose -f deploy/docker-compose.yml --profile test up -d postgres clickhouse snmp-agent
//! UOPS_SNMP_AGENT=127.0.0.1:16100 CLICKHOUSE_DB=uops //!   CLICKHOUSE_USER=uops CLICKHOUSE_PASSWORD=uops //!   cargo test -p uops-poller --test live
//! ```
//!
//! `ChConfig::from_env` defaults to the `default` user with no password, which is what a
//! bare `clickhouse-server` has and not what `deploy/docker-compose.yml` creates. The
//! variables are the same ones the compose file passes to the server itself.
//!
//! Skipped when `UOPS_SNMP_AGENT` is unset, rather than failing — a developer who has not
//! started the fixture should not get a red suite for a container they did not ask for.
//! The skip says so out loud, so it cannot be mistaken for a pass; CI asserts the absence
//! of that message.

use std::sync::Arc;
use std::time::Duration;

use uops_core::{
    AuthProtocol, CredentialMaterial, OrgId, PrivProtocol, Secret, SiteId, TenantId, TenantScope,
};
use uops_poll::poller::Schedule;
use uops_poll::{Executor, Limits};
use uops_secrets::{CredentialMeta, KekRing, LocalVault};
use uops_store_ch::{ChClient, ChConfig, ChStore};
use uops_store_pg::{Config as PgConfig, PgSealedStore, PgStore};

use uops_poller::config;
use uops_poller::credentials::Transports;
use uops_poller::run::{self, Runner};

/// Everything the SNMP fixture is configured with. See `deploy/snmp-agent/README.md` —
/// all of it is public on purpose.
const USER: &str = "uops-v3";
const AUTH_PASS: &str = "uops-auth-passphrase";
const PRIV_PASS: &str = "uops-priv-passphrase";

macro_rules! agent_or_skip {
    () => {
        match std::env::var("UOPS_SNMP_AGENT") {
            Ok(a) => a,
            Err(_) => {
                println!(
                    "SKIPPED: UOPS_SNMP_AGENT is unset. Start the fixture with \
                     `docker compose -f deploy/docker-compose.yml --profile test up -d snmp-agent`"
                );
                return;
            }
        }
    };
}

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
        let admin = PgStore::connect(&PgConfig {
            url: admin_url(),
            ..PgConfig::default()
        })
        .await
        .expect("connect to the admin database");

        // Interpolated, not bound: CREATE DATABASE takes an identifier, and identifiers
        // cannot be parameters. The name is a literal prefix plus a UUID with the hyphens
        // removed, so there is nothing here an attacker could reach even in principle.
        let name = format!("uops_poll_{}", uuid::Uuid::now_v7().simple());
        sqlx::query(&format!(r#"CREATE DATABASE "{name}""#))
            .execute(admin.pool())
            .await
            .expect("create the scratch database");

        let base = admin_url()
            .rsplit_once('/')
            .expect("the database url has a path")
            .0
            .to_owned();
        let store = PgStore::connect(&PgConfig {
            url: format!("{base}/{name}"),
            ..PgConfig::default()
        })
        .await
        .expect("connect to the scratch database");

        sqlx::migrate!("../../migrations")
            .run(store.pool())
            .await
            .expect("migrate the scratch database");

        Self { store, name }
    }

    /// Dropped explicitly rather than in `Drop`, which cannot await. A test that panics
    /// leaves its database behind; `db.sh sweep` clears them, and a stray empty database
    /// is a better failure than a test that hangs trying to drop one.
    async fn drop_database(self) {
        let Self { store, name } = self;
        store.pool().close().await;

        let admin = PgStore::connect(&PgConfig {
            url: admin_url(),
            ..PgConfig::default()
        })
        .await
        .expect("connect to the admin database");
        sqlx::query(&format!(r#"DROP DATABASE IF EXISTS "{name}" WITH (FORCE)"#))
            .execute(admin.pool())
            .await
            .expect("drop the scratch database");
    }
}

/// A KEK from fixed bytes, so the vault this test seals with and the one the poller
/// opens with are the same ring.
///
/// `ephemeral_for_tests` would generate a fresh key per call, which is what a *restart*
/// must not do and equally what two processes sharing a database must not do.
fn fixed_kek() -> KekRing {
    let path = std::env::temp_dir().join("uops-poller-live-kek.hex");
    if !path.exists() {
        std::fs::write(&path, "0".repeat(64)).expect("write the test kek");
    }
    // Owner-only, because `KekRing::from_file` refuses a group- or world-readable key on
    // Unix — rightly: a KEK other local accounts can read is not a root of trust. The
    // default here is 0644, so without this every test in this file fails on Linux and
    // passes on Windows, where the check does not apply. Which is exactly what happened.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .expect("restrict the test kek");
    }
    KekRing::from_file(&path, uops_secrets::record::KeyId("test-kek".to_owned()))
        .expect("load the test kek")
}

type Vault = LocalVault<uops_secrets::RustCryptoAead, PgSealedStore, uops_secrets::MemoryAccessLog>;

fn vault(store: &PgStore) -> Vault {
    LocalVault::new(
        uops_secrets::RustCryptoAead,
        PgSealedStore::new(store.clone()),
        uops_secrets::MemoryAccessLog::new(),
        fixed_kek(),
    )
}

/// A tenant with one device pointed at the SNMP fixture, and a credential that opens it.
async fn seed(store: &PgStore, address: &str) -> (TenantId, uops_core::ResourceId) {
    let org = OrgId::new();
    let tenant = TenantId::new();
    let site = SiteId::new();
    let resource = uops_core::ResourceId::new();
    let slug = tenant.into_uuid().simple().to_string();

    sqlx::query("INSERT INTO organization (id, name) VALUES ($1, $2)")
        .bind(org.into_uuid())
        .bind(format!("live-org-{slug}"))
        .execute(store.pool())
        .await
        .expect("organization");
    sqlx::query("INSERT INTO tenant (id, org_id, name, slug) VALUES ($1, $2, $3, $4)")
        .bind(tenant.into_uuid())
        .bind(org.into_uuid())
        .bind(format!("live-{slug}"))
        .bind(format!("live-{slug}"))
        .execute(store.pool())
        .await
        .expect("tenant");
    sqlx::query("INSERT INTO site (id, tenant_id, name) VALUES ($1, $2, $3)")
        .bind(site.into_uuid())
        .bind(tenant.into_uuid())
        .bind("live")
        .execute(store.pool())
        .await
        .expect("site");

    // The credential first: resource.credential_ref points at it.
    let credential = vault(store)
        .put(
            tenant,
            Secret::new(CredentialMaterial::SnmpV3 {
                username: USER.to_owned(),
                auth: AuthProtocol::Sha256,
                auth_key: AUTH_PASS.to_owned(),
                privacy: PrivProtocol::Aes256,
                priv_key: PRIV_PASS.to_owned(),
            }),
            &CredentialMeta::new("live-agent"),
        )
        .expect("seal the credential");

    sqlx::query(
        "INSERT INTO resource (id, tenant_id, site_id, kind, name, status, credential_ref)
         VALUES ($1, $2, $3, 'device', $4, 'unknown', $5)",
    )
    .bind(resource.into_uuid())
    .bind(tenant.into_uuid())
    .bind(site.into_uuid())
    .bind(format!("live-device-{slug}"))
    .bind(credential.into_uuid())
    .execute(store.pool())
    .await
    .expect("resource");

    // The address is an identifier, not a column — see uops_store_pg::pollable. This row
    // is the one thing that makes the resource a *device*.
    sqlx::query(
        "INSERT INTO resource_identifier (id, tenant_id, resource_id, kind, value, confidence, source)
         VALUES ($1, $2, $3, 'mgmt_ip', $4, 0.80, 'manual')",
    )
    .bind(uuid::Uuid::now_v7())
    .bind(tenant.into_uuid())
    .bind(resource.into_uuid())
    .bind(address)
    .execute(store.pool())
    .await
    .expect("mgmt_ip");

    (tenant, resource)
}

fn metrics() -> ChStore {
    ChStore::new(ChClient::new(ChConfig::from_env()))
}

/// How many metric rows exist for this resource, by metric name.
async fn rows_for(resource: uops_core::ResourceId) -> Vec<(String, u64)> {
    let client = ChClient::new(ChConfig::from_env());
    // A bound parameter, not a formatted id: the same rule the rest of the codebase
    // follows, and the reason uops-query emits placeholders at all.
    let result = client
        .run(
            "SELECT metric, count() FROM metrics WHERE resource_id = {resource:UUID} \
             GROUP BY metric ORDER BY metric FORMAT TSV",
            &[("resource", resource.into_uuid().to_string())],
        )
        .await
        .expect("query ClickHouse");
    result
        .body
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            let (metric, count) = line.split_once('\t').expect("two columns");
            (metric.to_owned(), count.parse().expect("a count"))
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_device_in_postgres_becomes_rows_in_clickhouse() {
    // Everything the binary does, minus the signal handler: seed profiles, load the
    // fleet, tick, and let the wheel decide what runs. The assertion is the product —
    // rows, under the right tenant, named what the profile names them.
    let address = agent_or_skip!();
    let scratch = Scratch::new().await;
    let store = scratch.store.clone();
    let (_tenant, resource) = seed(&store, &address).await;

    store
        .seed_builtin_profiles(&uops_profile::builtin::all().expect("built-ins"))
        .await
        .expect("seed profiles");

    let runner = Arc::new(Runner::new(
        store.clone(),
        metrics(),
        Arc::new(Transports::new(vault(&store))),
        // Generous: the agent is local, but a loaded CI runner is not a quiet laptop and
        // a budget that fails under load would make this test flake rather than fail.
        Duration::from_secs(5),
    ));

    let mut schedule = Schedule::new();
    let (added, _) = run::reload(&runner, &mut schedule, 10_000)
        .await
        .expect("reload");
    assert!(
        added >= 1,
        "the seeded device must be schedulable; if this is 0 the join in pollable_devices \
         did not find it"
    );

    // Drive the wheel rather than waiting on a clock. Jitter spreads a 60-second job
    // across its interval, so this advances until the device has actually been polled —
    // 300 slots is five minutes of schedule and runs in about as long as the polls take.
    let executor = Executor::new(Limits {
        global: 16,
        per_device: 4,
        device_budget: Duration::from_secs(5),
    });
    let mut due = Vec::new();
    let mut polled = 0;
    let mut failed = 0;
    for _ in 0..300 {
        let report = run::tick_once(&runner, &executor, &mut schedule, &mut due).await;
        polled += report.ok;
        failed += report.failed + report.budget_exhausted;
    }

    assert!(polled > 0, "nothing was polled in five minutes of schedule");

    // ClickHouse batches; give the insert a moment to be visible.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let written = rows_for(resource).await;

    // The scalar every agent has. Interface metrics depend on the container having
    // interfaces worth counting, which is a property of the fixture rather than of the
    // poller — so this asserts the one that must be there.
    assert!(
        written
            .iter()
            .any(|(metric, n)| metric == "system.uptime" && *n > 0),
        "no system.uptime rows for the polled device; {polled} polls succeeded, \
         {failed} did not, and ClickHouse has {written:?}"
    );

    // Nothing may fail. This assertion used to say the opposite — the availability job
    // could not succeed while ICMP was unimplemented, so failures were expected and
    // their absence would have meant the planner had stopped scheduling it. ICMP works
    // now, and the assertion that pinned the old behaviour is what noticed.
    //
    // Conditional on ICMP being available at all: on a machine without an unprivileged
    // ICMP socket the check reports a `CheckError`, which is a failure of the *poller*
    // rather than of the device, and the run is not measuring what this asserts.
    if icmp_available() {
        assert_eq!(
            failed, 0,
            "a device that answers both SNMP and ICMP must fail nothing"
        );
    }

    scratch.drop_database().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_agents_interfaces_become_child_resources_and_member_of_edges() {
    // SPEC §M2's third acceptance criterion, against a real agent and a real database.
    // The **and** is the part: a child with no edge is a resource unreachable from the
    // device it belongs to, which the topology UI would render as a switch with no ports.
    //
    // The container's interfaces are whatever Docker gave it — `lo` and an `eth0`, on a
    // default bridge — so this asserts the shape rather than the names. The names are the
    // simulator's job; what only a real agent can prove is that `ifXTable::ifName` is
    // populated at all, which it is not on every agent and which is why `generic-snmp`
    // reads it rather than `ifDescr`.
    let address = agent_or_skip!();
    let scratch = Scratch::new().await;
    let store = scratch.store.clone();
    let (tenant, resource) = seed(&store, &address).await;

    store
        .seed_builtin_profiles(&uops_profile::builtin::all().expect("built-ins"))
        .await
        .expect("seed profiles");

    let runner = Arc::new(Runner::new(
        store.clone(),
        metrics(),
        Arc::new(Transports::new(vault(&store))),
        Duration::from_secs(5),
    ));
    let mut schedule = Schedule::new();
    run::reload(&runner, &mut schedule, 10_000)
        .await
        .expect("reload");

    let executor = Executor::new(Limits {
        global: 16,
        per_device: 4,
        device_budget: Duration::from_secs(5),
    });
    let mut due = Vec::new();
    // Discovery runs on a fifteen-minute interval, so the wheel has to be driven further
    // than for a metric. 1 000 slots is under seventeen minutes of schedule and costs
    // only the polls it actually dispatches.
    for _ in 0..1_000 {
        run::tick_once(&runner, &executor, &mut schedule, &mut due).await;
    }

    let scope = TenantScope::collector(tenant);
    let children = store.children_of(&scope, resource).await.expect("children");
    assert!(
        !children.is_empty(),
        "the agent has interfaces and none of them became a resource"
    );

    // The container has an `eth0` on Docker's bridge and a loopback, which between them
    // are exactly the two cases worth having a real agent for: one with a physical
    // address and one without.
    let names: Vec<&str> = children.iter().map(|(_, n)| n.as_str()).collect();
    assert!(names.contains(&"lo"), "no loopback: {names:?}");
    assert!(
        names.iter().any(|n| n.starts_with("eth")),
        "no ethernet interface: {names:?}"
    );

    // `lo` reports six zero bytes for ifPhysAddress, as every loopback does. Treating
    // that as a MAC would have the first loopback in a tenant claim
    // `00:00:00:00:00:00` — resource_identifier is unique on (tenant_id, kind, value) —
    // and every other one silently attach to nothing.
    let identifiers: Vec<(String, String)> = sqlx::query_as(
        "SELECT r.name, i.value
           FROM resource_identifier i
           JOIN resource r ON r.id = i.resource_id AND r.tenant_id = i.tenant_id
          WHERE i.tenant_id = $1 AND i.kind = 'mac'
          ORDER BY r.name",
    )
    .bind(tenant.into_uuid())
    .fetch_all(store.pool())
    .await
    .expect("mac identifiers");
    assert!(
        identifiers.iter().all(|(name, _)| name != "lo"),
        "the loopback was given a MAC it does not have: {identifiers:?}"
    );
    assert!(
        identifiers.iter().any(|(name, _)| name.starts_with("eth")),
        "the ethernet interface has a real MAC and it was not recorded: {identifiers:?}"
    );

    let members = store.members_of(&scope, resource).await.expect("edges");
    assert_eq!(
        members.len(),
        children.len(),
        "every child must have its member_of edge: {children:?} vs {members:?}"
    );
    let ids: Vec<_> = children.iter().map(|(id, _)| *id).collect();
    for member in &members {
        assert!(ids.contains(member), "an edge points outside the children");
    }

    // Each child carries the index its samples are labelled with, so a row of telemetry
    // and the resource it belongs to can be joined.
    for (id, name) in &children {
        let child = store.resource(&scope, *id).await.expect("child");
        assert_eq!(child.parent_id, Some(resource));
        assert_eq!(child.kind, uops_core::ResourceKind::Interface);
        assert!(
            child.attributes.get("network.interface.index").is_some(),
            "{name} has no index to join its telemetry on"
        );
    }

    // And the second walk finds the same interfaces rather than making new ones — the
    // property that decides whether a device accumulates ports for ever.
    for _ in 0..1_000 {
        run::tick_once(&runner, &executor, &mut schedule, &mut due).await;
    }
    let again = store.children_of(&scope, resource).await.expect("children");
    assert_eq!(
        again, children,
        "a second discovery pass created new interfaces instead of finding the old ones"
    );

    scratch.drop_database().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_device_with_no_credential_is_reported_and_does_not_stop_the_others() {
    // The shape of a real fleet: somebody adds a switch and forgets the credential. It
    // must not take the rest of the tenant down with it, and it must say so.
    let address = agent_or_skip!();
    let scratch = Scratch::new().await;
    let store = scratch.store.clone();
    let (tenant, resource) = seed(&store, &address).await;

    // A second device on the same tenant, addressed but with nothing to authenticate
    // with.
    let orphan = uops_core::ResourceId::new();
    sqlx::query(
        "INSERT INTO resource (id, tenant_id, kind, name, status)
         VALUES ($1, $2, 'device', $3, 'unknown')",
    )
    .bind(orphan.into_uuid())
    .bind(tenant.into_uuid())
    .bind(format!("orphan-{}", orphan.into_uuid().simple()))
    .execute(store.pool())
    .await
    .expect("orphan resource");
    sqlx::query(
        "INSERT INTO resource_identifier (id, tenant_id, resource_id, kind, value, confidence, source)
         VALUES ($1, $2, $3, 'mgmt_ip', $4, 0.80, 'manual')",
    )
    .bind(uuid::Uuid::now_v7())
    .bind(tenant.into_uuid())
    .bind(orphan.into_uuid())
    // Not the fixture's address: (tenant_id, kind, value) is unique, which is identity
    // resolution's rule that two resources cannot claim the same management address.
    // Where it points does not matter — the credential is checked before the transport.
    .bind("127.0.0.1:16199")
    .execute(store.pool())
    .await
    .expect("orphan mgmt_ip");

    store
        .seed_builtin_profiles(&uops_profile::builtin::all().expect("built-ins"))
        .await
        .expect("seed profiles");

    let runner = Arc::new(Runner::new(
        store.clone(),
        metrics(),
        Arc::new(Transports::new(vault(&store))),
        Duration::from_secs(5),
    ));
    let mut schedule = Schedule::new();
    let (added, _) = run::reload(&runner, &mut schedule, 10_000)
        .await
        .expect("reload");
    // At least, not exactly: reload reads every tenant, and a shared development database
    // holds whatever other suites have left in it. What this asserts is that both of
    // *these* devices were scheduled — the orphan included, since a device with no
    // credential must still be scheduled and then fail, not be quietly dropped at load.
    assert!(
        added >= 2,
        "both devices are addressed, so both are scheduled"
    );
    assert!(
        schedule.device(resource).is_some(),
        "the device with a credential is not in the schedule"
    );
    assert!(
        schedule.device(orphan).is_some(),
        "the device with no credential was dropped at load rather than failing at poll"
    );

    let executor = Executor::new(Limits {
        global: 16,
        per_device: 4,
        device_budget: Duration::from_secs(5),
    });
    let mut due = Vec::new();
    let mut ok = 0;
    for _ in 0..300 {
        ok += run::tick_once(&runner, &executor, &mut schedule, &mut due)
            .await
            .ok;
    }

    assert!(
        ok > 0,
        "the device that does have a credential must still have been polled"
    );

    sqlx::query("DELETE FROM resource_identifier WHERE resource_id = $1")
        .bind(orphan.into_uuid())
        .execute(store.pool())
        .await
        .expect("clean up the orphan");
    sqlx::query("DELETE FROM resource WHERE id = $1")
        .bind(orphan.into_uuid())
        .execute(store.pool())
        .await
        .expect("clean up the orphan");
    scratch.drop_database().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_binary_refuses_to_start_without_a_key_ring() {
    // The configuration decision worth asserting. A poller that started happily with no
    // KEK would fail on every device, which reads as a network problem for as long as it
    // takes somebody to find the logs.
    //
    // Asserted through the error rather than by running the binary: the variables are
    // read from the process environment, mutating it is unsafe in Rust 2024 and forbidden
    // here, and a test that set them would race every other test in this binary.
    let Err(e) = config::Config::from_env() else {
        // UOPS_KEK_FILE is set in this environment, so there is nothing to assert.
        println!("SKIPPED-CONFIG: a KEK is configured in this environment");
        return;
    };
    assert_eq!(e.variable, "UOPS_KEK_FILE");
    assert!(e.problem.contains("key-encryption key"), "{}", e.problem);
    // The message must say what to do, not only what is wrong.
    assert!(e.problem.contains("64 hex characters"), "{}", e.problem);
}

/// Whether this machine can open an unprivileged ICMP socket.
///
/// Same shape as `agent_or_skip!`: a developer on a platform without it should not get a
/// red suite, and the skip says so out loud. CI runs on Linux and asserts the message is
/// absent.
fn icmp_available() -> bool {
    #[cfg(unix)]
    {
        use socket2::{Domain, Protocol, Socket, Type};
        Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::ICMPV4)).is_ok()
    }
    #[cfg(not(unix))]
    {
        false
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_availability_check_writes_a_state_transition_and_updates_the_resource() {
    // SPEC §M2's remaining criterion, end to end: the device answers an ICMP echo
    // request, the poller notices its status changed, and the change lands in both
    // places it has to — `states` in ClickHouse for the history, and `resource.status`
    // in PostgreSQL for the inventory.
    //
    // The two are not redundant. The row is what an availability report is computed
    // from and is retained for 1 095 days; the column is what a list of devices shows
    // and holds only the latest value. Writing one without the other gives either an
    // inventory that is right and a history that never happened, or a history nobody can
    // see.
    let address = agent_or_skip!();
    if !icmp_available() {
        // Note for anyone chasing this: a container on Docker's default bridge gets
        // `net.ipv4.ping_group_range = 0 2147483647` and this works. A container run
        // with `--network host` inherits the host's namespace instead, where the range
        // is usually closed — so a skip here is about how the container was started,
        // not about the code.
        println!(
            "SKIPPED: no unprivileged ICMP socket. On Linux, widen \
             net.ipv4.ping_group_range to include this process's group."
        );
        return;
    }

    let scratch = Scratch::new().await;
    let store = scratch.store.clone();
    let (tenant, resource) = seed(&store, &address).await;
    let scope = TenantScope::collector(tenant);

    store
        .seed_builtin_profiles(&uops_profile::builtin::all().expect("built-ins"))
        .await
        .expect("seed profiles");

    // The device starts unknown, which is what `create_resource` leaves it as.
    assert_eq!(
        store
            .resource(&scope, resource)
            .await
            .expect("device")
            .status,
        uops_core::ResourceStatus::Unknown
    );

    let runner = Arc::new(Runner::new(
        store.clone(),
        metrics(),
        Arc::new(Transports::new(vault(&store))),
        Duration::from_secs(5),
    ));
    let mut schedule = Schedule::new();
    run::reload(&runner, &mut schedule, 10_000)
        .await
        .expect("reload");

    let executor = Executor::new(Limits {
        global: 16,
        per_device: 4,
        device_budget: Duration::from_secs(5),
    });
    let mut due = Vec::new();
    // The check is on a 30-second interval; 120 slots is two cycles with room for the
    // jitter that spreads it.
    for _ in 0..120 {
        run::tick_once(&runner, &executor, &mut schedule, &mut due).await;
    }

    // The address is the SNMP fixture's, which is loopback — so the device answers.
    assert_eq!(
        store
            .resource(&scope, resource)
            .await
            .expect("device")
            .status,
        uops_core::ResourceStatus::Up,
        "the device answered an echo request and its status was not updated"
    );

    // ClickHouse batches; give the insert a moment to be visible.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let rows = states_for(resource).await;
    assert_eq!(
        rows.len(),
        1,
        "exactly one transition: unknown → up. A row per check would make an \
         availability report a scan of a million identical rows: {rows:?}"
    );
    let (previous, current, severity, reason) = &rows[0];
    assert_eq!(previous, "unknown");
    assert_eq!(current, "up");
    // Coming up is informational. An operator paged for a recovery stops reading pages.
    assert_eq!(severity, "info");
    assert!(
        reason.contains("ICMP"),
        "the reason is the only part of the row that says why: {reason}"
    );
}

/// The state transitions recorded for a resource, oldest first.
async fn states_for(resource: uops_core::ResourceId) -> Vec<(String, String, String, String)> {
    let client = ChClient::new(ChConfig::from_env());
    let result = client
        .run(
            "SELECT previous_status, current_status, severity, reason FROM states \
             WHERE resource_id = {resource:UUID} ORDER BY observed_at FORMAT TSV",
            &[("resource", resource.into_uuid().to_string())],
        )
        .await
        .expect("query ClickHouse");
    result
        .body
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            let mut parts = line.split('\t');
            (
                parts.next().unwrap_or_default().to_owned(),
                parts.next().unwrap_or_default().to_owned(),
                parts.next().unwrap_or_default().to_owned(),
                parts.next().unwrap_or_default().to_owned(),
            )
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_agent_says_what_it_is_and_the_inventory_records_it() {
    // The identity job end to end. net-snmp implements no ENTITY-MIB — which is the
    // common case, not an awkward one: the Windows SNMP service and most equipment that
    // is not enterprise hardware are the same — so what comes back is `sysDescr` and
    // nothing else. That is the fallback working, and it is worth asserting against a
    // real agent rather than against a simulator that answers whatever it is told to.
    let address = agent_or_skip!();
    let scratch = Scratch::new().await;
    let store = scratch.store.clone();
    let (tenant, resource) = seed(&store, &address).await;
    let scope = TenantScope::collector(tenant);

    store
        .seed_builtin_profiles(&uops_profile::builtin::all().expect("built-ins"))
        .await
        .expect("seed profiles");

    let runner = Arc::new(Runner::new(
        store.clone(),
        metrics(),
        Arc::new(Transports::new(vault(&store))),
        Duration::from_secs(5),
    ));
    let mut schedule = Schedule::new();
    run::reload(&runner, &mut schedule, 10_000)
        .await
        .expect("reload");

    let executor = Executor::new(Limits {
        global: 16,
        per_device: 4,
        device_budget: Duration::from_secs(5),
    });
    let mut due = Vec::new();
    // Identity runs on the discovery interval — fifteen minutes — so the wheel has to be
    // driven past it.
    for _ in 0..1_000 {
        run::tick_once(&runner, &executor, &mut schedule, &mut due).await;
    }

    let device = store.resource(&scope, resource).await.expect("device");
    let os = device.os.clone().unwrap_or_default();
    assert!(
        os.to_lowercase().contains("linux"),
        "sysDescr should have reached resource.os; got {os:?}"
    );

    // The vendor is not from ENTITY-MIB, which this agent does not implement. It comes
    // from the MAC discovery found on the container's own interface — the OUI fallback,
    // which is the only thing that puts a manufacturer on equipment like this.
    println!(
        "identified: vendor {:?}, model {:?}, os {:?}",
        device.vendor, device.model, device.os
    );

    scratch.drop_database().await;
}
