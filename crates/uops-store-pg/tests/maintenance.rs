//! Maintenance windows, against a real `PostgreSQL`.
//!
//! The occurrence arithmetic is unit-tested in `uops_core::maintenance`, where it is pure
//! and the DST cases are reachable. This is the half that needs a database: the composite
//! foreign keys, the `CHECK` that a window targets exactly one thing, and — the reason the
//! feature waited for groups — that a device added to a group on Friday is covered by
//! Saturday's window without anybody editing the window.

use chrono::{DateTime, Duration, Utc};
use uops_core::{
    OrgId, Recurrence, ResourceId, ResourceKind, Schedule, SiteId, Suppression, Target, TenantId,
    TenantScope,
};
use uops_store_pg::{Config, NewGroup, NewResource, NewWindow, PgStore};

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

/// A tenant with a site and three devices in it.
async fn tenant(store: &PgStore, slug: &str) -> (TenantScope, SiteId, Vec<ResourceId>) {
    let org = OrgId::new();
    let tenant = TenantId::new();
    let site = SiteId::new();
    let unique = tenant.into_uuid().simple().to_string();

    sqlx::query("INSERT INTO organization (id, name) VALUES ($1, $2)")
        .bind(org.into_uuid())
        .bind(format!("mw-org-{unique}"))
        .execute(store.pool())
        .await
        .expect("organization");
    sqlx::query("INSERT INTO tenant (id, org_id, name, slug) VALUES ($1, $2, $3, $4)")
        .bind(tenant.into_uuid())
        .bind(org.into_uuid())
        .bind(format!("mw-{slug}"))
        .bind(format!("{slug}-{unique}"))
        .execute(store.pool())
        .await
        .expect("tenant");
    sqlx::query("INSERT INTO site (id, tenant_id, name) VALUES ($1, $2, $3)")
        .bind(site.into_uuid())
        .bind(tenant.into_uuid())
        .bind("dhaka")
        .execute(store.pool())
        .await
        .expect("site");

    let scope = TenantScope::collector(tenant);
    let mut ids = Vec::new();
    for n in 0..3 {
        let r = store
            .create_resource(
                &scope,
                &NewResource {
                    // Only the first two are at the site, so a site-targeted window has
                    // something it must not cover.
                    site_id: (n < 2).then_some(site),
                    ..NewResource::new(ResourceKind::Device, format!("rtr-{slug}-{n}"))
                },
            )
            .await
            .expect("device");
        ids.push(r.id);
    }
    (scope, site, ids)
}

fn at(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s)
        .expect("an instant")
        .with_timezone(&Utc)
}

fn once(target: Target, starts: &str, minutes: i64) -> NewWindow {
    NewWindow {
        reason: "planned firmware upgrade".to_owned(),
        target,
        schedule: Schedule {
            starts_at: at(starts),
            duration_minutes: minutes,
            timezone: "Asia/Dhaka".to_owned(),
            recurrence: Recurrence::Once,
            until: None,
        },
        suppression: Suppression::default(),
    }
}

#[tokio::test]
async fn a_window_round_trips_with_its_recurrence_intact() {
    let store = store().await;
    let (scope, site, _) = tenant(&store, "rt").await;

    let saved = store
        .schedule_maintenance(
            &scope,
            None,
            &NewWindow {
                reason: "Saturday change window".to_owned(),
                target: Target::Site(site),
                schedule: Schedule {
                    starts_at: at("2026-09-19T16:00:00Z"),
                    duration_minutes: 240,
                    timezone: "Asia/Dhaka".to_owned(),
                    recurrence: Recurrence::Weekly {
                        weekday: chrono::Weekday::Sat,
                    },
                    until: None,
                },
                suppression: Suppression {
                    alerts: false,
                    notifications: true,
                },
            },
        )
        .await
        .expect("schedule");

    let read = store
        .maintenance_window(&scope, saved.id)
        .await
        .expect("read back");

    // The weekday especially. An off-by-one in the 0-6 mapping moves every weekly window
    // in the estate by a day and nothing reports it, which is why it is written out
    // longhand in both directions rather than computed.
    assert_eq!(
        read.schedule.recurrence,
        Recurrence::Weekly {
            weekday: chrono::Weekday::Sat
        }
    );
    assert_eq!(read.schedule.timezone, "Asia/Dhaka");
    assert_eq!(read.schedule.duration_minutes, 240);
    assert_eq!(read.target, Target::Site(site));
    assert!(!read.suppression.alerts && read.suppression.notifications);
}

