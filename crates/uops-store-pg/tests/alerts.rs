//! Alert rules and state, against a real `PostgreSQL`.
//!
//! The state machine is tested in `uops-core`, with no database and no clock. What only
//! these can settle is what happens when two evaluations of one series meet in the same
//! table — which is the case a restart, a redeploy or a second replica produces, and the
//! one that turns a single problem into two alerts, one of which never resolves.

use chrono::{Duration, Utc};
use uops_core::alert::{AlertSeverity, Comparison, Condition, Phase, dedup_key};
use uops_core::{OrgId, ResourceId, ResourceKind, TenantId, TenantScope};
use uops_query::{AggFunc, Aggregation, Field, Query, SignalType, TimeRange};
use uops_store_pg::{Config, Evaluated, NewResource, NewRule, PgStore};

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

/// A tenant with one device in it.
async fn tenant(store: &PgStore, slug: &str) -> (TenantScope, ResourceId) {
    let org = OrgId::new();
    let tenant = TenantId::new();
    let unique = tenant.into_uuid().simple().to_string();

    sqlx::query("INSERT INTO organization (id, name) VALUES ($1, $2)")
        .bind(org.into_uuid())
        .bind(format!("alert-org-{unique}"))
        .execute(store.pool())
        .await
        .expect("organization");
    sqlx::query("INSERT INTO tenant (id, org_id, name, slug) VALUES ($1, $2, $3, $4)")
        .bind(tenant.into_uuid())
        .bind(org.into_uuid())
        .bind(format!("alert-{slug}"))
        .bind(format!("{slug}-{unique}"))
        .execute(store.pool())
        .await
        .expect("tenant");

    let scope = TenantScope::collector(tenant);
    let device = store
        .create_resource(&scope, &NewResource::new(ResourceKind::Device, "rtr-01"))
        .await
        .expect("device");
    (scope, device.id)
}

/// `avg(value) > 90 for 5m` over metrics — the rule SPEC uses as its example.
fn cpu_rule(name: &str) -> NewRule {
    let end = Utc::now();
    NewRule {
        name: name.to_owned(),
        description: String::new(),
        // An aggregation, because a threshold rule compares a number and the number is
        // the query's own: `avg(value) > 90`. A rule without one is refused.
        query: Query {
            aggregations: vec![Aggregation {
                func: AggFunc::Avg,
                field: Some(Field::Value),
                alias: "v".to_owned(),
            }],
            ..Query::new(
                SignalType::Metric,
                TimeRange::new(end - Duration::minutes(5), end),
            )
        },
        condition: Condition::Threshold {
            op: Comparison::Gt,
            value: 90.0,
            hold_seconds: 300,
        },
        severity: AlertSeverity::Critical,
        enabled: true,
        eval_interval: Duration::seconds(60),
        notify: serde_json::json!([]),
    }
}

#[tokio::test]
async fn a_rule_round_trips_as_a_query_and_a_condition() {
    let store = store().await;
    let (scope, _) = tenant(&store, "round-trip").await;
    let new = cpu_rule("CPU hot");

    let created = store.create_rule(&scope, None, &new).await.expect("create");
    let read = store.alert_rule(&scope, created.id).await.expect("read");

    assert_eq!(
        read.query, new.query,
        "the AST is stored, not a rendering of it"
    );
    assert_eq!(read.condition, new.condition);
    assert_eq!(read.severity, AlertSeverity::Critical);
    assert_eq!(read.eval_interval, Duration::seconds(60));
    assert!(read.enabled);
}

#[tokio::test]
async fn a_query_the_compiler_refuses_is_never_stored() {
    // A rule is evaluated by a background task with nobody watching. A query that fails
    // at evaluation time produces a log line a minute and an alert that never fires —
    // discovered during the incident it should have caught.
    let store = store().await;
    let (scope, _) = tenant(&store, "refused").await;

    let mut broken = cpu_rule("Broken");
    broken.query = Query {
        filter: Some(uops_query::Expr::Text {
            field: Field::Body,
            mode: uops_query::TextMode::AnyToken,
            terms: vec!["metrics have no body".to_owned()],
        }),
        ..broken.query.clone()
    };

    assert!(matches!(
        store.create_rule(&scope, None, &broken).await,
        Err(uops_core::Error::Invalid(_))
    ));
}

