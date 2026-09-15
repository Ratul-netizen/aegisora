//! `Query` → parameterised `ClickHouse` SQL.
//!
//! The contract, from SPEC §M0.5:
//!
//! 1. **`tenant_id` is injected here, never by the caller.** [`compile`] takes a
//!    `&TenantScope` and writes the tenant predicate itself, first, on every statement
//!    it emits. There is no code path in this crate that produces SQL without one, and
//!    `TenantScope` cannot be conjured — it comes from an authenticated request or from
//!    a named system constructor. That chain is the single most important line of
//!    defence for tenant isolation in the product.
//! 2. Resources are resolved through `resource_alias` before codegen — see
//!    [`crate::resolve`].
//! 3. Every limit is capped at [`MAX_LIMIT`]; the tail has its own path and its own cap.
//! 4. Queries that cannot use the text index still compile, and return a
//!    [`QueryWarning`] saying so.
//! 5. The output is golden-tested, so any change to it is visible in a diff.

use uops_core::TenantScope;

use crate::ast::{
    AggFunc, Aggregation, CompareOp, Expr, Field, Query, SignalType, Sort, SortKey, TextMode, Value,
};
use crate::error::{Error, Result};
use crate::plan::{Col, TableKind, TablePlan, column_of, plan};
use crate::resolve::ResolvedResources;
use crate::sql::{Builder, Sql};
use crate::warning::QueryWarning;

/// Server-side row ceiling. A UI that wants more than this wants a different feature —
/// an export, or the tail — not a bigger number.
pub const MAX_LIMIT: u32 = 10_000;

/// The tail's own ceiling. Lower because these rows are pushed to a live view, not
/// paged through.
pub const TAIL_LIMIT: u32 = 1_000;

/// A compiled statement, everything the caller needs to run it and explain it.
#[derive(Clone, Debug)]
pub struct Compiled {
    pub sql: Sql,
    /// Empty when nothing about the query is worth mentioning.
    pub warnings: Vec<QueryWarning>,
    /// Which physical table the planner chose. Surfaced for the slow-query log: "which
    /// table did this actually read" is the first question asked of any slow query.
    pub table: &'static str,
}

/// Compile a query. The only way to produce telemetry SQL.
pub fn compile(q: &Query, scope: &TenantScope, resources: &ResolvedResources) -> Result<Compiled> {
    let mut cx = Cx::start(q, scope, resources)?;
    let rate = rate_usage(q)?;
    if rate && cx.plan.kind != TableKind::Base {
        // The window forced a rollup, and a rollup stores the *average* of a counter in
        // each bucket. The difference between two averages of a monotonic counter is a
        // number and it is not a rate of anything.
        //
        // This has to be checked here rather than in `column_of`, which never sees the
        // field: a rollup aggregation is chosen by its *function* — `avg` becomes
        // `avgMerge(avg_v)` whatever it was asked to average — so without this the query
        // compiled to an average of the rollup column over a rate subquery, and returned
        // a plausible number. Found by the test that expected it to be refused.
        return Err(Error::RollupCannotServe {
            what: "a counter rate".into(),
            table: cx.plan.table,
            why: "a rollup stores averages of the counter, and the difference between two                   averages is not a rate; ask over a window short enough to read raw points",
        });
    }

    cx.select_list(q)?;
    if rate {
        // The table is wrapped in a subquery that turns each series into rates, and the
        // predicates go *inside* it — see `rate_source`.
        cx.rate_source(q, scope, resources)?;
    } else {
        cx.from();
        cx.where_clause(q, scope, resources)?;
    }
    cx.group_by(q)?;
    cx.order_by(q)?;
    cx.limit(q.limit, MAX_LIMIT, q.offset);

    Ok(cx.finish())
}