#[tokio::test]
async fn a_group_window_covers_whoever_is_in_the_group_at_the_time() {
    // The reason maintenance windows waited for resource groups. A device added on
    // Friday is covered by Saturday's window without anybody editing the window, because
    // membership is resolved when the question is asked rather than frozen when the
    // window was written.
    let store = store().await;
    let (scope, _, ids) = tenant(&store, "grp").await;

    let group = store
        .create_group(&scope, &NewGroup::new("Core Routers"))
        .await
        .expect("group");
    store
        .add_to_group(&scope, group.id, &ids[..1])
        .await
        .expect("add one");

    let window = store
        .schedule_maintenance(
            &scope,
            None,
            &once(Target::Group(group.id), "2026-09-19T16:00:00Z", 120),
        )
        .await
        .expect("schedule");

    let covered = store
        .covered_by(&scope, window.target)
        .await
        .expect("covered");
    assert_eq!(covered, vec![ids[0]]);

    // Friday's addition, covered by the same window.
    store
        .add_to_group(&scope, group.id, &ids[1..2])
        .await
        .expect("add another");
    let mut covered = store
        .covered_by(&scope, window.target)
        .await
        .expect("covered again");
    covered.sort_unstable();
    let mut want = ids[..2].to_vec();
    want.sort_unstable();
    assert_eq!(covered, want, "membership is resolved at read time");
}

#[tokio::test]
async fn a_site_window_covers_the_site_and_nothing_else() {
    let store = store().await;
    let (scope, site, ids) = tenant(&store, "site").await;

    let window = store
        .schedule_maintenance(
            &scope,
            None,
            &once(Target::Site(site), "2026-09-19T16:00:00Z", 120),
        )
        .await
        .expect("schedule");

    let mut covered = store
        .covered_by(&scope, window.target)
        .await
        .expect("covered");
    covered.sort_unstable();
    let mut want = ids[..2].to_vec();
    want.sort_unstable();
    assert_eq!(covered, want);
    assert!(
        !covered.contains(&ids[2]),
        "the device with no site is not covered"
    );
}

#[tokio::test]
async fn a_resource_window_does_not_expand_to_its_children() {
    // Deliberate. An interface is a resource with its own alerts, and silencing a device
    // must not silently silence forty-eight ports somebody may be watching individually.
    // When that turns out to be the wrong default it becomes a flag on the window, not a
    // change of meaning here — so it is asserted rather than left to be discovered.
    let store = store().await;
    let (scope, _, ids) = tenant(&store, "child").await;

    let child = store
        .create_resource(
            &scope,
            &NewResource {
                parent_id: Some(ids[0]),
                ..NewResource::new(ResourceKind::Interface, "Gi0/1")
            },
        )
        .await
        .expect("interface");

    let covered = store
        .covered_by(&scope, Target::Resource(ids[0]))
        .await
        .expect("covered");
    assert_eq!(covered, vec![ids[0]]);
    assert!(!covered.contains(&child.id));
}