#[tokio::test]
async fn two_evaluations_of_one_series_are_one_alert() {
    // The case a restart overlapping its predecessor produces. Without the ON CONFLICT
    // this is two rows: one problem, two alerts, and the older one never resolves because
    // nothing evaluates it any more.
    let store = store().await;
    let (scope, device) = tenant(&store, "dedup").await;
    let rule = store
        .create_rule(&scope, None, &cpu_rule("CPU hot"))
        .await
        .expect("create");

    let key = dedup_key(rule.id, device, [("interface", "Gi0/1")]);
    let first = Evaluated {
        rule_id: rule.id,
        resource_id: device,
        dedup_key: key.clone(),
        phase: Phase::Pending,
        since: Utc::now(),
        at: Utc::now(),
        value: Some(94.0),
    };
    let stored = store
        .record_evaluation(&scope, &first)
        .await
        .expect("first");

    let second = Evaluated {
        phase: Phase::Firing,
        value: Some(96.0),
        at: Utc::now(),
        ..first.clone()
    };
    let updated = store
        .record_evaluation(&scope, &second)
        .await
        .expect("second");

    assert_eq!(updated.id, stored.id, "the same alert, not a second one");
    assert_eq!(updated.phase, Phase::Firing);
    assert_eq!(updated.last_value, Some(96.0));
    assert_eq!(
        store
            .rule_state(&scope, rule.id)
            .await
            .expect("state")
            .len(),
        1
    );
}

#[tokio::test]
async fn an_acknowledgement_survives_the_episode_it_was_made_in_and_no_longer() {
    let store = store().await;
    let (scope, device) = tenant(&store, "ack").await;
    let rule = store
        .create_rule(&scope, None, &cpu_rule("CPU hot"))
        .await
        .expect("create");
    let user = uops_core::ActorId::new();
    sqlx::query(
        "INSERT INTO app_user (id, org_id, email, display_name, password_hash) \
                 SELECT $1, org_id, $2, 'Ack Test', 'x' FROM tenant WHERE id = $3",
    )
    .bind(user.into_uuid())
    .bind(format!("ack-{}@example.com", user.into_uuid().simple()))
    .bind(scope.tenant_id().into_uuid())
    .execute(store.pool())
    .await
    .expect("user");

    let firing = Evaluated {
        rule_id: rule.id,
        resource_id: device,
        dedup_key: dedup_key(rule.id, device, []),
        phase: Phase::Firing,
        since: Utc::now(),
        at: Utc::now(),
        value: Some(95.0),
    };
    let alert = store
        .record_evaluation(&scope, &firing)
        .await
        .expect("fire");

    let acked = store
        .acknowledge_alert(&scope, alert.id, user, Utc::now())
        .await
        .expect("ack");
    assert_eq!(acked.acked_by, Some(user));
    assert_eq!(
        acked.phase,
        Phase::Firing,
        "an ack silences, it does not resolve"
    );

    // Still firing on the next cycle: the ack holds, or the same person is paged again
    // about something they are already holding a laptop over.
    let again = store
        .record_evaluation(
            &scope,
            &Evaluated {
                at: Utc::now(),
                ..firing.clone()
            },
        )
        .await
        .expect("still firing");
    assert_eq!(again.acked_by, Some(user));

    // Resolved, then firing again: a new problem, and the old acknowledgement must not
    // silence a page nobody has seen.
    store
        .record_evaluation(
            &scope,
            &Evaluated {
                phase: Phase::Resolved,
                at: Utc::now(),
                ..firing.clone()
            },
        )
        .await
        .expect("resolve");
    let refired = store
        .record_evaluation(
            &scope,
            &Evaluated {
                at: Utc::now(),
                ..firing
            },
        )
        .await
        .expect("refire");
    assert_eq!(
        refired.acked_by, None,
        "a new episode is not pre-acknowledged"
    );
}

#[tokio::test]
async fn the_active_list_is_what_is_wrong_now() {
    let store = store().await;
    let (scope, device) = tenant(&store, "active").await;
    let rule = store
        .create_rule(&scope, None, &cpu_rule("CPU hot"))
        .await
        .expect("create");

    let mut phases = [Phase::Firing, Phase::Pending, Phase::Resolved, Phase::Ok].into_iter();
    for (i, phase) in phases.by_ref().enumerate() {
        store
            .record_evaluation(
                &scope,
                &Evaluated {
                    rule_id: rule.id,
                    resource_id: device,
                    dedup_key: dedup_key(rule.id, device, [("n", i.to_string().as_str())]),
                    phase,
                    since: Utc::now(),
                    at: Utc::now(),
                    value: Some(1.0),
                },
            )
            .await
            .expect("record");
    }

    let active = store.active_alerts(&scope).await.expect("active");
    // Pending is included on purpose: nobody has been notified about it, and an operator
    // who has just been paged wants to see what is one evaluation away from paging too.
    assert_eq!(active.len(), 2, "{active:?}");
    assert!(
        active
            .iter()
            .all(|a| a.alert.phase.is_active() && a.alert.phase != Phase::Resolved)
    );
}

