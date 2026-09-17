//! Discovery jobs, runs and candidates against a real `PostgreSQL`.
//!
//! The parts a unit test cannot reach: `cidr[]` surviving the round trip through text,
//! the upsert that makes a candidate a thing rather than a sighting, an operator's
//! decision outliving the next run, and the counters the schema refuses to let lie.

use std::net::IpAddr;

use uops_core::{CredentialRef, Error as CoreError, OrgId, TenantId, TenantScope};
use uops_discover::Range;
use uops_store_pg::discovery_jobs::{
    CandidateSource, CandidateState, NewCandidate, NewJob, RunCounts, RunStatus, Trigger,
};
use uops_store_pg::{Config, PgStore};

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

/// A tenant of its own per test, so two runs cannot collide on a job name.
async fn tenant(store: &PgStore, slug: &str) -> TenantScope {
    let org = OrgId::new();
    let tenant = TenantId::new();
    let unique = tenant.into_uuid().simple().to_string();

    sqlx::query("INSERT INTO organization (id, name) VALUES ($1, $2)")
        .bind(org.into_uuid())
        .bind(format!("disc-org-{unique}"))
        .execute(store.pool())
        .await
        .expect("organization");
    sqlx::query("INSERT INTO tenant (id, org_id, name, slug) VALUES ($1, $2, $3, $4)")
        .bind(tenant.into_uuid())
        .bind(org.into_uuid())
        .bind(format!("disc-{slug}"))
        .bind(format!("{slug}-{unique}"))
        .execute(store.pool())
        .await
        .expect("tenant");

    TenantScope::collector(tenant)
}

fn range(s: &str) -> Range {
    s.parse().expect("a test range parses")
}

fn job(name: &str, ranges: &[&str]) -> NewJob {
    NewJob {
        name: name.to_owned(),
        description: String::new(),
        ranges: ranges.iter().map(|r| range(r)).collect(),
        site_id: None,
        credential_refs: vec![CredentialRef::new()],
        snmp_port: 161,
        skip_silent_hosts: false,
        schedule: None,
    }
}

fn address(s: &str) -> IpAddr {
    s.parse().expect("a test address parses")
}

#[tokio::test]
async fn a_job_round_trips_its_ranges() {
    // `cidr[]` crosses the boundary as text because sqlx has no mapping for it without a
    // new dependency. This is the test that the conversion is lossless in both
    // directions -- a job whose ranges came back wrong would scan the wrong network.
    let store = store().await;
    let scope = tenant(&store, "roundtrip").await;

    let created = store
        .create_discovery_job(
            &scope,
            None,
            &job("Branch offices", &["192.168.1.0/24", "10.4.0.0/22"]),
        )
        .await
        .expect("create");

    assert_eq!(
        created.ranges,
        vec![range("192.168.1.0/24"), range("10.4.0.0/22")]
    );
    assert_eq!(created.snmp_port, 161);
    assert!(created.enabled);
    assert!(created.schedule.is_none(), "no schedule means manual only");

    let read_back = store
        .discovery_job(&scope, created.id)
        .await
        .expect("read back");
    assert_eq!(read_back.ranges, created.ranges);
}

#[tokio::test]
async fn host_bits_survive_the_round_trip_as_the_network() {
    // `Range` clears host bits, which is what every router CLI does. The `cidr` column
    // would refuse `192.168.1.40/24` outright, so this also proves the crate normalises
    // before the schema ever sees it.
    let store = store().await;
    let scope = tenant(&store, "hostbits").await;

    let created = store
        .create_discovery_job(&scope, None, &job("Typed by hand", &["192.168.1.40/24"]))
        .await
        .expect("create");
    assert_eq!(created.ranges, vec![range("192.168.1.0/24")]);
}

#[tokio::test]
async fn a_schedule_round_trips_as_a_duration() {
    let store = store().await;
    let scope = tenant(&store, "schedule").await;

    let mut nightly = job("Nightly", &["10.1.0.0/24"]);
    nightly.schedule = Some(std::time::Duration::from_hours(24));

    let created = store
        .create_discovery_job(&scope, None, &nightly)
        .await
        .expect("create");
    assert_eq!(created.schedule, Some(std::time::Duration::from_hours(24)));
}

