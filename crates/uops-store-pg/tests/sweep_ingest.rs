//! A sweep, end to end: simulated agents in, inventory out.
//!
//! These are `docs/M5-discovery.md` §4's acceptance criteria — the ones that need both a
//! network and a database, so neither `uops-discover`'s tests nor the store's own can
//! reach them:
//!
//! * a /24 sweep finds every agent and creates one resource per agent;
//! * re-running it creates nothing new;
//! * a device whose hostname matches an existing resource goes to review, not to a
//!   silent merge and not to a second copy;
//! * and a swept device is pollable without a second registration step.

use std::net::SocketAddr;

use uops_core::{OrgId, ResourceId, TenantId, TenantScope};
use uops_discover::{Range, Sweep, run};
use uops_identity::Resolver;
use uops_profile::Oid;
use uops_snmp::sim::{Agent, Behaviour, Fleet};
use uops_snmp::transport::Value;
use uops_store_pg::sweep_ingest::SweepContext;
use uops_store_pg::{Config, PgStore};

const SYSOBJECTID: &str = "1.3.6.1.2.1.1.2.0";
const SYSDESCR: &str = "1.3.6.1.2.1.1.1.0";
const SYSNAME: &str = "1.3.6.1.2.1.1.5.0";

/// A Catalyst 2960, as far as a three-OID probe can tell.
const CATALYST: &str = "1.3.6.1.4.1.9.1.716";

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

async fn tenant(store: &PgStore, slug: &str) -> TenantScope {
    let org = OrgId::new();
    let tenant = TenantId::new();
    let unique = tenant.into_uuid().simple().to_string();

    sqlx::query("INSERT INTO organization (id, name) VALUES ($1, $2)")
        .bind(org.into_uuid())
        .bind(format!("sweep-org-{unique}"))
        .execute(store.pool())
        .await
        .expect("organization");
    sqlx::query("INSERT INTO tenant (id, org_id, name, slug) VALUES ($1, $2, $3, $4)")
        .bind(tenant.into_uuid())
        .bind(org.into_uuid())
        .bind(format!("sweep-{unique}"))
        .bind(format!("{slug}-{unique}"))
        .execute(store.pool())
        .await
        .expect("tenant");

    TenantScope::collector(tenant)
}

fn oid(s: &str) -> Oid {
    s.parse().expect("a constant OID must parse")
}

fn at(address: &str) -> SocketAddr {
    format!("{address}:161")
        .parse()
        .expect("a test address must parse")
}

fn device(name: &str, sysobjectid: &str) -> Agent {
    let mut agent = Agent::empty();
    agent.set(oid(SYSOBJECTID), Value::ObjectId(oid(sysobjectid)));
    agent.set(oid(SYSNAME), Value::Bytes(name.as_bytes().to_vec()));
    agent.set(
        oid(SYSDESCR),
        Value::Bytes(b"Cisco IOS Software, C2960 Software".to_vec()),
    );
    agent
}

fn sweep_of(range: &str) -> Sweep {
    Sweep::new(&[range.parse::<Range>().expect("a test range parses")])
        .expect("a test range is within the limits")
}

/// How many resources this tenant has, which is the number every test here is about.
async fn resource_count(store: &PgStore, scope: &TenantScope) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM resource WHERE tenant_id = $1")
        .bind(scope.tenant_id().into_uuid())
        .fetch_one(store.pool())
        .await
        .expect("count")
}

#[tokio::test]
async fn a_sweep_creates_one_resource_per_agent() {
    // §4's first acceptance criterion. Eight devices scattered through a /24 of
    // mostly-empty addresses, which is what a branch office looks like.
    let store = store().await;
    let scope = tenant(&store, "found").await;
    let resolver = Resolver::new(store.clone());

    let mut fleet = Fleet::new();
    let placed = [7u8, 12, 31, 64, 65, 128, 200, 254];
    for (n, host) in placed.iter().enumerate() {
        fleet.insert(
            at(&format!("192.168.1.{host}")),
            device(&format!("branch-sw-{n:02}"), CATALYST),
        );
    }

    let findings = run(&fleet, &sweep_of("192.168.1.0/24"), 161).await;
    let counts = store
        .record_sweep(&scope, &resolver, &findings, SweepContext::default())
        .await
        .expect("record");

    assert_eq!(counts.probed, 254);
    assert_eq!(counts.answered, 8);
    assert_eq!(counts.created, 8, "one resource per agent");
    assert_eq!(counts.merged, 0);
    assert_eq!(counts.for_review, 0, "nothing here resembles anything else");
    assert_eq!(counts.candidates, 0, "every agent was identifiable");
    assert_eq!(resource_count(&store, &scope).await, 8);

    // And the counters the schema will accept: nothing answered that was not probed.
    assert!(counts.answered <= counts.probed);
}