/// Whether this query asks for a rate, and whether it asks for it somewhere legal.
///
/// [`Field::Rate`] is allowed only as an aggregation's field. The other positions are
/// refused rather than quietly ignored, and each for its own reason:
///
/// * **`GROUP BY rate`** would make one bucket per distinct floating-point rate, which
///   is a group per sample and never what anybody meant.
/// * **A filter on `rate`** would have to run before the rate exists — the predicates go
///   inside the window subquery, where the column is not yet computed. It could be made
///   to work with a second layer of nesting; it has no caller yet, and a query language
///   that accepts something it handles subtly differently from how it reads is worse
///   than one that says no.
/// * **`ORDER BY rate` as a field** is the same problem. Ordering by an *alias* of an
///   aggregation over a rate works, because that is an output column.
fn rate_usage(q: &Query) -> Result<bool> {
    let refused = |where_: &'static str| {
        Err(Error::Invalid(format!(
            "rate can only be aggregated, not used {where_};              ask for avg(rate) or max(rate) over a time bucket"
        )))
    };

    if q.group_by.contains(&Field::Rate) {
        return refused("as a group key");
    }
    if q.order_by
        .iter()
        .any(|s| matches!(&s.key, SortKey::Field { field } if *field == Field::Rate))
    {
        return refused("as a sort key");
    }
    if q.filter.as_ref().is_some_and(mentions_rate) {
        return refused("in a filter");
    }

    let used = q
        .aggregations
        .iter()
        .any(|a| a.field.as_ref() == Some(&Field::Rate));
    if used && q.signal != SignalType::Metric {
        return Err(Error::FieldNotAvailable {
            field: Field::Rate.label(),
            signal: q.signal.as_str(),
        });
    }
    Ok(used)
}

fn mentions_rate(e: &Expr) -> bool {
    match e {
        Expr::Compare { field, .. } | Expr::Text { field, .. } | Expr::Exists { field } => {
            *field == Field::Rate
        }
        Expr::And { of } | Expr::Or { of } => of.iter().any(mentions_rate),
        Expr::Not { of } => mentions_rate(of),
    }
}

/// The live tail — SPEC §M0.5 requirement 3, which keeps it off the normal path.
///
/// It is a separate entry point because it is a different physical query: ordered by
/// time rather than by resource, which is what the `p_by_time` projection exists to
/// serve (W1: 2 303 ms and 33.8M rows → 72 ms and 254K). `ClickHouse` picks that
/// projection itself when the `ORDER BY` matches, so there is no table to name here —
/// but only if nothing else in the statement gets in the way, which is why aggregation
/// and paging are refused rather than ignored.
pub fn compile_tail(
    q: &Query,
    scope: &TenantScope,
    resources: &ResolvedResources,
) -> Result<Compiled> {
    if q.is_aggregate() || !q.group_by.is_empty() {
        return Err(Error::Invalid(
            "the tail streams rows; aggregate the same filter with compile() instead".into(),
        ));
    }
    if q.offset != 0 {
        return Err(Error::Invalid(
            "the tail is a stream and cannot be paged; it has no stable offset".into(),
        ));
    }

    let mut cx = Cx::start(q, scope, resources)?;
    if cx.plan.kind != TableKind::Base {
        return Err(Error::Invalid("the tail reads raw rows only".into()));
    }

    cx.select_list(q)?;
    cx.from();
    cx.where_clause(q, scope, resources)?;
    // Time-ordered, not resource-ordered. This ORDER BY *is* the projection match.
    cx.b.push(" ORDER BY observed_at DESC");
    cx.limit(q.limit, TAIL_LIMIT, 0);

    Ok(cx.finish())
}

/// Codegen state: the statement under construction, the chosen table, warnings so far.
struct Cx {
    b: Builder,
    plan: TablePlan,
    warnings: Vec<QueryWarning>,
}

impl Cx {
    fn start(q: &Query, scope: &TenantScope, resources: &ResolvedResources) -> Result<Self> {
        // Resolution carries the tenant it ran under precisely so this check exists.
        // Without it, a resolved set from tenant A could be compiled under tenant B's
        // scope and the IN list would silently name another customer's resources.
        if resources.tenant() != scope.tenant_id() {
            return Err(Error::TenantMismatch);
        }
        if q.time.end <= q.time.start {
            return Err(Error::Invalid(
                "time range must be non-empty and forward: start < end".into(),
            ));
        }
        if q.limit == 0 {
            return Err(Error::Invalid("limit must be at least 1".into()));
        }

        let (plan, warnings) = plan(q)?;
        Ok(Self {
            b: Builder::new(),
            plan,
            warnings,
        })
    }