#[tokio::test]
async fn a_range_wider_than_a_16_is_refused_with_the_sentence_not_the_constraint() {
    // §4's fourth acceptance criterion is about the message. The CHECK in migration 0017
    // would refuse the same row, but "violates discovery_job_no_range_wider_than_a_16"
    // tells an operator only that they are wrong.
    let store = store().await;
    let scope = tenant(&store, "toowide").await;

    let error = store
        .create_discovery_job(&scope, None, &job("Everything", &["10.0.0.0/8"]))
        .await
        .expect_err("a /8 must be refused");

    let CoreError::Invalid(sentence) = &error else {
        panic!("a range that is too wide is Invalid, not {error:?}");
    };
    assert!(
        sentence.contains("Split"),
        "the operator must be told what to do instead: {sentence}"
    );
    assert!(
        !sentence.contains("violates"),
        "they must not be shown the constraint name: {sentence}"
    );
}

#[tokio::test]
async fn a_job_with_no_credentials_is_refused_in_words() {
    // Not fastidiousness. A job with no credentials would probe every address in the
    // range, be refused by all of them, and report an empty network -- which is the one
    // outcome that makes an operator believe discovery does not work.
    let store = store().await;
    let scope = tenant(&store, "nocreds").await;

    let mut bare = job("No credentials", &["10.1.0.0/24"]);
    bare.credential_refs.clear();

    let error = store
        .create_discovery_job(&scope, None, &bare)
        .await
        .expect_err("no credentials must be refused");
    let CoreError::Invalid(sentence) = &error else {
        panic!("expected Invalid, got {error:?}");
    };
    assert!(sentence.contains("credential"), "{sentence}");
}

#[tokio::test]
async fn a_job_name_belongs_to_its_tenant() {
    // Two customers of one MSP both have a job called "Branch offices".
    let store = store().await;
    let ours = tenant(&store, "ours").await;
    let theirs = tenant(&store, "theirs").await;

    store
        .create_discovery_job(&ours, None, &job("Branch offices", &["10.1.0.0/24"]))
        .await
        .expect("ours");
    store
        .create_discovery_job(&theirs, None, &job("Branch offices", &["10.1.0.0/24"]))
        .await
        .expect("theirs is a different tenant's job of the same name");

    let error = store
        .create_discovery_job(&ours, None, &job("Branch offices", &["10.2.0.0/24"]))
        .await
        .expect_err("the same name twice in one tenant");
    assert!(matches!(error, CoreError::Invalid(_)), "{error:?}");
}

#[tokio::test]
async fn another_tenants_job_is_not_found_rather_than_forbidden() {
    // 404-never-403. A distinguishable response is a way to enumerate another customer's
    // jobs by id, and a discovery job names their networks.
    let store = store().await;
    let ours = tenant(&store, "seeker").await;
    let theirs = tenant(&store, "sought").await;

    let hidden = store
        .create_discovery_job(&theirs, None, &job("Theirs", &["10.9.0.0/24"]))
        .await
        .expect("create");

    let error = store
        .discovery_job(&ours, hidden.id)
        .await
        .expect_err("another tenant's job");
    assert!(
        matches!(error, CoreError::NotFound { .. }),
        "must be NotFound, not {error:?}"
    );
}

#[tokio::test]
async fn deleting_a_job_keeps_the_record_of_what_it_scanned() {
    // §2.7. This is also the case that caught the composite ON DELETE SET NULL bug: the
    // default nulls the whole key including tenant_id, which is NOT NULL.
    let store = store().await;
    let scope = tenant(&store, "audit").await;

    let created = store
        .create_discovery_job(&scope, None, &job("Doomed", &["10.3.0.0/24"]))
        .await
        .expect("create");
    let run = store
        .start_discovery_run(
            &scope,
            Some(created.id),
            &created.ranges,
            Trigger::Schedule,
            None,
        )
        .await
        .expect("start");

    store
        .delete_discovery_job(&scope, created.id)
        .await
        .expect("delete");

    let runs = store.discovery_runs(&scope, 10).await.expect("runs");
    let kept = runs
        .iter()
        .find(|r| r.id == run.id)
        .expect("the run outlives the job that started it");
    assert_eq!(
        kept.job_id, None,
        "the reference is gone, the record is not"
    );
    assert_eq!(
        kept.ranges,
        vec![range("10.3.0.0/24")],
        "and it still says what was scanned"
    );
}

