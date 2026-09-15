//! `IdentityStore` over PostgreSQL.
//!
//! The resolver's rules are tested against an in-memory store, exhaustively and without
//! a database. This file is tested for the one thing only it can independently get
//! wrong: whether its SQL agrees with the schema. So the interesting content here is
//! not logic — it is which constraint each statement is leaning on.
//!
//! | operation | what the schema does |
//! |---|---|
//! | [`IdentityStore::lookup`] | `UNIQUE (tenant_id, kind, value)` makes resolution an index probe, not a scan |
//! | `attach_identifiers` | `ON CONFLICT DO UPDATE last_seen` — a repeat is not an error, and the owner does not change |
//! | `reassign_identifiers` | `ON CONFLICT DO UPDATE` — the one path that takes an identifier, for replaced hardware |
//! | [`IdentityStore::merge`] | one transaction: move the identifiers, write the alias, let the trigger collapse chains |
//! | [`IdentityStore::split`] | one transaction: move them back, drop the alias |
//!
//! Merge and split are transactional because half of either is worse than neither: a
//! merge that moved the identifiers but did not write the alias would orphan every piece
//! of telemetry recorded under the old `resource_id`.

use async_trait::async_trait;
use sqlx::types::Json;
use uops_core::{
    ActorId, DecisionId, Error, Identifier, IdentifierKind, Match, ResourceId, ResourceKind,
    Result, TenantId,
};
use uops_identity::{Decision, DecisionOutcome, Hit, IdentityStore, ReviewItem};

use crate::error::map;
use crate::store::PgStore;

/// Split identifiers into parallel arrays.
///
/// `unnest` of two arrays is how a variable-length set of `(kind, value)` pairs reaches
/// a single statement without building SQL from strings — and keeping `kind` as its
/// enum type rather than casting to text is what lets the join use the unique index,
/// which is the whole reason resolution is fast.
fn columns(identifiers: &[Identifier]) -> (Vec<IdentifierKind>, Vec<String>) {
    identifiers
        .iter()
        .map(|i| (i.kind, i.value.clone()))
        .unzip()
}

#[async_trait]
impl IdentityStore for PgStore {
    async fn lookup(&self, tenant: TenantId, identifiers: &[Identifier]) -> Result<Vec<Hit>> {
        if identifiers.is_empty() {
            return Ok(Vec::new());
        }
        let (kinds, values) = columns(identifiers);

        let rows = sqlx::query!(
            r#"
            SELECT ri.resource_id AS "resource_id: ResourceId",
                   ri.kind        AS "kind: IdentifierKind",
                   ri.value
              FROM unnest($2::identifier_kind[], $3::text[]) AS given(kind, value)
              JOIN resource_identifier ri
                     ON ri.tenant_id = $1
                    AND ri.kind = given.kind
                    AND ri.value = given.value
            "#,
            tenant as TenantId,
            &kinds as &[IdentifierKind],
            &values,
        )
        .fetch_all(self.pool())
        .await
        .map_err(|e| map("identifier", String::new(), e))?;

        Ok(rows
            .into_iter()
            .map(|r| Hit {
                resource_id: r.resource_id,
                identifier: Identifier::new(r.kind, r.value),
            })
            .collect())
    }

    async fn identifiers_of(
        &self,
        tenant: TenantId,
        resource: ResourceId,
    ) -> Result<Vec<Identifier>> {
        let rows = sqlx::query!(
            r#"
            SELECT kind AS "kind: IdentifierKind", value
              FROM resource_identifier
             WHERE tenant_id = $1 AND resource_id = $2
             ORDER BY kind, value
            "#,
            tenant as TenantId,
            resource as ResourceId,
        )
        .fetch_all(self.pool())
        .await
        .map_err(|e| map("identifier", resource.to_string(), e))?;

        Ok(rows
            .into_iter()
            .map(|r| Identifier::new(r.kind, r.value))
            .collect())
    }

    async fn create_provisional(
        &self,
        tenant: TenantId,
        kind: ResourceKind,
        name: &str,
    ) -> Result<ResourceId> {
        let id = ResourceId::new();
        sqlx::query!(
            r#"
            INSERT INTO resource (id, tenant_id, kind, name)
            VALUES ($1, $2, $3, $4)
            "#,
            id as ResourceId,
            tenant as TenantId,
            kind as ResourceKind,
            name,
        )
        .execute(self.pool())
        .await
        .map_err(|e| map("resource", id.to_string(), e))?;
        Ok(id)
    }