#[tokio::test]
async fn maintenance_for_answers_the_question_the_alert_engine_asks() {
    let store = store().await;
    let (scope, site, ids) = tenant(&store, "ask").await;

    store
        .schedule_maintenance(
            &scope,
            None,
            &once(Target::Site(site), "2026-09-19T16:00:00Z", 120),
        )
        .await
        .expect("schedule");

    // Inside the window, a device at the site is in maintenance.
    let inside = store
        .maintenance_for(&scope, ids[0], at("2026-09-19T17:00:00Z"))
        .await
        .expect("during");
    assert_eq!(inside, Some(Suppression::default()));

    // A device that is not at the site is not, at the same instant.
    assert_eq!(
        store
            .maintenance_for(&scope, ids[2], at("2026-09-19T17:00:00Z"))
            .await
            .expect("elsewhere"),
        None
    );

    // And outside the window nobody is.
    assert_eq!(
        store
            .maintenance_for(&scope, ids[0], at("2026-09-19T19:00:00Z"))
            .await
            .expect("after"),
        None
    );
}

#[tokio::test]
async fn two_overlapping_windows_union_their_suppressions() {
    // Adding a second window must never make the estate noisier than one — that is the
    // opposite of what somebody scheduling maintenance is asking for. So if either says
    // to suppress alerts, alerts are suppressed.
    let store = store().await;
    let (scope, site, ids) = tenant(&store, "union").await;

    // One that only silences notifications.
    store
        .schedule_maintenance(
            &scope,
            None,
            &NewWindow {
                suppression: Suppression {
                    alerts: false,
                    notifications: true,
                },
                ..once(Target::Site(site), "2026-09-19T16:00:00Z", 120)
            },
        )
        .await
        .expect("first");

    assert_eq!(
        store
            .maintenance_for(&scope, ids[0], at("2026-09-19T17:00:00Z"))
            .await
            .expect("one window"),
        Some(Suppression {
            alerts: false,
            notifications: true
        })
    );

    // A second, overlapping, that silences everything.
    store
        .schedule_maintenance(
            &scope,
            None,
            &once(Target::Resource(ids[0]), "2026-09-19T16:30:00Z", 60),
        )
        .await
        .expect("second");

    assert_eq!(
        store
            .maintenance_for(&scope, ids[0], at("2026-09-19T17:00:00Z"))
            .await
            .expect("two windows"),
        Some(Suppression {
            alerts: true,
            notifications: true
        }),
        "the union, not the last one read"
    );
}

#[tokio::test]
async fn a_window_cannot_target_another_tenants_anything() {
    // The composite foreign keys refuse it. Asserted for all three target kinds, because
    // each one has its own key and a missing tenant column in any of them would be a
    // cross-tenant write that nothing else would catch.
    let store = store().await;
    let (a, _, _) = tenant(&store, "iso-a").await;
    let (b, their_site, their_devices) = tenant(&store, "iso-b").await;

    let their_group = store
        .create_group(&b, &NewGroup::new("Theirs"))
        .await
        .expect("group");

    for target in [
        Target::Site(their_site),
        Target::Resource(their_devices[0]),
        Target::Group(their_group.id),
    ] {
        let kind = target.kind();
        assert!(
            store
                .schedule_maintenance(&a, None, &once(target, "2026-09-19T16:00:00Z", 60))
                .await
                .is_err(),
            "a window must not target another tenant's {kind}"
        );
    }

    assert!(
        store
            .live_windows(&a, at("2026-09-19T00:00:00Z"))
            .await
            .expect("list")
            .is_empty(),
        "and nothing was written"
    );
}

#[tokio::test]
async fn another_tenants_window_is_not_found_rather_than_forbidden() {
    let store = store().await;
    let (a, _, _) = tenant(&store, "nf-a").await;
    let (b, their_site, _) = tenant(&store, "nf-b").await;

    let theirs = store
        .schedule_maintenance(
            &b,
            None,
            &once(Target::Site(their_site), "2026-09-19T16:00:00Z", 60),
        )
        .await
        .expect("schedule");

    assert!(matches!(
        store.maintenance_window(&a, theirs.id).await,
        Err(uops_core::Error::NotFound { .. })
    ));
    assert!(matches!(
        store.cancel_maintenance(&a, theirs.id).await,
        Err(uops_core::Error::NotFound { .. })
    ));
    // A window that never existed answers identically, which is the point.
    assert!(matches!(
        store.maintenance_window(&a, uuid::Uuid::now_v7()).await,
        Err(uops_core::Error::NotFound { .. })
    ));

    assert!(store.maintenance_window(&b, theirs.id).await.is_ok());
}