#[tokio::test]
async fn a_run_snapshots_the_ranges_as_they_were() {
    // Not read back through the job, whose ranges are editable. "Who scanned 10.0.0.0/16
    // on Tuesday" is a question about the ranges at the time.
    let store = store().await;
    let scope = tenant(&store, "snapshot").await;

    let run = store
        .start_discovery_run(
            &scope,
            None,
            &[range("172.20.0.0/24")],
            Trigger::Manual,
            None,
        )
        .await
        .expect("start");

    assert_eq!(run.ranges, vec![range("172.20.0.0/24")]);
    assert_eq!(run.status, RunStatus::Running);
    assert!(run.finished_at.is_none(), "a running run has no end");
    assert_eq!(run.counts, RunCounts::default());
}

#[tokio::test]
async fn finishing_a_run_records_what_it_did_and_stamps_the_job() {
    // The scheduler reads `last_run_at` to decide what is due, and it is stamped in the
    // same transaction as the finish -- so a run that completed cannot leave its job
    // looking as though it never ran.
    let store = store().await;
    let scope = tenant(&store, "finish").await;

    let created = store
        .create_discovery_job(&scope, None, &job("Branch", &["10.5.0.0/24"]))
        .await
        .expect("create");
    assert!(created.last_run_at.is_none(), "a new job has never run");

    let run = store
        .start_discovery_run(
            &scope,
            Some(created.id),
            &created.ranges,
            Trigger::Schedule,
            None,
        )
        .await
        .expect("start");

    let counts = RunCounts {
        probed: 254,
        answered: 9,
        created: 7,
        merged: 1,
        for_review: 1,
        candidates: 2,
        edges: 0,
    };
    let finished = store
        .finish_discovery_run(&scope, run.id, RunStatus::Succeeded, counts, None)
        .await
        .expect("finish");

    assert_eq!(finished.status, RunStatus::Succeeded);
    assert_eq!(finished.counts, counts);
    assert!(finished.finished_at.is_some(), "a finished run has an end");

    let job_now = store
        .discovery_job(&scope, created.id)
        .await
        .expect("re-read");
    assert!(
        job_now.last_run_at.is_some(),
        "the job must know it has run"
    );
}

#[tokio::test]
async fn a_run_cannot_report_more_answers_than_probes() {
    // The schema's CHECK, reached through the store. A counter incremented on the wrong
    // path is the kind of bug that makes an operator distrust the whole screen.
    let store = store().await;
    let scope = tenant(&store, "counters").await;

    let run = store
        .start_discovery_run(&scope, None, &[range("10.6.0.0/24")], Trigger::Manual, None)
        .await
        .expect("start");

    let error = store
        .finish_discovery_run(
            &scope,
            run.id,
            RunStatus::Succeeded,
            RunCounts {
                probed: 254,
                answered: 300,
                ..RunCounts::default()
            },
            None,
        )
        .await
        .expect_err("300 devices cannot answer 254 probes");
    assert!(matches!(error, CoreError::Invalid(_)), "{error:?}");
}

#[tokio::test]
async fn a_failed_run_must_say_why() {
    let store = store().await;
    let scope = tenant(&store, "whyfail").await;

    let run = store
        .start_discovery_run(&scope, None, &[range("10.7.0.0/24")], Trigger::Manual, None)
        .await
        .expect("start");

    let error = store
        .finish_discovery_run(
            &scope,
            run.id,
            RunStatus::Failed,
            RunCounts::default(),
            None,
        )
        .await
        .expect_err("a failure with no reason is a red row nobody can act on");
    assert!(matches!(error, CoreError::Invalid(_)), "{error:?}");

    let run = store
        .start_discovery_run(&scope, None, &[range("10.7.0.0/24")], Trigger::Manual, None)
        .await
        .expect("start again");
    let finished = store
        .finish_discovery_run(
            &scope,
            run.id,
            RunStatus::Failed,
            RunCounts::default(),
            Some("no route to 10.7.0.0/24"),
        )
        .await
        .expect("a failure with a sentence is fine");
    assert_eq!(finished.error.as_deref(), Some("no route to 10.7.0.0/24"));
}