    fn finish(self) -> Compiled {
        Compiled {
            sql: self.b.finish(),
            warnings: self.warnings,
            table: self.plan.table,
        }
    }

    /// Write a column reference. Map keys are bound; column names come from an enum.
    fn col(&mut self, f: &Field) -> Result<()> {
        let c = column_of(f, &self.plan, &mut self.warnings)?;
        self.write_col(&c);
        Ok(())
    }

    fn write_col(&mut self, c: &Col) {
        match c {
            Col::Plain(name) => self.b.push(name),
            Col::Attr { map, key } => {
                self.b.push(map);
                self.b.push("[");
                self.b.bind("String", key.clone());
                self.b.push("]");
            }
            Col::Bucket { seconds, base } => {
                self.b.push("toStartOfInterval(");
                self.b.push(base);
                self.b.push(", INTERVAL ");
                // An integer from the AST, range-checked in group_by(). Not caller text.
                self.b.push(&seconds.to_string());
                self.b.push(" SECOND)");
            }
        }
    }

    /// `SELECT` — explicit columns, never `*`.
    ///
    /// W1 is the reason: `Map` columns decompress in full per row, so `SELECT *` pays
    /// for `attributes` on every query whether or not anything reads it.
    fn select_list(&mut self, q: &Query) -> Result<()> {
        self.b.push("SELECT ");

        if !q.is_aggregate() {
            let cols = match self.plan.signal {
                SignalType::Log => {
                    "tenant_id, resource_id, site_id, observed_at, ingested_at, source_kind, \
                     source_vendor, severity, facility, body, attributes, trace_id, span_id"
                }
                SignalType::Metric => {
                    "tenant_id, resource_id, site_id, metric, observed_at, ingested_at, value, \
                     unit, labels"
                }
                SignalType::Event => {
                    "tenant_id, resource_id, site_id, observed_at, ingested_at, source_kind, \
                     source_vendor, severity, event_category, event_type, summary, attributes"
                }
                SignalType::State => {
                    "tenant_id, resource_id, site_id, observed_at, ingested_at, severity, \
                     previous_status, current_status, reason, attributes"
                }
                SignalType::Trace | SignalType::Flow => unreachable!("refused by plan()"),
            };
            self.b.push(cols);
            return Ok(());
        }

        for (i, f) in q.group_by.iter().enumerate() {
            if i > 0 {
                self.b.push(", ");
            }
            self.col(f)?;
            // Positional aliases: the group key may be a map lookup or an interval
            // expression, and neither has a name a client could rely on. The AST order
            // is the contract.
            self.b.push(&format!(" AS g{i}"));
        }

        for (i, a) in q.aggregations.iter().enumerate() {
            if i > 0 || !q.group_by.is_empty() {
                self.b.push(", ");
            }
            self.aggregation(a)?;
        }
        Ok(())
    }

    fn aggregation(&mut self, a: &Aggregation) -> Result<()> {
        validate_alias(&a.alias)?;

        match self.plan.kind {
            TableKind::Base => self.base_aggregation(a)?,
            // Pre-aggregated rows hold states, so the function is a *Merge over the
            // stored state column rather than a fresh aggregation over values.
            TableKind::LogCounts => match a.func {
                AggFunc::Count => self.b.push("countMerge(cnt)"),
                _ => return Err(rollup_refuses(a.func, "logs_counts_5m")),
            },
            TableKind::MetricRollup => {
                let f = match a.func {
                    AggFunc::Count => "countMerge(cnt)",
                    AggFunc::Min => "minMerge(min_v)",
                    AggFunc::Max => "maxMerge(max_v)",
                    AggFunc::Avg => "avgMerge(avg_v)",
                    other => return Err(rollup_refuses(other, self.plan.table)),
                };
                self.b.push(f);
            }
        }

        self.b.push(" AS ");
        self.b.push(&a.alias);
        Ok(())
    }

