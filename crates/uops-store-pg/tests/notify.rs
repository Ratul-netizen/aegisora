//! Notification channels and the limits, against a real `PostgreSQL`.
//!
//! The limits are the reason this module exists, and they are only real in a database:
//! an in-process counter forgets on restart, and a rule trying to send ten thousand
//! notifications is also the thing most likely to make somebody restart the process.

use chrono::Utc;
use uops_core::{OrgId, TenantId, TenantScope};
use uops_store_pg::{Attempt, Config, NewChannel, Outcome, PgStore};

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
        .bind(format!("notify-org-{unique}"))
        .execute(store.pool())
        .await
        .expect("organization");
    sqlx::query("INSERT INTO tenant (id, org_id, name, slug) VALUES ($1, $2, $3, $4)")
        .bind(tenant.into_uuid())
        .bind(org.into_uuid())
        .bind(format!("notify-{slug}"))
        .bind(format!("{slug}-{unique}"))
        .execute(store.pool())
        .await
        .expect("tenant");

    TenantScope::collector(tenant)
}

fn webhook(name: &str, max_per_minute: i32) -> NewChannel {
    NewChannel {
        name: name.to_owned(),
        kind: "webhook".to_owned(),
        config: serde_json::json!({ "url": "http://example.invalid/hook" }),
        enabled: true,
        max_per_minute,
    }
}

fn attempt(channel: uuid::Uuid, n: usize) -> Attempt {
    Attempt {
        channel_id: channel,
        rule_id: uuid::Uuid::now_v7(),
        dedup_key: format!("rule/resource-{n}"),
        phase: "firing".to_owned(),
    }
}

/// SPEC §M4's acceptance criterion: *"notification rate limit prevents a storm from a
/// rule matching 5 000 resources"*.
///
/// The rule fires for all five thousand at once — the evaluator really does produce five
/// thousand decisions — and the channel lets twelve through. The other 4 988 are recorded
/// as refused rather than dropped, because "why did nobody get paged" and "why did I get
/// paged 5 000 times" are both questions this table has to answer.
#[tokio::test]
async fn a_rule_matching_five_thousand_resources_does_not_send_five_thousand_notifications() {
    let store = store().await;
    let scope = tenant(&store, "storm").await;
    let channel = store
        .create_channel(&scope, None, &webhook("ops", 12))
        .await
        .expect("channel");

    let mut sent = 0;
    let mut refused = 0;
    for n in 0..5_000 {
        let reservation = store
            .reserve_notification(&scope, &attempt(channel.id, n))
            .await
            .expect("reserve");
        if reservation.allowed() {
            sent += 1;
        } else {
            assert_eq!(reservation.outcome, Outcome::RateLimited);
            refused += 1;
        }
    }

    assert_eq!(sent, 12, "the channel's rate is what got through");
    assert_eq!(refused, 4_988);

    // And every one of them left a row, including the refusals.
    let recorded = store.notifications(&scope, 500).await.expect("list");
    assert_eq!(recorded.len(), 500, "capped, but present");
    assert!(
        recorded.iter().any(|r| r.outcome == Outcome::RateLimited),
        "a refusal that leaves no trace is indistinguishable from a rule that never fired"
    );
}

#[tokio::test]
async fn the_daily_budget_is_the_backstop_behind_the_rate() {
    // The rate limits how fast, the budget limits how many. A rule slow enough to stay
    // under the rate for ever is a slow leak, and the budget is what stops it.
    let store = store().await;
    let scope = tenant(&store, "budget").await;
    sqlx::query("UPDATE tenant SET notification_budget_per_day = $2 WHERE id = $1")
        .bind(scope.tenant_id().into_uuid())
        .bind(5_i32)
        .execute(store.pool())
        .await
        .expect("budget");

    // A generous rate, so the only thing that can stop this is the budget.
    let channel = store
        .create_channel(&scope, None, &webhook("ops", 60))
        .await
        .expect("channel");

    let mut outcomes = Vec::new();
    for n in 0..8 {
        outcomes.push(
            store
                .reserve_notification(&scope, &attempt(channel.id, n))
                .await
                .expect("reserve")
                .outcome,
        );
    }

    assert_eq!(outcomes.iter().filter(|o| **o == Outcome::Sent).count(), 5);
    assert_eq!(
        outcomes
            .iter()
            .filter(|o| **o == Outcome::OverBudget)
            .count(),
        3
    );
    // The budget is the bigger fact, so it is what the operator is told about — not that
    // one channel is briefly busy.
    assert_eq!(outcomes[7], Outcome::OverBudget);
}

