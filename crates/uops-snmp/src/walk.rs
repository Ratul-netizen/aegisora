//! Walking a table.
//!
//! `GETBULK` returns whatever follows the OID you asked about, lexicographically —
//! which, at the end of a table, is the *next table*. A walk is therefore a loop with a
//! stopping condition, and every one of the three ways it can go wrong is a real device
//! doing a real thing:
//!
//! | what the agent does | what a naive loop does |
//! |---|---|
//! | runs past the end of the table | keeps walking, into the rest of the MIB |
//! | returns the same OID forever | never terminates |
//! | says `tooBig` to everything | never terminates |
//!
//! The first is the common one and the most damaging: an `ifTable` walk that does not
//! stop at the table boundary keeps going into `ifXTable`, `ipAddrTable` and eventually
//! the whole agent, turning a three-request poll into a several-thousand-request one.
//! On a fleet, that is the difference between polling and a denial of service you
//! wrote yourself.
//!
//! Stopping is [`Oid::starts_with`], arc-wise — which is the second time that decision
//! pays for itself. A textual prefix would stop `1.3.6.1.2.1.2.2.1` correctly and let
//! `1.3.6.1.2.1.2.2.10` through as if it were part of the same table.

use uops_profile::Oid;

use crate::bulk::{Repetitions, Tuning};
use crate::transport::{Target, Transport, TransportError, Value, VarBind};

/// Why a walk stopped without finishing.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum WalkError {
    #[error(transparent)]
    Transport(#[from] TransportError),

    #[error(
        "the agent cannot answer even one repetition; it is refusing every request \
         rather than sending a large one"
    )]
    RefusesEverything,

    #[error(
        "the agent returned {oid} again instead of advancing; walking it would not \
         terminate"
    )]
    NotAdvancing { oid: String },

    #[error("the table did not end within {0} rows; refusing to keep walking")]
    TooManyRows(usize),
}

/// The most varbinds one walk will collect.
///
/// A 48-port switch's `ifTable` is a few hundred; a large chassis with sub-interfaces
/// can be tens of thousands. 100 000 is far above any legitimate table and far below
/// "this agent is broken and will stream until we run out of memory".
pub const MAX_ROWS: usize = 100_000;

/// Walk everything under `table`.
///
/// `tuning` is this device's remembered `max-repetitions`; it is updated in place, so
/// the caller should keep it and pass the same one next poll. See [`crate::bulk`].
///
/// # Errors
///
/// A transport failure, an agent that refuses every size, one that does not advance, or
/// a table that does not end.
pub async fn walk<T: Transport + ?Sized>(
    transport: &T,
    target: &Target,
    table: &Oid,
    tuning: &mut Tuning,
) -> Result<Vec<VarBind>, WalkError> {
    let mut collected: Vec<VarBind> = Vec::new();
    let mut cursor = table.clone();

    loop {
        let batch = request(transport, target, &cursor, tuning).await?;

        // An empty response with no error is an agent saying "nothing follows". Some
        // return EndOfMibView; some just stop. Both mean the same thing.
        if batch.is_empty() {
            return Ok(collected);
        }

        for vb in batch {
            // Past the end of the table. See the module docs: this is the stop that
            // matters, and it is arc-wise.
            if !vb.oid.starts_with(table) {
                return Ok(collected);
            }
            if vb.value == Value::EndOfMibView {
                return Ok(collected);
            }

            // The agent did not advance. Left alone this is an infinite loop against a
            // device that is, from every other angle, responding normally.
            if vb.oid <= cursor {
                return Err(WalkError::NotAdvancing {
                    oid: vb.oid.to_string(),
                });
            }

            cursor = vb.oid.clone();

            // NoSuchInstance is a hole in a row, not the end of the table — a column
            // the agent does not implement for this index. Skipped, but the cursor has
            // already moved past it.
            if vb.value != Value::NoSuchInstance {
                collected.push(vb);
            }

            if collected.len() > MAX_ROWS {
                return Err(WalkError::TooManyRows(MAX_ROWS));
            }
        }
    }
}

/// One `GETBULK`, halving on `tooBig` until the agent answers or refuses one.
async fn request<T: Transport + ?Sized>(
    transport: &T,
    target: &Target,
    after: &Oid,
    tuning: &mut Tuning,
) -> Result<Vec<VarBind>, WalkError> {
    let mut size = tuning.current();

    loop {
        match transport.get_bulk(target, after, size).await {
            Ok(batch) => {
                tuning.succeeded();
                return Ok(batch);
            }
            Err(TransportError::TooBig) => match tuning.too_big() {
                Some(smaller) => size = smaller,
                // Already at one repetition. Retrying cannot help, and looping here is
                // how a poller hangs on a single overloaded agent.
                None => return Err(WalkError::RefusesEverything),
            },
            Err(other) => return Err(other.into()),
        }
    }
}

/// Walk a table and keep only the rows under one column.
///
/// What a profile actually asks for: `ifName` is column 1 of `ifXEntry`, and a metric
/// wants that column across every index rather than the whole table.
#[must_use]
pub fn column(rows: &[VarBind], column: &Oid) -> Vec<VarBind> {
    rows.iter()
        .filter(|vb| vb.oid.starts_with(column))
        .cloned()
        .collect()
}

/// The instance suffix of `oid` beneath `table`, as arcs.
///
/// For `ifName.3` under `ifName` this is `[3]`. The index is what ties a value to the
/// interface it belongs to, and it is the only thing that does — SNMP tables have no
/// other join key.
#[must_use]
pub fn index_of(oid: &Oid, column: &Oid) -> Option<Vec<u32>> {
    if !oid.starts_with(column) {
        return None;
    }
    Some(oid.arcs()[column.len()..].to_vec())
}

/// A convenience for the common case: `max-repetitions` for a fresh device.
#[must_use]
pub const fn default_repetitions() -> Repetitions {
    crate::bulk::DEFAULT
}