    fn base_aggregation(&mut self, a: &Aggregation) -> Result<()> {
        if a.func == AggFunc::Count {
            if a.field.is_some() {
                return Err(Error::Invalid(
                    "count takes no field; use count_distinct to count values".into(),
                ));
            }
            self.b.push("count()");
            return Ok(());
        }

        let field = a
            .field
            .as_ref()
            .ok_or_else(|| Error::Invalid(format!("{} requires a field", a.func.label())))?;

        if let Some(q) = a.func.quantile() {
            self.b.push(&format!("quantile({q})("));
            self.col(field)?;
            self.b.push(")");
            return Ok(());
        }

        let f = match a.func {
            AggFunc::CountDistinct => "uniqExact(",
            AggFunc::Sum => "sum(",
            AggFunc::Min => "min(",
            AggFunc::Max => "max(",
            AggFunc::Avg => "avg(",
            AggFunc::Count | AggFunc::P50 | AggFunc::P95 | AggFunc::P99 => {
                unreachable!("handled above")
            }
        };
        self.b.push(f);
        self.col(field)?;
        self.b.push(")");
        Ok(())
    }

    /// The effective start of the window for the table that was chosen.
    ///
    /// Unchanged on a base table, floored to the bucket on a pre-aggregate.
    fn window_start(
        &self,
        requested: chrono::DateTime<chrono::Utc>,
    ) -> chrono::DateTime<chrono::Utc> {
        let width = i64::from(self.plan.stored_bucket_seconds);
        if width <= 0 {
            return requested;
        }
        let seconds = requested.timestamp();
        let floored = seconds - seconds.rem_euclid(width);
        chrono::DateTime::from_timestamp(floored, 0).unwrap_or(requested)
    }

    fn from(&mut self) {
        self.b.push(" FROM ");
        self.b.push(self.plan.table);
    }

