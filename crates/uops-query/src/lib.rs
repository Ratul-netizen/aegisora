//! The query layer — SPEC §M0.5 and the `ClickHouse` half of §M0.6.
//!
//! One AST ([`Query`]), one compiler ([`compile`]), one place where `tenant_id` is
//! written into a statement. The UI, the API, saved alerts and the M6 text query
//! language are all supposed to be the *same* path to the database, and this crate is
//! what makes "supposed to" enforceable.
//!
//! # Three invariants, and where each is enforced
//!
//! | invariant | enforced by |
//! |---|---|
//! | SQL cannot exist without a tenant | [`compile`] takes `&TenantScope` and writes the predicate itself |
//! | Aliases are collapsed before codegen | [`ResolvedResources`] is the only accepted resource input, and only [`resolve`] produces one |
//! | No value is ever formatted into a statement | [`Sql`] binds parameters; the crate has no escaping function because it needs none |
//!
//! # What W1 put in here
//!
//! The benchmark is the reason several of these decisions are not the obvious ones:
//! the Explorer histogram is routed to a pre-aggregate, materialised semconv keys
//! compile to real columns, the tail is a separate entry point, and substring and
//! phrase search compile but warn. See [`plan`] and [`warning`].
//!
//! # Example
//!
//! ```
//! use chrono::Duration;
//! use uops_core::{TenantId, TenantScope};
//! use uops_query::{
//!     Field, Query, ResolvedResources, SignalType, TextMode, TimeRange, compile,
//! };
//! use uops_query::ast::Expr;
//!
//! let scope = TenantScope::system(TenantId::new());
//! let q = Query::new(SignalType::Log, TimeRange::last(Duration::hours(1)))
//!     .with_filter(Expr::Text {
//!         field: Field::Body,
//!         mode: TextMode::AllToken,
//!         terms: vec!["link".into(), "down".into()],
//!     })
//!     .with_limit(100);
//!
//! let out = compile(&q, &scope, &ResolvedResources::whole_tenant(&scope)).unwrap();
//!
//! assert!(out.sql.text().starts_with("SELECT tenant_id, resource_id"));
//! assert!(out.sql.text().contains("WHERE tenant_id = {p0:UUID}"));
//! assert!(out.sql.text().contains("hasAllTokens(body, [{p3:String}, {p4:String}])"));
//! ```

pub mod ast;
pub mod compile;
pub mod error;
pub mod plan;
pub mod resolve;
pub mod sql;
pub mod warning;