#[tokio::test]
async fn an_expired_window_is_not_live_and_a_standing_one_always_is() {
    let store = store().await;
    let (scope, site, _) = tenant(&store, "live").await;

    // Recurs daily, but stops.
    store
        .schedule_maintenance(
            &scope,
            None,
            &NewWindow {
                reason: "a fortnight of upgrades".to_owned(),
                schedule: Schedule {
                    recurrence: Recurrence::Daily,
                    until: Some(at("2026-09-21T00:00:00Z")),
                    ..once(Target::Site(site), "2026-09-19T16:00:00Z", 60).schedule
                },
                ..once(Target::Site(site), "2026-09-19T16:00:00Z", 60)
            },
        )
        .await
        .expect("bounded");

    assert_eq!(
        store
            .live_windows(&scope, at("2026-09-20T00:00:00Z"))
            .await
            .expect("during")
            .len(),
        1
    );
    assert!(
        store
            .live_windows(&scope, at("2026-10-01T00:00:00Z"))
            .await
            .expect("after")
            .is_empty(),
        "a window past its `until` can never open again"
    );

    // A standing Saturday-night change window has no `until` and is always live.
    store
        .schedule_maintenance(
            &scope,
            None,
            &NewWindow {
                reason: "standing change window".to_owned(),
                schedule: Schedule {
                    recurrence: Recurrence::Weekly {
                        weekday: chrono::Weekday::Sat,
                    },
                    ..once(Target::Site(site), "2026-09-19T16:00:00Z", 60).schedule
                },
                ..once(Target::Site(site), "2026-09-19T16:00:00Z", 60)
            },
        )
        .await
        .expect("standing");

    assert_eq!(
        store
            .live_windows(&scope, at("2030-01-01T00:00:00Z"))
            .await
            .expect("years later")
            .len(),
        1
    );
}

#[tokio::test]
async fn an_invalid_schedule_is_refused_before_the_database_sees_it() {
    let store = store().await;
    let (scope, site, _) = tenant(&store, "bad").await;

    // An unknown timezone. Checked against chrono-tz rather than PostgreSQL's own zone
    // table, which is a different list — a name the database accepted and the application
    // could not resolve would be a window that silently never opens.
    let err = store
        .schedule_maintenance(
            &scope,
            None,
            &NewWindow {
                schedule: Schedule {
                    timezone: "Mars/Olympus_Mons".to_owned(),
                    ..once(Target::Site(site), "2026-09-19T16:00:00Z", 60).schedule
                },
                ..once(Target::Site(site), "2026-09-19T16:00:00Z", 60)
            },
        )
        .await
        .expect_err("an unknown zone must be refused");
    assert!(format!("{err}").contains("Mars/Olympus_Mons"), "{err}");

    // A month of silence is almost always a mis-typed end date, and the consequence is an
    // estate that stops alerting with nobody noticing.
    assert!(
        store
            .schedule_maintenance(
                &scope,
                None,
                &once(Target::Site(site), "2026-09-19T16:00:00Z", 30 * 24 * 60),
            )
            .await
            .is_err(),
        "a window longer than a week must be refused"
    );

    // And a window nobody can explain later is one nobody dares delete.
    assert!(
        store
            .schedule_maintenance(
                &scope,
                None,
                &NewWindow {
                    reason: "   ".to_owned(),
                    ..once(Target::Site(site), "2026-09-19T16:00:00Z", 60)
                },
            )
            .await
            .is_err(),
        "a window needs a reason"
    );

    assert!(
        store
            .live_windows(&scope, at("2026-09-19T00:00:00Z"))
            .await
            .expect("list")
            .is_empty(),
        "and none of them was written"
    );
}