    async fn attach_identifiers(
        &self,
        tenant: TenantId,
        resource: ResourceId,
        identifiers: &[Identifier],
        source: &str,
    ) -> Result<()> {
        if identifiers.is_empty() {
            return Ok(());
        }
        let (kinds, values) = columns(identifiers);
        let confidences: Vec<f32> = identifiers
            .iter()
            .map(|i| i.kind.base_confidence())
            .collect();

        // DO UPDATE on last_seen rather than DO NOTHING: the identifier stays with
        // whichever resource already owns it — that is the invariant — but the row
        // still records that it was seen again, which is what makes a stale identifier
        // distinguishable from a live one later.
        sqlx::query!(
            r#"
            INSERT INTO resource_identifier
                (id, tenant_id, resource_id, kind, value, confidence, source)
            SELECT gen_random_uuid(), $1, $2, given.kind, given.value, given.confidence, $6
              FROM unnest($3::identifier_kind[], $4::text[], $5::real[])
                     AS given(kind, value, confidence)
            ON CONFLICT (tenant_id, kind, value)
            DO UPDATE SET last_seen = now()
            "#,
            tenant as TenantId,
            resource as ResourceId,
            &kinds as &[IdentifierKind],
            &values,
            &confidences,
            source,
        )
        .execute(self.pool())
        .await
        .map_err(|e| map("identifier", resource.to_string(), e))?;
        Ok(())
    }

    async fn reassign_identifiers(
        &self,
        tenant: TenantId,
        resource: ResourceId,
        identifiers: &[Identifier],
        source: &str,
    ) -> Result<()> {
        if identifiers.is_empty() {
            return Ok(());
        }
        let (kinds, values) = columns(identifiers);
        let confidences: Vec<f32> = identifiers
            .iter()
            .map(|i| i.kind.base_confidence())
            .collect();

        // The dangerous one: DO UPDATE SET resource_id takes the identifier from
        // whoever holds it. Reached only from a tier-1 contradiction, where the serial
        // proves the hardware was replaced and the management address now describes the
        // new box.
        sqlx::query!(
            r#"
            INSERT INTO resource_identifier
                (id, tenant_id, resource_id, kind, value, confidence, source)
            SELECT gen_random_uuid(), $1, $2, given.kind, given.value, given.confidence, $6
              FROM unnest($3::identifier_kind[], $4::text[], $5::real[])
                     AS given(kind, value, confidence)
            ON CONFLICT (tenant_id, kind, value)
            DO UPDATE SET resource_id = EXCLUDED.resource_id,
                          source      = EXCLUDED.source,
                          last_seen   = now()
            "#,
            tenant as TenantId,
            resource as ResourceId,
            &kinds as &[IdentifierKind],
            &values,
            &confidences,
            source,
        )
        .execute(self.pool())
        .await
        .map_err(|e| map("identifier", resource.to_string(), e))?;
        Ok(())
    }

    async fn record_decision(&self, decision: &Decision) -> Result<()> {
        insert_decision(self.pool(), decision).await
    }

    async fn merge(
        &self,
        tenant: TenantId,
        historical: ResourceId,
        surviving: ResourceId,
        decision: &Decision,
    ) -> Result<()> {
        // One transaction. A merge that moved the identifiers but failed before writing
        // the alias would orphan every piece of telemetry already recorded under the old
        // resource_id — the query layer expands through resource_alias and would find
        // nothing to expand.
        let mut tx = self
            .pool()
            .begin()
            .await
            .map_err(|e| map("resource", historical.to_string(), e))?;

        sqlx::query!(
            r#"
            UPDATE resource_identifier
               SET resource_id = $3, last_seen = now()
             WHERE tenant_id = $1 AND resource_id = $2
            "#,
            tenant as TenantId,
            historical as ResourceId,
            surviving as ResourceId,
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| map("identifier", historical.to_string(), e))?;

        insert_decision(&mut *tx, decision).await?;

        // The collapse-on-write trigger in 0004_identity.sql does the rest: anything
        // already pointing at `historical` follows it here, so no alias chain is ever
        // more than one hop and the query layer's expansion stays a single lookup.
        sqlx::query!(
            r#"
            INSERT INTO resource_alias (tenant_id, historical_id, current_id, decision_id)
            VALUES ($1, $2, $3, $4)
            ON CONFLICT (tenant_id, historical_id)
            DO UPDATE SET current_id = EXCLUDED.current_id, merged_at = now()
            "#,
            tenant as TenantId,
            historical as ResourceId,
            surviving as ResourceId,
            decision.id as DecisionId,
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| map("resource_alias", historical.to_string(), e))?;

        tx.commit()
            .await
            .map_err(|e| map("resource", historical.to_string(), e))
    }

