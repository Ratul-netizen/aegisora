//! Saved searches, against a real `PostgreSQL`.
//!
//! The parts a unit test cannot reach: the round trip of a `Query` AST through jsonb, the
//! CHECK that keeps the denormalised `signal` honest, the scoped uniqueness of a name,
//! and the refusal — at save time, not at open time — of a query the compiler cannot
//! answer.

use chrono::{Duration, Utc};
use uops_core::{OrgId, SavedSearchId, TenantId, TenantScope};
use uops_query::{Expr, Field, Query, SignalType, TextMode, TimeRange};
use uops_store_pg::{Config, NewSearch, PgStore};

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

/// A tenant of its own per test, so two runs cannot collide on a search name.
async fn tenant(store: &PgStore, slug: &str) -> TenantScope {
    let org = OrgId::new();
    let tenant = TenantId::new();
    let unique = tenant.into_uuid().simple().to_string();

    sqlx::query("INSERT INTO organization (id, name) VALUES ($1, $2)")
        .bind(org.into_uuid())
        .bind(format!("search-org-{unique}"))
        .execute(store.pool())
        .await
        .expect("organization");
    sqlx::query("INSERT INTO tenant (id, org_id, name, slug) VALUES ($1, $2, $3, $4)")
        .bind(tenant.into_uuid())
        .bind(org.into_uuid())
        .bind(format!("search-{slug}"))
        .bind(format!("{slug}-{unique}"))
        .execute(store.pool())
        .await
        .expect("tenant");

    TenantScope::collector(tenant)
}

/// The search an operator actually saves: errors mentioning a word, over a window.
fn errors_about(word: &str) -> Query {
    let end = Utc::now();
    Query::new(
        SignalType::Log,
        TimeRange::new(end - Duration::minutes(15), end),
    )
    .with_filter(Expr::Text {
        field: Field::Body,
        mode: TextMode::AnyToken,
        terms: vec![word.to_owned()],
    })
}

fn named(name: &str, query: Query) -> NewSearch {
    NewSearch {
        name: name.to_owned(),
        description: String::new(),
        query,
    }
}

#[tokio::test]
async fn a_search_round_trips_as_the_ast_it_was_saved_from() {
    // The whole feature in one assertion. What comes back is the same `Query` value, not
    // a reconstruction of one — which is what makes M4's "convert to an alert rule with
    // no edits" a copy rather than a translation.
    let store = store().await;
    let scope = tenant(&store, "round-trip").await;
    let query = errors_about("timeout");

    let saved = store
        .save_search(&scope, None, &named("Timeouts", query.clone()))
        .await
        .expect("save");
    assert_eq!(saved.name, "Timeouts");

    let read = store.saved_search(&scope, saved.id).await.expect("read");
    assert_eq!(read.query, query);
}

#[tokio::test]
async fn a_query_the_compiler_refuses_is_never_stored() {
    // `trace` is in the AST and has no table until M8. Stored, it would be a search that
    // lists, opens, and fails only when somebody runs it — during an incident, which is
    // the only time anybody opens a saved search in a hurry.
    let store = store().await;
    let scope = tenant(&store, "refused").await;

    let end = Utc::now();
    let traces = Query::new(
        SignalType::Trace,
        TimeRange::new(end - Duration::minutes(15), end),
    );

    assert!(matches!(
        store
            .save_search(&scope, None, &named("Traces", traces))
            .await,
        Err(uops_core::Error::Invalid(_))
    ));

    // And the same check guards an update, or a valid search could be edited into a
    // broken one and the guard on saving would mean nothing.
    let ok = store
        .save_search(&scope, None, &named("Fine", errors_about("bgp")))
        .await
        .expect("save");
    let metric_body = Query::new(
        SignalType::Metric,
        TimeRange::new(end - Duration::minutes(15), end),
    )
    .with_filter(Expr::Text {
        field: Field::Body,
        mode: TextMode::AnyToken,
        terms: vec!["nope".to_owned()],
    });
    assert!(matches!(
        store
            .update_search(&scope, ok.id, &named("Fine", metric_body))
            .await,
        Err(uops_core::Error::Invalid(_))
    ));
}