#[tokio::test]
async fn re_running_a_sweep_creates_nothing_new() {
    // §4's second criterion, and the one that decides whether discovery can be scheduled
    // at all. A nightly job that creates eight more devices every night is worse than no
    // discovery: after a week the inventory is unusable and nobody can tell which row is
    // the real switch.
    //
    // Note what makes it work. The probe proves only an address and a hostname, neither
    // of which is tier one, so the *confidence* of the pair is 0.93 -- below
    // AUTO_MERGE_THRESHOLD. What resolves it is the resolver's exclusive-match path:
    // every identifier points at one resource and nothing else, which is a repeat
    // sighting rather than a coincidence.
    let store = store().await;
    let scope = tenant(&store, "again").await;
    let resolver = Resolver::new(store.clone());

    let mut fleet = Fleet::new();
    for n in 1..=4u8 {
        fleet.insert(
            at(&format!("10.20.0.{n}")),
            device(&format!("core-{n:02}"), CATALYST),
        );
    }
    let sweep = sweep_of("10.20.0.0/29");

    let first = run(&fleet, &sweep, 161).await;
    let first = store
        .record_sweep(&scope, &resolver, &first, SweepContext::default())
        .await
        .expect("first run");
    assert_eq!(first.created, 4);
    assert_eq!(resource_count(&store, &scope).await, 4);

    // A fresh resolver, because a warm cache would prove only that the cache works.
    // Tonight's sweep runs in a process that started this evening.
    let tonight = Resolver::new(store.clone());
    let second = run(&fleet, &sweep, 161).await;
    let second = store
        .record_sweep(&scope, &tonight, &second, SweepContext::default())
        .await
        .expect("second run");

    assert_eq!(second.created, 0, "nothing is new the second time");
    assert_eq!(
        second.merged, 4,
        "every address resolved to what the first run made"
    );
    assert_eq!(
        second.for_review, 0,
        "a repeat sighting is not a question for a human"
    );
    assert_eq!(
        resource_count(&store, &scope).await,
        4,
        "four switches, not eight"
    );
}

#[tokio::test]
async fn a_hostname_that_matches_an_existing_device_goes_to_review() {
    // §4's third criterion. The same hostname in two sites is the situation a sweep
    // produces constantly -- an estate built from one template has a `core-01` in every
    // building -- and it is exactly what must not be merged silently.
    let store = store().await;
    let scope = tenant(&store, "twins").await;
    let resolver = Resolver::new(store.clone());

    let mut site_a = Fleet::new();
    site_a.insert(at("10.21.0.1"), device("core-01", CATALYST));
    let findings = run(&site_a, &sweep_of("10.21.0.0/29"), 161).await;
    let first = store
        .record_sweep(&scope, &resolver, &findings, SweepContext::default())
        .await
        .expect("site A");
    assert_eq!(first.created, 1);

    // The other building. Same name, different address, and nothing tier-one to tell
    // them apart -- because a probe cannot see a serial or an engine id.
    let mut site_b = Fleet::new();
    site_b.insert(at("10.22.0.1"), device("core-01", CATALYST));
    let findings = run(&site_b, &sweep_of("10.22.0.0/29"), 161).await;
    let second = store
        .record_sweep(&scope, &resolver, &findings, SweepContext::default())
        .await
        .expect("site B");

    assert_eq!(
        second.merged, 0,
        "two devices sharing a hostname must never be merged on that alone"
    );
    assert_eq!(
        second.for_review, 1,
        "it is a question for a human, and the review queue is where it goes"
    );

    // A provisional resource *and* a review item. The provisional row is what a reviewer
    // merges from; without one there would be nothing to point the merge at. So there
    // are two resources here, and that is correct -- what matters is that a human was
    // asked rather than a machine deciding.
    assert_eq!(resource_count(&store, &scope).await, 2);

    let reviews = resolver
        .reviews(scope.tenant_id(), 10)
        .await
        .expect("reviews");
    assert_eq!(
        reviews.len(),
        1,
        "and the question is actually on the queue"
    );
}