    /// `FROM (…)` — the table wrapped in a subquery that turns each series into rates.
    ///
    /// # What the SQL does, and why it is exactly this
    ///
    /// SPEC §M2 requires that *"a 32-bit counter wrap produces no negative rate in any
    /// query"*. `uops_poll::counter` states the rule and explains it at length; reading
    /// it closely gives one simplification worth having, because it is the whole of the
    /// SQL below:
    ///
    /// > Width and the timing window decide only what a backwards step is *called* —
    /// > `Wrapped` or `Reset`. Neither ever produces a number. So the condition a query
    /// > needs is simply: forwards, or nothing.
    ///
    /// That is why nothing here knows whether a counter is 32-bit. It does not need to.
    /// A wrap cannot be repaired — on a 10 Gbps link a 32-bit byte counter goes round
    /// seventeen times in a minute, and `2^32 - previous + current` assumes exactly once
    /// — so the honest answer to a backwards step is `NULL`, which `ClickHouse`'s
    /// aggregates skip. A gap in the line, which an operator can see and ask about,
    /// rather than a wrong point, which they cannot.
    ///
    /// # Why the predicates go inside
    ///
    /// The tenant predicate, the time range and the resource set all belong in the inner
    /// query. Outside, they would filter rows *after* the window had run over the whole
    /// table — the tenant predicate is what prunes the primary key, so hoisting it out
    /// turns a granule scan into a full scan and, far worse, computes one customer's
    /// window frames over another's rows.
    ///
    /// # Why the first row of a series has no rate
    ///
    /// `lagInFrame` returns the column's default when there is no previous row — zero
    /// for a `Float64`, the epoch for a `DateTime64`. Both are values the arithmetic
    /// would accept, and the result would be a plausible-looking rate over fifty-six
    /// years. `row_number() > 1` is the guard, and it is the reason it is here rather
    /// than a comparison against the epoch.
    fn rate_source(
        &mut self,
        q: &Query,
        scope: &TenantScope,
        resources: &ResolvedResources,
    ) -> Result<()> {
        // Every column the outer query could name. Listed rather than `*` for the reason
        // the select list is: a `Map` column decompresses in full per row, so projecting
        // what is not read is paid for on every row. `ingested_at` and `tenant_id` are
        // here because `column_of` can emit them and a subquery that omitted one would
        // fail at runtime rather than at compile time.
        // Every column the outer query could name. Listed rather than `*` for the same
        // reason the select list is: a `Map` column decompresses in full per row, so
        // projecting what is not read is paid for on every row. `ingested_at` and
        // `tenant_id` are here because `column_of` can emit them, and a subquery that
        // omitted one would fail at runtime rather than at compile time.
        const CARRIED: &str = "tenant_id, resource_id, site_id, metric, observed_at, ingested_at, value, unit, labels";

        // The rate itself. `NULL` for anything that is not a step forwards in time and
        // in value — see the doc comment.
        const RATE: &str = ", if(rn > 1 AND observed_at > prev_at AND value >= prev_value, (value - prev_value) / ((toUnixTimestamp64Milli(observed_at) - toUnixTimestamp64Milli(prev_at)) / 1000), NULL) AS rate";

        // The previous sample in the same series, and which row this is within it.
        const LAGS: &str = ", lagInFrame(value) OVER w AS prev_value, lagInFrame(observed_at) OVER w AS prev_at, row_number() OVER w AS rn";

        // A series is one resource's one metric with one set of labels — an interface is
        // distinguished from its siblings by `network.interface.index` and by nothing
        // else in the row. Partitioning by less would difference two interfaces'
        // counters against each other, which produces a number rather than an error.
        const WINDOW: &str = " WINDOW w AS (PARTITION BY resource_id, metric, labels ORDER BY observed_at ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW)))";

        self.b.push(" FROM (SELECT ");
        self.b.push(CARRIED);
        self.b.push(RATE);
        self.b.push(" FROM (SELECT ");
        self.b.push(CARRIED);
        self.b.push(LAGS);
        self.b.push(" FROM ");
        self.b.push(self.plan.table);
        self.where_clause(q, scope, resources)?;
        self.b.push(WINDOW);
        Ok(())
    }

    /// The tenant predicate leads, always, on every statement this crate emits.
    fn where_clause(
        &mut self,
        q: &Query,
        scope: &TenantScope,
        resources: &ResolvedResources,
    ) -> Result<()> {
        self.b.push(" WHERE tenant_id = ");
        self.b.bind("UUID", scope.tenant_id().to_string());

        let time_col = self.plan.time_col;
        self.b.push(" AND ");
        self.b.push(time_col);
        self.b.push(" >= ");
        // On a pre-aggregate, the window's start is floored to the stored bucket width.
        // A bucket is the unit of storage: a window that starts partway through one
        // either includes it or loses it, and losing it means the leftmost bar of every
        // histogram silently disappears — an Explorer opened at 14:37 would drop the
        // 14:35 bucket. Found by running a real query against a real pre-aggregate;
        // both sides' unit tests were happy.
        self.b
            .bind("DateTime64(3)", fmt_ts(self.window_start(q.time.start)));
        self.b.push(" AND ");
        self.b.push(time_col);
        self.b.push(" < ");
        self.b.bind("DateTime64(3)", fmt_ts(q.time.end));

        match resources.ids() {
            None => {
                if self.plan.kind == TableKind::Base {
                    self.warnings.push(QueryWarning::FullTenantScan);
                }
            }
            Some([]) => {
                // Not an error and not an empty predicate: an empty IN list is a syntax
                // error, and *omitting* the predicate would read the whole tenant.
                self.b.push(" AND 1 = 0");
                self.warnings.push(QueryWarning::SelectorMatchedNothing);
            }
            Some(ids) => {
                self.b.push(" AND resource_id IN (");
                for (i, id) in ids.iter().enumerate() {
                    if i > 0 {
                        self.b.push(", ");
                    }
                    self.b.bind("UUID", id.to_string());
                }
                self.b.push(")");
            }
        }

        if let Some(f) = &q.filter {
            self.b.push(" AND ");
            self.expr(f)?;
        }
        Ok(())
    }