    async fn split(
        &self,
        tenant: TenantId,
        from: ResourceId,
        identifiers: &[Identifier],
        decision: &Decision,
    ) -> Result<ResourceId> {
        if identifiers.is_empty() {
            return Err(Error::Invalid(
                "a split needs the identifiers to move".into(),
            ));
        }

        let new_id = ResourceId::new();
        let name = identifiers
            .first()
            .map_or_else(|| "split".to_owned(), |i| i.value.clone());
        let (kinds, values) = columns(identifiers);

        let mut tx = self
            .pool()
            .begin()
            .await
            .map_err(|e| map("resource", from.to_string(), e))?;

        sqlx::query!(
            r#"
            INSERT INTO resource (id, tenant_id, kind, name)
            VALUES ($1, $2, 'device', $3)
            "#,
            new_id as ResourceId,
            tenant as TenantId,
            name,
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| map("resource", new_id.to_string(), e))?;

        // Only identifiers that are actually on `from`. Splitting one that is not is a
        // caller error, and silently moving it would take an identifier off an unrelated
        // resource.
        let moved = sqlx::query!(
            r#"
            UPDATE resource_identifier ri
               SET resource_id = $3, last_seen = now()
              FROM unnest($4::identifier_kind[], $5::text[]) AS given(kind, value)
             WHERE ri.tenant_id = $1
               AND ri.resource_id = $2
               AND ri.kind = given.kind
               AND ri.value = given.value
            "#,
            tenant as TenantId,
            from as ResourceId,
            new_id as ResourceId,
            &kinds as &[IdentifierKind],
            &values,
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| map("identifier", from.to_string(), e))?
        .rows_affected();

        let asked = identifiers.len() as u64;
        if moved != asked {
            return Err(Error::Invalid(format!(
                "{} of {asked} identifiers do not belong to {from}",
                asked - moved
            )));
        }

        insert_decision(&mut *tx, decision).await?;

        // The merge that created the alias is undone, so the alias goes with it.
        // Otherwise telemetry from before the merge keeps resolving to the wrong side.
        sqlx::query!(
            r#"
            DELETE FROM resource_alias
             WHERE tenant_id = $1 AND current_id = $2
            "#,
            tenant as TenantId,
            from as ResourceId,
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| map("resource_alias", from.to_string(), e))?;

        tx.commit()
            .await
            .map_err(|e| map("resource", from.to_string(), e))?;

        Ok(new_id)
    }

    async fn pending_reviews(&self, tenant: TenantId, limit: i64) -> Result<Vec<ReviewItem>> {
        let rows = sqlx::query!(
            r#"
            SELECT d.id          AS "id: DecisionId",
                   d.resource_id AS "resource_id: ResourceId",
                   d.confidence,
                   d.matched_by  AS "matched_by: Json<Vec<Match>>",
                   d.observed    AS "observed: Json<Vec<Identifier>>",
                   d.source,
                   d.actor_id    AS "actor_id: ActorId",
                   d.decided_at
              FROM identity_decision d
             WHERE d.tenant_id = $1
               AND d.outcome = 'review'
               AND d.resource_id IS NOT NULL
               -- Answered one of two ways, and both are needed.
               --
               -- A manual decision that names the provisional: the operator merged
               -- something *into* it, or split something off it.
               AND NOT EXISTS (
                     SELECT 1 FROM identity_decision answered
                      WHERE answered.tenant_id = d.tenant_id
                        AND answered.resource_id = d.resource_id
                        AND answered.outcome IN ('manual_merge', 'manual_split')
                        AND answered.decided_at >= d.decided_at)
               -- Or the provisional was merged away, which is the common case and the
               -- one a decision row cannot express: merge() records the *surviving*
               -- resource, so nothing in identity_decision names the provisional at
               -- all. Without this clause an operator answers the question, and is
               -- asked it again tomorrow, forever.
               AND NOT EXISTS (
                     SELECT 1 FROM resource_alias a
                      WHERE a.tenant_id = d.tenant_id
                        AND a.historical_id = d.resource_id)
             ORDER BY d.decided_at DESC
             LIMIT $2
            "#,
            tenant as TenantId,
            limit,
        )
        .fetch_all(self.pool())
        .await
        .map_err(|e| map("identity_decision", String::new(), e))?;

        Ok(rows
            .into_iter()
            .filter_map(|r| {
                let provisional_id = r.resource_id?;
                Some(ReviewItem {
                    decision: Decision {
                        id: r.id,
                        tenant_id: tenant,
                        resource_id: Some(provisional_id),
                        outcome: DecisionOutcome::Review,
                        confidence: r.confidence,
                        matched_by: r.matched_by.0,
                        observed: r.observed.0,
                        source: r.source,
                        actor_id: r.actor_id,
                        decided_at: r.decided_at,
                    },
                    provisional_id,
                })
            })
            .collect())
    }