#[tokio::test]
async fn a_swept_device_is_pollable_without_a_second_step() {
    // `pollable_devices` joins `resource_identifier` on kind = 'mgmt_ip', and
    // `Sighting::observed` emits exactly that -- so attaching the identity *is* making
    // the device pollable. This is the test that says so, because it is the kind of
    // coupling that is easy to break from either end.
    let store = store().await;
    let scope = tenant(&store, "pollable").await;
    let resolver = Resolver::new(store.clone());

    let mut fleet = Fleet::new();
    fleet.insert(at("10.23.0.5"), device("edge-01", CATALYST));

    let findings = run(&fleet, &sweep_of("10.23.0.0/29"), 161).await;
    store
        .record_sweep(&scope, &resolver, &findings, SweepContext::default())
        .await
        .expect("record");

    let devices = store.pollable_devices(&scope, 10).await.expect("pollable");
    assert_eq!(devices.len(), 1, "a swept device is in the fleet");
    assert_eq!(devices[0].address, "10.23.0.5");

    // And with its sysObjectID already cached, so the first poll uses the right profile
    // rather than falling back to generic-snmp for a cycle.
    assert_eq!(devices[0].sysobjectid.as_deref(), Some(CATALYST));

    // But not pinned. `resource.profile_id` is a human's override, and writing it from
    // discovery would make every swept device look like one somebody decided about.
    assert_eq!(
        devices[0].profile_id, None,
        "discovery caches the sysObjectID; it does not pin a profile"
    );
}

#[tokio::test]
async fn an_agent_that_says_nothing_about_itself_is_a_candidate_not_a_resource() {
    // UPSs, PDUs and environmental sensors answer on 161 with almost nothing in the MIB.
    // A resource with no name and no profile is a row nothing can poll and nobody can
    // act on -- the same argument §2.5 makes about inventing a device from a chassis id.
    let store = store().await;
    let scope = tenant(&store, "mute").await;
    let resolver = Resolver::new(store.clone());

    let mut fleet = Fleet::new();
    fleet.insert(at("10.24.0.3"), Agent::empty());
    fleet.insert(at("10.24.0.4"), device("real-switch", CATALYST));

    let findings = run(&fleet, &sweep_of("10.24.0.0/29"), 161).await;
    let counts = store
        .record_sweep(&scope, &resolver, &findings, SweepContext::default())
        .await
        .expect("record");

    assert_eq!(counts.answered, 2, "both are there");
    assert_eq!(counts.created, 1, "only one is inventory");
    assert_eq!(counts.candidates, 1, "the other is on the list to look at");
    assert_eq!(resource_count(&store, &scope).await, 1);

    let candidates = store.discovery_candidates(&scope, 10).await.expect("list");
    assert_eq!(candidates.len(), 1);
    assert!(
        candidates[0].reason.contains("sysObjectID"),
        "the reason must say what was missing: {}",
        candidates[0].reason
    );
}

#[tokio::test]
async fn an_agent_that_refuses_the_credentials_becomes_a_candidate_with_advice() {
    // §2.2 as it reaches an operator. The alternative to guessing another community
    // string is telling somebody, in words, that there is a device here and which
    // problem they have.
    let store = store().await;
    let scope = tenant(&store, "locked").await;
    let resolver = Resolver::new(store.clone());

    let mut fleet = Fleet::new();
    fleet.insert(
        at("10.25.0.2"),
        device("locked-sw", CATALYST).behaving(Behaviour::AuthFails),
    );

    let findings = run(&fleet, &sweep_of("10.25.0.0/29"), 161).await;
    let counts = store
        .record_sweep(&scope, &resolver, &findings, SweepContext::default())
        .await
        .expect("record");

    assert_eq!(
        counts.answered, 1,
        "a refusal is an answer: something is there"
    );
    assert_eq!(
        counts.created, 0,
        "but nothing was learned about what it is"
    );
    assert_eq!(counts.candidates, 1);
    assert_eq!(resource_count(&store, &scope).await, 0);

    let candidates = store.discovery_candidates(&scope, 10).await.expect("list");
    assert!(
        candidates[0].reason.contains("credential"),
        "the operator must be told what to supply: {}",
        candidates[0].reason
    );
}