    fn expr(&mut self, e: &Expr) -> Result<()> {
        match e {
            Expr::And { of } => self.junction(of, " AND "),
            Expr::Or { of } => self.junction(of, " OR "),
            Expr::Not { of } => {
                self.b.push("NOT (");
                self.expr(of)?;
                self.b.push(")");
                Ok(())
            }
            Expr::Compare { field, cmp, value } => self.compare(field, *cmp, value),
            Expr::Text { field, mode, terms } => self.text(field, *mode, terms),
            Expr::Exists { field } => self.exists(field),
        }
    }

    fn junction(&mut self, of: &[Expr], sep: &str) -> Result<()> {
        if of.is_empty() {
            // An empty AND is `true` and an empty OR is `false`; a UI that sends one
            // by accident means neither. Refusing is the only safe reading.
            return Err(Error::Invalid(
                "empty and/or: a boolean group must have at least one operand".into(),
            ));
        }
        self.b.push("(");
        for (i, sub) in of.iter().enumerate() {
            if i > 0 {
                self.b.push(sep);
            }
            self.expr(sub)?;
        }
        self.b.push(")");
        Ok(())
    }

    fn compare(&mut self, field: &Field, cmp: CompareOp, value: &Value) -> Result<()> {
        if cmp.is_set_op() {
            let Value::List(items) = value else {
                return Err(Error::Invalid(format!(
                    "{} needs a list of values",
                    cmp.as_sql()
                )));
            };
            if items.is_empty() {
                return Err(Error::Invalid(format!("{} needs a value", cmp.as_sql())));
            }
            self.col(field)?;
            self.b.push(" ");
            self.b.push(cmp.as_sql());
            self.b.push(" (");
            for (i, v) in items.iter().enumerate() {
                if i > 0 {
                    self.b.push(", ");
                }
                self.bind_value(v)?;
            }
            self.b.push(")");
            return Ok(());
        }

        if matches!(value, Value::List(_)) {
            return Err(Error::Invalid(format!(
                "{} compares against one value, not a list",
                cmp.as_sql()
            )));
        }

        self.col(field)?;
        self.b.push(" ");
        self.b.push(cmp.as_sql());
        self.b.push(" ");
        self.bind_value(value)
    }

    /// Text search. The mode decides whether the index is used, and W1 decided which
    /// modes those are — see [`TextMode`].
    fn text(&mut self, field: &Field, mode: TextMode, terms: &[String]) -> Result<()> {
        if *field != Field::Body {
            return Err(Error::Invalid(format!(
                "text search is only defined on `body`; `{}` is not a text column",
                field.label()
            )));
        }
        if self.plan.signal != SignalType::Log {
            return Err(Error::FieldNotAvailable {
                field: "body".into(),
                signal: self.plan.signal.as_str(),
            });
        }
        if terms.iter().all(|t| t.trim().is_empty()) {
            return Err(Error::Invalid("text search needs a term".into()));
        }

        match mode {
            TextMode::AnyToken | TextMode::AllToken => {
                // VERIFIED against the running server, not against documentation.
                // `searchAny`/`searchAll` — the names this compiler emitted first, and
                // the ones the beta announcements used — do not exist in 26.8:
                // "Function with name `searchAll` does not exist (UNKNOWN_FUNCTION)".
                // Caught by running the golden files against a real schema; every unit
                // test on both sides passed the whole time.
                let f = if mode == TextMode::AnyToken {
                    "hasAnyTokens(body, ["
                } else {
                    "hasAllTokens(body, ["
                };
                self.b.push(f);
                self.terms(terms);
                self.b.push("])");
            }
            TextMode::Substring => {
                let needle = terms.join(" ");
                self.b.push("positionCaseInsensitive(body, ");
                self.b.bind("String", needle);
                self.b.push(") > 0");
                self.warnings.push(QueryWarning::NotIndexAccelerated {
                    mode: "substring".into(),
                    why: "a substring can start inside a token, so the token index \
                          cannot prune granules; measured at 1 928 ms over 100M rows"
                        .into(),
                });
            }
            TextMode::Phrase => {
                // Tokens first so the index prunes granules, then the phrase is
                // verified by scanning only what survived. Strictly better than the
                // bare scan, still not index-accelerated in the sense users expect.
                let phrase = terms.join(" ");
                let tokens = tokenize(&phrase);
                self.b.push("(");
                if !tokens.is_empty() {
                    self.b.push("hasAllTokens(body, [");
                    self.terms(&tokens);
                    self.b.push("]) AND ");
                }
                self.b.push("positionCaseInsensitive(body, ");
                self.b.bind("String", phrase);
                self.b.push(") > 0)");
                self.warnings.push(QueryWarning::NotIndexAccelerated {
                    mode: "phrase".into(),
                    why: "tokens narrow granules, then word order is verified by \
                          scanning them; measured at 2 378 ms over 100M rows"
                        .into(),
                });
            }
        }
        Ok(())
    }