#[tokio::test]
async fn a_candidate_is_a_thing_rather_than_a_sighting() {
    // A nightly sweep over a /22 that finds the same 40 unidentifiable printers must not
    // write 14 600 rows a year to describe 40 printers.
    let store = store().await;
    let scope = tenant(&store, "printers").await;

    let first = store
        .record_candidate(
            &scope,
            None,
            CandidateSource::Sweep,
            &NewCandidate {
                address: Some(address("192.168.1.9")),
                sys_descr: Some("HP LaserJet".to_owned()),
                reason: "no monitoring profile matches this device".to_owned(),
                ..NewCandidate::default()
            },
        )
        .await
        .expect("first sighting");

    let second = store
        .record_candidate(
            &scope,
            None,
            CandidateSource::Sweep,
            &NewCandidate {
                address: Some(address("192.168.1.9")),
                sys_name: Some("printer-3".to_owned()),
                reason: "no monitoring profile matches this device".to_owned(),
                ..NewCandidate::default()
            },
        )
        .await
        .expect("second sighting");

    assert_eq!(second.id, first.id, "one printer, one row");
    assert_eq!(
        second.first_seen, first.first_seen,
        "first_seen is when we first saw it"
    );
    assert!(second.last_seen >= first.last_seen);
    assert_eq!(
        second.sys_descr.as_deref(),
        Some("HP LaserJet"),
        "a later sighting that says less must not erase what an earlier one knew"
    );
    assert_eq!(
        second.sys_name.as_deref(),
        Some("printer-3"),
        "and adds what it does know"
    );

    let outstanding = store.discovery_candidates(&scope, 50).await.expect("list");
    assert_eq!(outstanding.len(), 1);
}

#[tokio::test]
async fn a_sweep_sighting_and_an_lldp_sighting_are_two_candidates() {
    // Deliberately. Deciding that they are the same device is identity resolution's job,
    // not a unique index's -- and the fingerprint is only strong enough to stop one run
    // duplicating its own findings.
    let store = store().await;
    let scope = tenant(&store, "twosources").await;

    store
        .record_candidate(
            &scope,
            None,
            CandidateSource::Sweep,
            &NewCandidate {
                address: Some(address("10.8.0.9")),
                ..NewCandidate::default()
            },
        )
        .await
        .expect("sweep");
    store
        .record_candidate(
            &scope,
            None,
            CandidateSource::Lldp,
            &NewCandidate {
                address: Some(address("10.8.0.9")),
                chassis_id: Some("00:1b:21:3c:4d:5e".to_owned()),
                port_id: Some("GigabitEthernet0/1".to_owned()),
                ..NewCandidate::default()
            },
        )
        .await
        .expect("lldp");

    assert_eq!(
        store
            .discovery_candidates(&scope, 50)
            .await
            .expect("list")
            .len(),
        2
    );
}

#[tokio::test]
async fn an_ignored_candidate_stays_ignored_through_the_next_run() {
    // The whole point of the state. An operator who has said "I know, it is a printer"
    // must not be shown it again tomorrow morning, and a plain upsert would reset it.
    let store = store().await;
    let scope = tenant(&store, "ignored").await;

    let candidate = store
        .record_candidate(
            &scope,
            None,
            CandidateSource::Sweep,
            &NewCandidate {
                address: Some(address("10.9.0.9")),
                reason: "nothing matches this device".to_owned(),
                ..NewCandidate::default()
            },
        )
        .await
        .expect("first sighting");

    store
        // None rather than a minted ActorId: ignoring is attributable to an app_user
        // row, and the FK is what says so. The nullable case is the real one here --
        // this fixture has no users in it.
        .ignore_candidate(&scope, candidate.id, None, "it is a printer")
        .await
        .expect("ignore");
    assert!(
        store
            .discovery_candidates(&scope, 50)
            .await
            .expect("list")
            .is_empty(),
        "an ignored candidate is off the list"
    );

    let again = store
        .record_candidate(
            &scope,
            None,
            CandidateSource::Sweep,
            &NewCandidate {
                address: Some(address("10.9.0.9")),
                reason: "nothing matches this device".to_owned(),
                ..NewCandidate::default()
            },
        )
        .await
        .expect("tonight's sweep finds it again");

    assert_eq!(
        again.state,
        CandidateState::Ignored,
        "the decision outlives the run"
    );
    assert_eq!(
        again.reason, "it is a printer",
        "and so does the operator's words"
    );
    assert!(
        store
            .discovery_candidates(&scope, 50)
            .await
            .expect("list")
            .is_empty(),
        "and it is still off the list"
    );
    assert!(
        again.last_seen >= candidate.last_seen,
        "though it was still seen"
    );
}