pub use ast::{
    AggFunc, Aggregation, CompareOp, Expr, Field, Query, ResourceSelector, SignalType, Sort,
    SortKey, TextMode, TimeRange, Value,
};
pub use compile::{Compiled, MAX_LIMIT, TAIL_LIMIT, compile, compile_tail};
pub use error::{Error, Result};
pub use plan::{TableKind, TablePlan};
pub use resolve::{ResolvedResources, ResourceCatalog, resolve};
pub use sql::{Param, Sql};
pub use warning::{QueryWarning, WarningView};

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use uops_core::{ResourceId, TenantId, TenantScope};

    use super::*;

    fn scope() -> TenantScope {
        TenantScope::system(TenantId::new())
    }

    fn window() -> TimeRange {
        TimeRange::new(
            Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
            Utc.timestamp_opt(1_700_003_600, 0).unwrap(),
        )
    }

    fn all(s: &TenantScope) -> ResolvedResources {
        ResolvedResources::whole_tenant(s)
    }

    #[test]
    fn every_statement_carries_the_tenant_predicate() {
        // The invariant the whole crate exists for, asserted across every shape of
        // query the compiler can emit rather than on one example. A future code path
        // that forgets the predicate has to get past this test to exist.
        let s = scope();
        let r = all(&s);
        let tenant = s.tenant_id().to_string();

        let mut histogram = Query::new(SignalType::Log, window());
        histogram.aggregations = vec![Aggregation {
            func: AggFunc::Count,
            field: None,
            alias: "c".into(),
        }];
        histogram.group_by = vec![Field::TimeBucket { seconds: 300 }];

        let mut metrics = Query::new(SignalType::Metric, window());
        metrics.aggregations = vec![Aggregation {
            func: AggFunc::Avg,
            field: Some(Field::Value),
            alias: "v".into(),
        }];

        // Grouping on an unmaterialised key binds a parameter *before* the WHERE
        // clause, which shifts every placeholder number. Included because that shift
        // is exactly what an over-literal assertion here would miss.
        let mut by_attr = Query::new(SignalType::Log, window());
        by_attr.aggregations = vec![Aggregation {
            func: AggFunc::Count,
            field: None,
            alias: "c".into(),
        }];
        by_attr.group_by = vec![Field::Attr {
            key: "device.role".into(),
        }];

        let queries = [
            Query::new(SignalType::Log, window()),
            by_attr,
            Query::new(SignalType::Event, window()),
            Query::new(SignalType::State, window()),
            histogram,
            metrics,
        ];

        for q in &queries {
            for out in [
                compile(q, &s, &r).unwrap(),
                // The tail is a separate entry point, so it needs its own assertion.
                compile_tail(&Query::new(SignalType::Log, window()), &s, &r).unwrap(),
            ] {
                // Not always p0: a map-lookup group key binds its key in the SELECT
                // list, before the WHERE clause is written. What matters is that the
                // predicate exists and binds *this* tenant.
                let Some(name) = out
                    .sql
                    .text()
                    .split_once("WHERE tenant_id = {")
                    .and_then(|(_, rest)| rest.split_once(':'))
                    .map(|(n, _)| n.to_owned())
                else {
                    panic!("no tenant predicate in: {}", out.sql.text())
                };
                assert_eq!(out.sql.params()[&name].value, tenant);
            }
        }
    }

    #[test]
    fn a_resolved_set_from_another_tenant_is_refused() {
        let a = scope();
        let b = scope();
        let err = compile(&Query::new(SignalType::Log, window()), &a, &all(&b)).unwrap_err();
        assert!(matches!(err, Error::TenantMismatch), "{err}");
    }

    #[test]
    fn an_attribute_key_is_bound_not_interpolated() {
        // Attribute keys are user data — they arrive from whatever a device put in a
        // syslog message. This is the one place caller text could plausibly reach the
        // statement, so it gets the hostile input.
        let s = scope();
        let q = Query::new(SignalType::Log, window()).with_filter(Expr::Compare {
            field: Field::Attr {
                key: "x'] = 1 OR attributes['y".into(),
            },
            cmp: CompareOp::Eq,
            value: Value::Str("v".into()),
        });

        let out = compile(&q, &s, &all(&s)).unwrap();
        assert!(
            out.sql
                .text()
                .contains("attributes[{p3:String}] = {p4:String}"),
            "{}",
            out.sql.text()
        );
        assert!(!out.sql.text().contains("OR attributes"));
    }

    #[test]
    fn an_alias_that_is_not_an_identifier_is_rejected() {
        // Aliases cannot be parameters, so they are validated instead. This is the
        // complement to the test above: the one string that does reach the text.
        let s = scope();
        let mut q = Query::new(SignalType::Log, window());
        q.aggregations = vec![Aggregation {
            func: AggFunc::Count,
            field: None,
            alias: "c, 1 AS x".into(),
        }];
        assert!(matches!(
            compile(&q, &s, &all(&s)).unwrap_err(),
            Error::Invalid(_)
        ));
    }

    #[test]
    fn the_limit_ceiling_is_applied_and_reported() {
        let s = scope();
        let q = Query::new(SignalType::Log, window()).with_limit(5_000_000);
        let out = compile(&q, &s, &all(&s)).unwrap();

        assert!(out.sql.text().ends_with(&format!(" LIMIT {MAX_LIMIT}")));
        assert!(out.warnings.contains(&QueryWarning::LimitClamped {
            requested: 5_000_000,
            applied: MAX_LIMIT,
        }));
    }

    #[test]
    fn an_empty_selector_returns_nothing_rather_than_everything() {
        // The failure mode this guards is not a crash. It is a query that was meant to
        // read one decommissioned device and instead reads the entire tenant.
        let s = scope();
        let empty = ResolvedResources::for_test(s.tenant_id(), Some(Vec::new()));
        let out = compile(&Query::new(SignalType::Log, window()), &s, &empty).unwrap();

        assert!(out.sql.text().contains(" AND 1 = 0"), "{}", out.sql.text());
        assert!(
            out.warnings.contains(&QueryWarning::SelectorMatchedNothing),
            "and it must say so, or it reads as a broken query"
        );
    }

    #[test]
    fn resources_compile_to_a_sorted_in_list() {
        let s = scope();
        let ids: Vec<ResourceId> = (0..3).map(|_| ResourceId::new()).collect();
        let r = ResolvedResources::for_test(s.tenant_id(), Some(ids.clone()));
        let out = compile(&Query::new(SignalType::Log, window()), &s, &r).unwrap();

        assert!(
            out.sql
                .text()
                .contains("resource_id IN ({p3:UUID}, {p4:UUID}, {p5:UUID})"),
            "{}",
            out.sql.text()
        );
        assert!(!out.warnings.contains(&QueryWarning::FullTenantScan));
    }

    #[test]
    fn the_tail_orders_by_time_so_the_projection_can_serve_it() {
        // W1 measured this exact difference: 2 303 ms reading 33.8M rows, against
        // 72 ms reading 254K once p_by_time existed. ClickHouse only picks the
        // projection when the ORDER BY matches it, so the clause is the fix.
        let s = scope();
        let q = Query::new(SignalType::Log, window()).with_limit(50_000);
        let out = compile_tail(&q, &s, &all(&s)).unwrap();

        assert!(out.sql.text().contains(" ORDER BY observed_at DESC"));
        // The tail has its own, lower ceiling: these rows are pushed to a live view
        // rather than paged through.
        assert!(out.sql.text().ends_with(&format!(" LIMIT {TAIL_LIMIT}")));
        assert!(out.warnings.contains(&QueryWarning::FullTenantScan));
    }

    #[test]
    fn the_tail_refuses_to_aggregate_or_page() {
        let s = scope();
        let mut agg = Query::new(SignalType::Log, window());
        agg.aggregations = vec![Aggregation {
            func: AggFunc::Count,
            field: None,
            alias: "c".into(),
        }];
        assert!(compile_tail(&agg, &s, &all(&s)).is_err());

        let mut paged = Query::new(SignalType::Log, window());
        paged.offset = 100;
        assert!(compile_tail(&paged, &s, &all(&s)).is_err());
    }

    #[test]
    fn a_backwards_window_is_rejected() {
        let s = scope();
        let w = window();
        let q = Query::new(SignalType::Log, TimeRange::new(w.end, w.start));
        assert!(matches!(
            compile(&q, &s, &all(&s)).unwrap_err(),
            Error::Invalid(_)
        ));
    }

    #[test]
    fn index_accelerated_search_produces_no_warning_and_the_slow_modes_do() {
        let s = scope();
        let search = |mode| {
            let q = Query::new(SignalType::Log, window()).with_filter(Expr::Text {
                field: Field::Body,
                mode,
                terms: vec!["interface down".into()],
            });
            compile(&q, &s, &all(&s)).unwrap()
        };

        for mode in [TextMode::AnyToken, TextMode::AllToken] {
            assert!(
                !search(mode)
                    .warnings
                    .iter()
                    .any(|w| matches!(w, QueryWarning::NotIndexAccelerated { .. })),
                "{mode:?} is index-accelerated"
            );
        }
        for mode in [TextMode::Substring, TextMode::Phrase] {
            assert!(
                search(mode)
                    .warnings
                    .iter()
                    .any(|w| matches!(w, QueryWarning::NotIndexAccelerated { .. })),
                "{mode:?} reads the whole tenant and the UI must be able to say so"
            );
        }
    }

    #[test]
    fn phrase_search_prunes_with_the_same_tokenizer_the_index_uses() {
        // The DDL says `tokenizer = 'splitByNonAlpha'`. If these tokens are produced
        // any other way, hasAllTokens() prunes granules that do contain the phrase and
        // the search silently misses rows — the worst kind of bug this crate can ship.
        let s = scope();
        let q = Query::new(SignalType::Log, window()).with_filter(Expr::Text {
            field: Field::Body,
            mode: TextMode::Phrase,
            terms: vec!["%LINK-3-UPDOWN: changed state".into()],
        });
        let out = compile(&q, &s, &all(&s)).unwrap();

        let bound: Vec<&str> = out
            .sql
            .params()
            .values()
            .map(|p| p.value.as_str())
            .collect();
        for token in ["LINK", "3", "UPDOWN", "changed", "state"] {
            assert!(bound.contains(&token), "missing token {token}: {bound:?}");
        }
        assert!(out.sql.text().contains("hasAllTokens(body, ["));
        assert!(out.sql.text().contains("positionCaseInsensitive(body, "));
    }

    #[test]
    fn text_search_on_a_signal_without_a_body_is_rejected() {
        let s = scope();
        let q = Query::new(SignalType::Metric, window()).with_filter(Expr::Text {
            field: Field::Body,
            mode: TextMode::AnyToken,
            terms: vec!["x".into()],
        });
        assert!(compile(&q, &s, &all(&s)).is_err());
    }

    #[test]
    fn ordering_by_something_that_survived_neither_grouping_nor_aggregation_is_rejected() {
        let s = scope();
        let mut q = Query::new(SignalType::Log, window());
        q.aggregations = vec![Aggregation {
            func: AggFunc::Count,
            field: None,
            alias: "c".into(),
        }];
        q.group_by = vec![Field::Severity];
        q.order_by = vec![Sort {
            key: SortKey::Field { field: Field::Body },
            desc: true,
        }];
        assert!(matches!(
            compile(&q, &s, &all(&s)).unwrap_err(),
            Error::Invalid(_)
        ));
    }

    #[test]
    fn a_pre_aggregate_query_starts_at_a_bucket_boundary() {
        // A bucket is the unit of storage. A window starting partway through one either
        // includes it or loses it, and losing it drops the leftmost bar of every
        // histogram — an Explorer opened at 14:37 would silently omit 14:35.
        //
        // Found by running a real histogram against a real pre-aggregate: it returned
        // nothing at all, because every bucket in the fixture began before the window
        // did. Both sides' unit tests passed throughout.
        let s = scope();
        let unaligned = TimeRange::new(
            Utc.timestamp_opt(1_700_000_000, 0).unwrap(), // 22:13:20
            Utc.timestamp_opt(1_700_003_600, 0).unwrap(),
        );

        let mut histogram = Query::new(SignalType::Log, unaligned);
        histogram.aggregations = vec![Aggregation {
            func: AggFunc::Count,
            field: None,
            alias: "c".into(),
        }];
        histogram.group_by = vec![Field::TimeBucket { seconds: 300 }];

        let out = compile(&histogram, &s, &all(&s)).unwrap();
        assert_eq!(out.table, "logs_counts_5m");
        assert_eq!(
            out.sql.params()["p1"].value,
            "2023-11-14 22:10:00.000",
            "the window must be floored to the stored five-minute bucket"
        );

        // A base-table query is untouched: there is no bucket to align to, and moving
        // the window would return rows the caller did not ask for.
        let raw = Query::new(SignalType::Log, unaligned);
        let out = compile(&raw, &s, &all(&s)).unwrap();
        assert_eq!(out.sql.params()["p1"].value, "2023-11-14 22:13:20.000");
    }

    #[test]
    fn the_default_ordering_follows_the_sort_key() {
        // (tenant_id, resource_id, observed_at) is the sort key on every telemetry
        // table, so this ordering is free and any other is a sort.
        let s = scope();
        let out = compile(&Query::new(SignalType::Log, window()), &s, &all(&s)).unwrap();
        assert!(
            out.sql
                .text()
                .contains(" ORDER BY resource_id ASC, observed_at DESC"),
            "{}",
            out.sql.text()
        );
    }
}