    fn terms(&mut self, terms: &[String]) {
        for (i, t) in terms.iter().enumerate() {
            if i > 0 {
                self.b.push(", ");
            }
            self.b.bind("String", t.clone());
        }
    }

    fn exists(&mut self, field: &Field) -> Result<()> {
        let c = column_of(field, &self.plan, &mut self.warnings)?;
        match &c {
            Col::Attr { map, key } => {
                self.b.push("has(mapKeys(");
                self.b.push(map);
                self.b.push("), ");
                self.b.bind("String", key.clone());
                self.b.push(")");
                Ok(())
            }
            // A materialised attribute is an empty string when the key was absent, so
            // `exists` on one must ask the same question the map lookup would.
            Col::Plain(name) if is_string_column(name) => {
                self.b.push("notEmpty(");
                self.b.push(name);
                self.b.push(")");
                Ok(())
            }
            _ => Err(Error::Invalid(format!(
                "`{}` is always present; exists is only meaningful on attributes and \
                 text columns",
                field.label()
            ))),
        }
    }

    fn bind_value(&mut self, v: &Value) -> Result<()> {
        match v {
            Value::Bool(b) => self.b.bind("UInt8", if *b { "1" } else { "0" }),
            Value::Int(i) => self.b.bind("Int64", i.to_string()),
            Value::Float(f) => {
                if !f.is_finite() {
                    return Err(Error::Invalid(
                        "NaN and infinity are not comparable values".into(),
                    ));
                }
                self.b.bind("Float64", f.to_string());
            }
            Value::Str(s) => self.b.bind("String", s.clone()),
            Value::Uuid(u) => self.b.bind("UUID", u.to_string()),
            Value::Timestamp(t) => self.b.bind("DateTime64(3)", fmt_ts(*t)),
            Value::List(_) => {
                return Err(Error::Invalid(
                    "a list cannot be nested inside a list".into(),
                ));
            }
        }
        Ok(())
    }

    fn group_by(&mut self, q: &Query) -> Result<()> {
        if q.group_by.is_empty() {
            return Ok(());
        }
        if !q.is_aggregate() {
            return Err(Error::Invalid(
                "group_by without an aggregation has no meaning; add one".into(),
            ));
        }
        for f in &q.group_by {
            if let Field::TimeBucket { seconds } = f
                && (*seconds == 0 || *seconds > 86_400)
            {
                return Err(Error::Invalid(
                    "time bucket must be between 1 second and 1 day".into(),
                ));
            }
        }
        // By alias: the expression is already in the SELECT list, and repeating a map
        // lookup here would bind its key a second time.
        self.b.push(" GROUP BY ");
        for i in 0..q.group_by.len() {
            if i > 0 {
                self.b.push(", ");
            }
            self.b.push(&format!("g{i}"));
        }
        Ok(())
    }