#[tokio::test]
async fn rediscovery_does_not_undo_what_an_operator_changed() {
    // A sweep tonight must not move a device to another site or replace a credential
    // somebody fixed this afternoon. Everything but the system-chosen name is COALESCEd
    // for this reason, and this is the test that keeps it that way.
    let store = store().await;
    let scope = tenant(&store, "hands-off").await;
    let resolver = Resolver::new(store.clone());

    let mut fleet = Fleet::new();
    fleet.insert(at("10.26.0.1"), device("sw-01", CATALYST));
    let sweep = sweep_of("10.26.0.0/29");

    let findings = run(&fleet, &sweep, 161).await;
    store
        .record_sweep(&scope, &resolver, &findings, SweepContext::default())
        .await
        .expect("first");

    let id: ResourceId =
        sqlx::query_scalar::<_, uuid::Uuid>("SELECT id FROM resource WHERE tenant_id = $1")
            .bind(scope.tenant_id().into_uuid())
            .fetch_one(store.pool())
            .await
            .map(ResourceId::from_uuid)
            .expect("the swept device");

    // The operator names it properly and the display name is theirs alone.
    sqlx::query("UPDATE resource SET display_name = $2 WHERE id = $1")
        .bind(id.into_uuid())
        .bind("Comms room switch")
        .execute(store.pool())
        .await
        .expect("rename");

    let findings = run(&fleet, &sweep, 161).await;
    store
        .record_sweep(&scope, &resolver, &findings, SweepContext::default())
        .await
        .expect("second");

    let display: Option<String> =
        sqlx::query_scalar("SELECT display_name FROM resource WHERE id = $1")
            .bind(id.into_uuid())
            .fetch_one(store.pool())
            .await
            .expect("read back");
    assert_eq!(
        display.as_deref(),
        Some("Comms room switch"),
        "a sweep must never overwrite what an operator named"
    );
}

#[tokio::test]
async fn the_counters_a_sweep_reports_are_the_ones_a_run_will_accept() {
    // `record_sweep` produces the counters and `finish_discovery_run` writes them, and
    // the schema refuses several kinds of nonsense. Running the two together is what
    // proves they agree -- a counter incremented on the wrong path is the kind of bug
    // that makes an operator distrust the whole screen.
    let store = store().await;
    let scope = tenant(&store, "endtoend").await;
    let resolver = Resolver::new(store.clone());

    let mut fleet = Fleet::new();
    fleet.insert(at("10.27.0.1"), device("sw-01", CATALYST));
    fleet.insert(at("10.27.0.2"), Agent::empty());
    fleet.insert(
        at("10.27.0.3"),
        device("sw-03", CATALYST).behaving(Behaviour::AuthFails),
    );

    let ranges = vec!["10.27.0.0/29".parse::<Range>().expect("parses")];
    let started = store
        .start_discovery_run(
            &scope,
            None,
            &ranges,
            uops_store_pg::discovery_jobs::Trigger::Manual,
            None,
        )
        .await
        .expect("start");

    let findings = run(&fleet, &sweep_of("10.27.0.0/29"), 161).await;
    let counts = store
        .record_sweep(
            &scope,
            &resolver,
            &findings,
            SweepContext {
                run_id: Some(started.id),
                ..SweepContext::default()
            },
        )
        .await
        .expect("record");

    let finished = store
        .finish_discovery_run(
            &scope,
            started.id,
            uops_store_pg::discovery_jobs::RunStatus::Succeeded,
            counts,
            None,
        )
        .await
        .expect("the schema accepts what the sweep counted");

    assert_eq!(finished.counts, counts);
    assert_eq!(finished.counts.answered, 3);
    assert_eq!(finished.counts.created, 1);
    assert_eq!(finished.counts.candidates, 2);

    // And every candidate says which run last saw it, which is how an operator gets from
    // a row on the list back to the sweep that produced it.
    let candidates = store.discovery_candidates(&scope, 10).await.expect("list");
    assert_eq!(candidates.len(), 2);
    assert!(candidates.iter().all(|c| c.last_run_id == Some(started.id)));
}