#[tokio::test]
async fn a_failed_delivery_keeps_its_reservation_but_not_its_capacity() {
    // A webhook that is refusing everything must not retry at full speed — the
    // reservation is spent. But the rate counts deliveries, so a channel whose failures
    // have been marked is not also rate-limited by them.
    let store = store().await;
    let scope = tenant(&store, "failed").await;
    let channel = store
        .create_channel(&scope, None, &webhook("ops", 2))
        .await
        .expect("channel");

    let first = store
        .reserve_notification(&scope, &attempt(channel.id, 0))
        .await
        .expect("reserve");
    assert!(first.allowed());

    store
        .notification_failed(&scope, first.id, "502 Bad Gateway from the proxy")
        .await
        .expect("mark failed");

    // The failure freed the rate: two more may be attempted.
    for n in 1..3 {
        assert!(
            store
                .reserve_notification(&scope, &attempt(channel.id, n))
                .await
                .expect("reserve")
                .allowed(),
            "attempt {n}"
        );
    }
    assert!(
        !store
            .reserve_notification(&scope, &attempt(channel.id, 3))
            .await
            .expect("reserve")
            .allowed(),
        "and the rate still applies to the ones that succeeded"
    );

    let recorded = store.notifications(&scope, 10).await.expect("list");
    let failed = recorded
        .iter()
        .find(|r| r.id == first.id)
        .expect("the failed attempt");
    assert_eq!(failed.outcome, Outcome::Failed);
    assert!(failed.detail.contains("502"), "{}", failed.detail);
}

#[tokio::test]
async fn a_disabled_channel_is_not_found_rather_than_silently_skipped() {
    // A channel that is off sends nothing. The caller is told so rather than being handed
    // a reservation it cannot use — and a disabled channel answers exactly as another
    // tenant's does, for the same reason every other lookup here does.
    let store = store().await;
    let scope = tenant(&store, "disabled").await;
    let channel = store
        .create_channel(
            &scope,
            None,
            &NewChannel {
                enabled: false,
                ..webhook("off", 12)
            },
        )
        .await
        .expect("channel");

    assert!(matches!(
        store
            .reserve_notification(&scope, &attempt(channel.id, 0))
            .await,
        Err(uops_core::Error::NotFound { .. })
    ));
}

#[tokio::test]
async fn another_tenants_channel_cannot_be_sent_to() {
    let store = store().await;
    let mine = tenant(&store, "mine").await;
    let theirs = tenant(&store, "theirs").await;

    let channel = store
        .create_channel(&theirs, None, &webhook("theirs", 12))
        .await
        .expect("channel");

    assert!(matches!(
        store.channel(&mine, channel.id).await,
        Err(uops_core::Error::NotFound { .. })
    ));
    assert!(matches!(
        store
            .reserve_notification(&mine, &attempt(channel.id, 0))
            .await,
        Err(uops_core::Error::NotFound { .. })
    ));
    assert!(matches!(
        store.delete_channel(&mine, channel.id).await,
        Err(uops_core::Error::NotFound { .. })
    ));
}

#[tokio::test]
async fn a_channel_is_created_listed_replaced_and_deleted() {
    let store = store().await;
    let scope = tenant(&store, "crud").await;

    let created = store
        .create_channel(&scope, None, &webhook("ops", 12))
        .await
        .expect("create");
    assert_eq!(created.max_per_minute, 12);
    assert!(created.enabled);

    assert_eq!(store.channels(&scope).await.expect("list").len(), 1);

    let replaced = store
        .update_channel(
            &scope,
            created.id,
            &NewChannel {
                name: "ops (quieter)".to_owned(),
                max_per_minute: 3,
                ..webhook("ignored", 12)
            },
        )
        .await
        .expect("update");
    assert_eq!(replaced.id, created.id, "replacing keeps its identity");
    assert_eq!(replaced.max_per_minute, 3);

    store
        .delete_channel(&scope, created.id)
        .await
        .expect("delete");
    assert!(store.channels(&scope).await.expect("list").is_empty());
}

#[tokio::test]
async fn an_attempt_outlives_the_rule_it_was_about() {
    // Deliberate: state rows are deleted with their rule, and the record of having woken
    // somebody up at 3am must outlive the rule somebody deleted at 9am to stop it
    // happening again. There is no foreign key from here to a rule.
    let store = store().await;
    let scope = tenant(&store, "outlives").await;
    let channel = store
        .create_channel(&scope, None, &webhook("ops", 12))
        .await
        .expect("channel");

    let gone = uuid::Uuid::now_v7();
    store
        .reserve_notification(
            &scope,
            &Attempt {
                channel_id: channel.id,
                rule_id: gone,
                dedup_key: "deleted-rule/device".to_owned(),
                phase: "firing".to_owned(),
            },
        )
        .await
        .expect("reserve");

    let recorded = store.notifications(&scope, 10).await.expect("list");
    assert!(recorded.iter().any(|r| r.rule_id == gone));
    assert!(recorded[0].sent_at <= Utc::now());
}