#[tokio::test]
async fn a_name_belongs_to_a_tenant_rather_than_to_the_installation() {
    let store = store().await;
    let mine = tenant(&store, "mine").await;
    let theirs = tenant(&store, "theirs").await;

    store
        .save_search(&mine, None, &named("BGP", errors_about("bgp")))
        .await
        .expect("save");

    // Twice in one tenant is a conflict...
    assert!(matches!(
        store
            .save_search(&mine, None, &named("BGP", errors_about("bgp")))
            .await,
        Err(uops_core::Error::Invalid(_))
    ));

    // ...and in another customer's tenant it is simply their own search. An MSP running
    // this for forty customers would otherwise have the first of them claim the obvious
    // names for everybody.
    store
        .save_search(&theirs, None, &named("BGP", errors_about("bgp")))
        .await
        .expect("the other tenant may use the same name");
}

#[tokio::test]
async fn another_tenants_search_is_not_found_rather_than_forbidden() {
    let store = store().await;
    let mine = tenant(&store, "not-found-mine").await;
    let theirs = tenant(&store, "not-found-theirs").await;

    let theirs_search = store
        .save_search(&theirs, None, &named("Theirs", errors_about("drop")))
        .await
        .expect("save");

    // 404 and never 403 on all three verbs: a distinguishable answer is a way to
    // enumerate another customer's searches by id.
    assert!(matches!(
        store.saved_search(&mine, theirs_search.id).await,
        Err(uops_core::Error::NotFound { .. })
    ));
    assert!(matches!(
        store
            .update_search(&mine, theirs_search.id, &named("Stolen", errors_about("x")))
            .await,
        Err(uops_core::Error::NotFound { .. })
    ));
    assert!(matches!(
        store.delete_search(&mine, theirs_search.id).await,
        Err(uops_core::Error::NotFound { .. })
    ));

    // A search that never existed answers identically, which is the point.
    assert!(matches!(
        store.saved_search(&mine, SavedSearchId::new()).await,
        Err(uops_core::Error::NotFound { .. })
    ));

    // And it is still there afterwards, so none of the above was a silent success.
    assert!(store.saved_search(&theirs, theirs_search.id).await.is_ok());
}

#[tokio::test]
async fn the_list_is_ordered_by_what_was_touched_last() {
    // The ordering an operator wants during an incident: the search they were editing
    // five minutes ago is the one they want back, not the one they wrote in March.
    let store = store().await;
    let scope = tenant(&store, "ordering").await;

    let first = store
        .save_search(&scope, None, &named("First", errors_about("one")))
        .await
        .expect("save");
    store
        .save_search(&scope, None, &named("Second", errors_about("two")))
        .await
        .expect("save");

    store
        .update_search(&scope, first.id, &named("First", errors_about("one again")))
        .await
        .expect("update");

    let names: Vec<String> = store
        .saved_searches(&scope)
        .await
        .expect("list")
        .into_iter()
        .map(|s| s.name)
        .collect();
    assert_eq!(names, ["First", "Second"]);
}

#[tokio::test]
async fn deleting_a_search_removes_it() {
    // Gone, not archived. Nothing downstream points at the row — M4 will copy the AST
    // into an alert rule rather than reference it — so there is nothing for a tombstone
    // to protect.
    let store = store().await;
    let scope = tenant(&store, "delete").await;

    let saved = store
        .save_search(&scope, None, &named("Temporary", errors_about("once")))
        .await
        .expect("save");

    store.delete_search(&scope, saved.id).await.expect("delete");
    assert!(matches!(
        store.saved_search(&scope, saved.id).await,
        Err(uops_core::Error::NotFound { .. })
    ));
    assert!(store.saved_searches(&scope).await.expect("list").is_empty());

    // The name is free again immediately, because uniqueness is over rows that exist.
    store
        .save_search(&scope, None, &named("Temporary", errors_about("again")))
        .await
        .expect("the name is free once the search is gone");
}