#[tokio::test]
async fn cancelling_a_window_stops_it_suppressing() {
    let store = store().await;
    let (scope, site, ids) = tenant(&store, "cancel").await;

    let window = store
        .schedule_maintenance(
            &scope,
            None,
            &once(Target::Site(site), "2026-09-19T16:00:00Z", 120),
        )
        .await
        .expect("schedule");
    assert!(
        store
            .maintenance_for(&scope, ids[0], at("2026-09-19T17:00:00Z"))
            .await
            .expect("before")
            .is_some()
    );

    store
        .cancel_maintenance(&scope, window.id)
        .await
        .expect("cancel");

    assert_eq!(
        store
            .maintenance_for(&scope, ids[0], at("2026-09-19T17:00:00Z"))
            .await
            .expect("after"),
        None
    );
}

#[tokio::test]
async fn a_window_does_not_outlive_what_it_covers() {
    // A window pointing at a group that no longer exists cannot be evaluated and cannot
    // be found in any UI, so it would sit in the table forever.
    let store = store().await;
    let (scope, _, ids) = tenant(&store, "cascade").await;

    let group = store
        .create_group(&scope, &NewGroup::new("Temporary"))
        .await
        .expect("group");
    store
        .add_to_group(&scope, group.id, &ids)
        .await
        .expect("add");
    store
        .schedule_maintenance(
            &scope,
            None,
            &once(Target::Group(group.id), "2026-09-19T16:00:00Z", 60),
        )
        .await
        .expect("schedule");

    store.delete_group(&scope, group.id).await.expect("delete");

    assert!(
        store
            .live_windows(&scope, at("2026-09-19T00:00:00Z"))
            .await
            .expect("list")
            .is_empty(),
        "the window went with the group"
    );
    // The devices did not.
    assert!(store.resource(&scope, ids[0]).await.is_ok());
}

#[tokio::test]
async fn a_weekly_window_opens_on_the_right_local_evening() {
    // End to end through the database: the 0-6 weekday column, the stored IANA zone, and
    // `is_open_at` agreeing. 22:00 Saturday in Dhaka is 16:00Z, and the window is still
    // open at 01:00 Sunday local — the case a naive "does it recur today" check reports
    // as alerting.
    let store = store().await;
    let (scope, site, ids) = tenant(&store, "weekly").await;

    store
        .schedule_maintenance(
            &scope,
            None,
            &NewWindow {
                reason: "Saturday night".to_owned(),
                schedule: Schedule {
                    recurrence: Recurrence::Weekly {
                        weekday: chrono::Weekday::Sat,
                    },
                    duration_minutes: 240,
                    ..once(Target::Site(site), "2026-09-19T16:00:00Z", 240).schedule
                },
                ..once(Target::Site(site), "2026-09-19T16:00:00Z", 240)
            },
        )
        .await
        .expect("schedule");

    let who = ids[0];
    for (probe, want, why) in [
        ("2026-09-19T16:30:00Z", true, "Saturday evening, Dhaka"),
        (
            "2026-09-19T19:30:00Z",
            true,
            "01:30 Sunday local, still inside the Saturday window",
        ),
        ("2026-09-19T20:30:00Z", false, "after it closes"),
        ("2026-09-26T16:30:00Z", true, "the following Saturday"),
        ("2026-09-23T16:30:00Z", false, "but not on a Wednesday"),
    ] {
        let is_open = store
            .maintenance_for(&scope, who, at(probe))
            .await
            .expect("ask")
            .is_some();
        assert_eq!(is_open, want, "{why}");
    }

    // A sanity check that the fixture is not simply always-true.
    let long_before = at("2026-09-19T16:00:00Z") - Duration::days(30);
    assert!(
        store
            .maintenance_for(&scope, ids[0], long_before)
            .await
            .expect("before the first occurrence")
            .is_none()
    );
}