#[tokio::test]
async fn a_refused_credential_is_recorded_rather_than_guessed_at() {
    // §2.2, as it reaches the database. The candidate's reason is what tells the operator
    // to supply a credential -- which is the alternative to the product trying another
    // community string.
    let store = store().await;
    let scope = tenant(&store, "refused").await;

    let candidate = store
        .record_candidate(
            &scope,
            None,
            CandidateSource::Sweep,
            &NewCandidate {
                address: Some(address("10.10.0.5")),
                state: CandidateState::Unreachable,
                reason: "no credential named by this job was accepted".to_owned(),
                ..NewCandidate::default()
            },
        )
        .await
        .expect("record");

    assert_eq!(candidate.state, CandidateState::Unreachable);
    assert!(
        candidate.state.is_outstanding(),
        "it is on the operator's list"
    );
    assert!(candidate.reason.contains("credential"));
}

#[tokio::test]
async fn a_candidate_with_nothing_to_look_for_is_refused() {
    let store = store().await;
    let scope = tenant(&store, "empty").await;

    let error = store
        .record_candidate(
            &scope,
            None,
            CandidateSource::Lldp,
            &NewCandidate::default(),
        )
        .await
        .expect_err("neither an address nor a chassis id");
    assert!(matches!(error, CoreError::Invalid(_)), "{error:?}");
}

#[tokio::test]
async fn a_neighbour_with_no_address_is_still_a_candidate() {
    // Many platforms report a chassis ID and a port and no management address at all,
    // which is precisely why §2.5 refuses to invent a resource from one -- but it is
    // still something that exists and is worth writing down.
    let store = store().await;
    let scope = tenant(&store, "chassisonly").await;

    let candidate = store
        .record_candidate(
            &scope,
            None,
            CandidateSource::Lldp,
            &NewCandidate {
                chassis_id: Some("00:1b:21:aa:bb:cc".to_owned()),
                port_id: Some("Gi1/0/24".to_owned()),
                platform: Some("cisco WS-C2960".to_owned()),
                reason: "no resource matches this chassis id".to_owned(),
                ..NewCandidate::default()
            },
        )
        .await
        .expect("record");

    assert!(candidate.address.is_none());
    assert_eq!(candidate.chassis_id.as_deref(), Some("00:1b:21:aa:bb:cc"));
}

#[tokio::test]
async fn a_mac_is_one_value_however_the_vendor_spells_it() {
    // `macaddr` rather than text, so 00:1b:21:3c:4d:5e and 001b.213c.4d5e -- the same
    // address, spelled the two ways two vendors spell it -- do not become two candidates.
    let store = store().await;
    let scope = tenant(&store, "macspelling").await;

    let cisco = store
        .record_candidate(
            &scope,
            None,
            CandidateSource::Arp,
            &NewCandidate {
                address: Some(address("10.11.0.7")),
                mac: Some("001b.213c.4d5e".to_owned()),
                ..NewCandidate::default()
            },
        )
        .await
        .expect("record");

    assert_eq!(
        cisco.mac.as_deref(),
        Some("00:1b:21:3c:4d:5e"),
        "PostgreSQL normalises the spelling, which is why the column is macaddr"
    );
}

#[tokio::test]
async fn a_candidate_cannot_be_recorded_into_another_tenant() {
    // The composite foreign key on `last_run_id` is what makes guessing a run's uuid
    // useless rather than merely unlikely.
    let store = store().await;
    let ours = tenant(&store, "candours").await;
    let theirs = tenant(&store, "candtheirs").await;

    let their_run = store
        .start_discovery_run(
            &theirs,
            None,
            &[range("10.12.0.0/24")],
            Trigger::Manual,
            None,
        )
        .await
        .expect("their run");

    let error = store
        .record_candidate(
            &ours,
            Some(their_run.id),
            CandidateSource::Sweep,
            &NewCandidate {
                address: Some(address("10.12.0.5")),
                ..NewCandidate::default()
            },
        )
        .await
        .expect_err("a candidate of ours cannot point at their run");
    assert!(matches!(error, CoreError::Invalid(_)), "{error:?}");
}