    fn order_by(&mut self, q: &Query) -> Result<()> {
        if q.order_by.is_empty() {
            self.default_order(q);
            return Ok(());
        }
        self.b.push(" ORDER BY ");
        for (i, s) in q.order_by.iter().enumerate() {
            if i > 0 {
                self.b.push(", ");
            }
            self.sort_key(q, s)?;
            self.b.push(if s.desc { " DESC" } else { " ASC" });
        }
        Ok(())
    }

    fn sort_key(&mut self, q: &Query, s: &Sort) -> Result<()> {
        match &s.key {
            SortKey::Alias { alias } => {
                if !q.aggregations.iter().any(|a| &a.alias == alias) {
                    return Err(Error::Invalid(format!(
                        "order_by names `{alias}`, which is not an aggregation on this query"
                    )));
                }
                validate_alias(alias)?;
                self.b.push(alias);
                Ok(())
            }
            SortKey::Field { field } => {
                if q.is_aggregate() {
                    // Only grouped columns survive aggregation.
                    let Some(i) = q.group_by.iter().position(|g| g == field) else {
                        return Err(Error::Invalid(format!(
                            "order_by `{}` is neither grouped nor aggregated",
                            field.label()
                        )));
                    };
                    self.b.push(&format!("g{i}"));
                    return Ok(());
                }
                self.col(field)
            }
        }
    }

    /// What to order by when the caller did not say.
    ///
    /// Chosen to match the sort key rather than to be neutral: `(tenant_id,
    /// resource_id, observed_at)` means this ordering is free, and any other is a sort.
    fn default_order(&mut self, q: &Query) {
        if q.is_aggregate() {
            if let Some(i) = q
                .group_by
                .iter()
                .position(|f| matches!(f, Field::TimeBucket { .. }))
            {
                self.b.push(&format!(" ORDER BY g{i} ASC"));
            }
            return;
        }
        self.b.push(" ORDER BY resource_id ASC, ");
        self.b.push(self.plan.time_col);
        self.b.push(" DESC");
    }

    fn limit(&mut self, requested: u32, ceiling: u32, offset: u32) {
        let applied = requested.min(ceiling);
        if applied < requested {
            self.warnings
                .push(QueryWarning::LimitClamped { requested, applied });
        }
        self.b.push(&format!(" LIMIT {applied}"));
        if offset > 0 {
            self.b.push(&format!(" OFFSET {offset}"));
        }
    }
}

fn rollup_refuses(func: AggFunc, table: &'static str) -> Error {
    Error::RollupCannotServe {
        what: func.label().to_owned(),
        table,
        why: "the pre-aggregate stores only the states its materialised view declared",
    }
}

/// `ClickHouse` parses `DateTime64(3)` parameters from this form.
fn fmt_ts(t: chrono::DateTime<chrono::Utc>) -> String {
    t.format("%Y-%m-%d %H:%M:%S%.3f").to_string()
}

/// Must match the DDL's `tokenizer = 'splitByNonAlpha'`, or the tokens used to prune
/// granules will not be the tokens the index holds and a phrase search silently
/// misses rows.
fn tokenize(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Aggregation aliases are the one caller-supplied string that reaches the statement
/// text, because an alias is an identifier and identifiers cannot be parameters. So it
/// is validated against a whitelist rather than escaped.
fn validate_alias(alias: &str) -> Result<()> {
    let ok = !alias.is_empty()
        && alias.len() <= 63
        && alias
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && alias
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if ok {
        Ok(())
    } else {
        Err(Error::Invalid(format!(
            "`{alias}` is not a valid output name: lowercase letters, digits and \
             underscores, starting with a letter or underscore"
        )))
    }
}

fn is_string_column(name: &str) -> bool {
    matches!(
        name,
        "body"
            | "trace_id"
            | "span_id"
            | "unit"
            | "source_vendor"
            | "host_name"
            | "service_name"
            | "summary"
            | "reason"
    )
}