    async fn find_pending_review(
        &self,
        tenant: TenantId,
        observed: &[Identifier],
    ) -> Result<Option<ReviewItem>> {
        let as_json = Json(observed.to_vec());

        // Containment in both directions is set equality: the same question, however the
        // collector happened to order the identifiers. `@>` is also what a GIN index on
        // `observed` would accelerate if this ever becomes hot.
        let row = sqlx::query!(
            r#"
            SELECT d.id          AS "id: DecisionId",
                   d.resource_id AS "resource_id!: ResourceId",
                   d.confidence,
                   d.matched_by  AS "matched_by: Json<Vec<Match>>",
                   d.observed    AS "observed: Json<Vec<Identifier>>",
                   d.source,
                   d.actor_id    AS "actor_id: ActorId",
                   d.decided_at
              FROM identity_decision d
             WHERE d.tenant_id = $1
               AND d.outcome = 'review'
               AND d.resource_id IS NOT NULL
               AND d.observed @> $2::jsonb
               AND $2::jsonb @> d.observed
               AND NOT EXISTS (
                     SELECT 1 FROM identity_decision answered
                      WHERE answered.tenant_id = d.tenant_id
                        AND answered.resource_id = d.resource_id
                        AND answered.outcome IN ('manual_merge', 'manual_split')
                        AND answered.decided_at >= d.decided_at)
             ORDER BY d.decided_at DESC
             LIMIT 1
            "#,
            tenant as TenantId,
            as_json as Json<Vec<Identifier>>,
        )
        .fetch_optional(self.pool())
        .await
        .map_err(|e| map("identity_decision", String::new(), e))?;

        Ok(row.map(|r| ReviewItem {
            decision: Decision {
                id: r.id,
                tenant_id: tenant,
                resource_id: Some(r.resource_id),
                outcome: DecisionOutcome::Review,
                confidence: r.confidence,
                matched_by: r.matched_by.0,
                observed: r.observed.0,
                source: r.source,
                actor_id: r.actor_id,
                decided_at: r.decided_at,
            },
            provisional_id: r.resource_id,
        }))
    }
}

/// Written from both the pool and inside a transaction, so it takes an executor.
async fn insert_decision<'e, E>(executor: E, decision: &Decision) -> Result<()>
where
    E: sqlx::PgExecutor<'e>,
{
    let matched_by = Json(decision.matched_by.clone());
    let observed = Json(decision.observed.clone());

    sqlx::query!(
        r#"
        INSERT INTO identity_decision
            (id, tenant_id, resource_id, outcome, confidence,
             matched_by, observed, source, actor_id, decided_at)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
        "#,
        decision.id as DecisionId,
        decision.tenant_id as TenantId,
        decision.resource_id as Option<ResourceId>,
        decision.outcome.as_str(),
        decision.confidence,
        matched_by as Json<Vec<Match>>,
        observed as Json<Vec<Identifier>>,
        decision.source,
        decision.actor_id as Option<ActorId>,
        decision.decided_at,
    )
    .execute(executor)
    .await
    .map_err(|e| map("identity_decision", decision.id.to_string(), e))?;
    Ok(())
}