#[tokio::test]
async fn deleting_a_rule_deletes_what_it_believed() {
    // State rows naming a rule nobody can look up are alerts in the UI that cannot be
    // explained, acknowledged or silenced.
    let store = store().await;
    let (scope, device) = tenant(&store, "cascade").await;
    let rule = store
        .create_rule(&scope, None, &cpu_rule("CPU hot"))
        .await
        .expect("create");

    store
        .record_evaluation(
            &scope,
            &Evaluated {
                rule_id: rule.id,
                resource_id: device,
                dedup_key: dedup_key(rule.id, device, []),
                phase: Phase::Firing,
                since: Utc::now(),
                at: Utc::now(),
                value: Some(99.0),
            },
        )
        .await
        .expect("fire");

    store.delete_rule(&scope, rule.id).await.expect("delete");
    assert!(
        store
            .active_alerts(&scope)
            .await
            .expect("active")
            .is_empty()
    );
}

#[tokio::test]
async fn another_tenants_rule_is_not_found_rather_than_forbidden() {
    let store = store().await;
    let (mine, _) = tenant(&store, "mine").await;
    let (theirs, _) = tenant(&store, "theirs").await;

    let rule = store
        .create_rule(&theirs, None, &cpu_rule("Theirs"))
        .await
        .expect("create");

    assert!(matches!(
        store.alert_rule(&mine, rule.id).await,
        Err(uops_core::Error::NotFound { .. })
    ));
    assert!(matches!(
        store.update_rule(&mine, rule.id, &cpu_rule("Stolen")).await,
        Err(uops_core::Error::NotFound { .. })
    ));
    assert!(matches!(
        store.set_rule_enabled(&mine, rule.id, false).await,
        Err(uops_core::Error::NotFound { .. })
    ));
    assert!(matches!(
        store.delete_rule(&mine, rule.id).await,
        Err(uops_core::Error::NotFound { .. })
    ));

    assert!(store.alert_rule(&theirs, rule.id).await.is_ok());
}

#[tokio::test]
async fn disabling_a_rule_keeps_what_it_believed() {
    // Disabling is what an operator reaches for at 3am instead of deleting. A rule that
    // lost its history when it was silenced would make the next morning's question
    // unanswerable.
    let store = store().await;
    let (scope, device) = tenant(&store, "disable").await;
    let rule = store
        .create_rule(&scope, None, &cpu_rule("CPU hot"))
        .await
        .expect("create");

    store
        .record_evaluation(
            &scope,
            &Evaluated {
                rule_id: rule.id,
                resource_id: device,
                dedup_key: dedup_key(rule.id, device, []),
                phase: Phase::Firing,
                since: Utc::now(),
                at: Utc::now(),
                value: Some(99.0),
            },
        )
        .await
        .expect("fire");

    let off = store
        .set_rule_enabled(&scope, rule.id, false)
        .await
        .expect("disable");
    assert!(!off.enabled);
    assert_eq!(
        store
            .rule_state(&scope, rule.id)
            .await
            .expect("state")
            .len(),
        1
    );
}

#[tokio::test]
async fn a_rule_that_could_never_produce_a_number_is_refused() {
    // These save, list and look healthy while evaluating to nothing or to the wrong
    // thing. The symptom is an alert that never arrives, noticed during the incident it
    // was written for — so they are refused where somebody is still looking at the screen.
    let store = store().await;
    let (scope, _) = tenant(&store, "evaluable").await;

    // Two aggregates leave no way to say which one the threshold is about.
    let mut ambiguous = cpu_rule("Two numbers");
    ambiguous.query.aggregations.push(Aggregation {
        func: AggFunc::Max,
        field: Some(Field::Value),
        alias: "m".to_owned(),
    });
    assert!(matches!(
        store.create_rule(&scope, None, &ambiguous).await,
        Err(uops_core::Error::Invalid(_))
    ));

    // None, on the other hand, is a saved Log Explorer search: alert on how many rows it
    // returns. The evaluator supplies the count.
    let mut rows = cpu_rule("A search");
    rows.query.aggregations.clear();
    store
        .create_rule(&scope, None, &rows)
        .await
        .expect("a search alerts on its row count");

    // An absence rule over "everything", which includes every resource that has never
    // reported once.
    let mut everything = cpu_rule("Everything is quiet");
    everything.condition = Condition::Absence { after_seconds: 300 };
    everything.query.resources = uops_query::ResourceSelector::All;
    assert!(matches!(
        store.create_rule(&scope, None, &everything).await,
        Err(uops_core::Error::Invalid(_))
    ));

    // Named resources, and the same rule is fine.
    everything.query.resources = uops_query::ResourceSelector::Kind {
        kind: ResourceKind::Device,
    };
    store
        .create_rule(&scope, None, &everything)
        .await
        .expect("an absence rule that names what it watches");
}
