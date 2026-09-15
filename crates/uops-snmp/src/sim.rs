//! A simulated agent, for testing the poller without a room full of hardware.
//!
//! Public rather than `#[cfg(test)]`, because the thing it is for lives in another
//! crate: SPEC §M2's *"1 000 simulated SNMP agents polled at 60s with p95 poll latency
//! < 5 s and no missed cycles"* is a test of the poller, not of this crate.
//!
//! It models the behaviours that are hard to obtain on demand from real equipment and
//! that a poller must survive:
//!
//! * a normal agent with a table;
//! * one that answers `tooBig` above some size — most real agents, at some size;
//! * one that answers `tooBig` to everything, which overloaded agents do;
//! * one that never answers, which is what a dead device looks like;
//! * one that returns the same OID forever, which is a real firmware bug and an
//!   infinite loop in any walk that does not check.
//!
//! Waiting for a switch to do the last three is not a test strategy.

use std::collections::BTreeMap;
use std::time::Duration;

use uops_profile::Oid;

use crate::bulk::Repetitions;
use crate::transport::{Target, Transport, TransportError, Value, VarBind};

/// How an agent misbehaves.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Behaviour {
    /// Answers correctly, up to any requested size.
    #[default]
    Normal,
    /// Answers `tooBig` above this many repetitions. What a real agent does when the
    /// response would exceed its buffer or the MTU.
    TooBigAbove(u32),
    /// Answers `tooBig` to everything, including one repetition. Overloaded agents do
    /// this, and it is the case that turns "halve and retry" into a loop.
    RefusesEverything,
    /// Never answers. A dead device, or one behind a dropped route.
    Silent,
    /// Returns the OID it was asked about, unchanged, forever. A firmware bug, and an
    /// infinite loop in any walk that trusts the agent to advance.
    NeverAdvances,
    /// Refuses the credentials.
    AuthFails,
}

/// One simulated agent.
#[derive(Clone, Debug)]
pub struct Agent {
    /// The MIB, ordered — which is what makes `GETBULK`'s "lexicographically next"
    /// answerable at all.
    mib: BTreeMap<Oid, Value>,
    pub behaviour: Behaviour,
    /// Added to every response, for exercising a poller's timeout budget.
    pub latency: Duration,
}

impl Agent {
    /// An agent holding nothing.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            mib: BTreeMap::new(),
            behaviour: Behaviour::Normal,
            latency: Duration::ZERO,
        }
    }

    /// An agent with an `ifTable`-shaped table: `columns` columns × `rows` rows.
    ///
    /// The values are the index, so a test can tell which row it got without a fixture
    /// file. Real column semantics do not matter to a walk — only the shape does.
    #[must_use]
    pub fn with_table(table: &Oid, columns: u32, rows: u32) -> Self {
        let mut mib = BTreeMap::new();
        for column in 1..=columns {
            for index in 1..=rows {
                let oid = table.child(column).child(index);
                mib.insert(oid, Value::Unsigned(u64::from(index)));
            }
        }
        Self {
            mib,
            behaviour: Behaviour::Normal,
            latency: Duration::ZERO,
        }
    }

    /// Put something at an exact OID.
    pub fn set(&mut self, oid: Oid, value: Value) {
        self.mib.insert(oid, value);
    }

    #[must_use]
    pub fn behaving(mut self, behaviour: Behaviour) -> Self {
        self.behaviour = behaviour;
        self
    }

    #[must_use]
    pub const fn slow(mut self, latency: Duration) -> Self {
        self.latency = latency;
        self
    }

    /// How many objects it holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.mib.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.mib.is_empty()
    }

    fn answer(&self, after: &Oid, max: Repetitions) -> Result<Vec<VarBind>, TransportError> {
        match self.behaviour {
            Behaviour::RefusesEverything => return Err(TransportError::TooBig),
            Behaviour::AuthFails => return Err(TransportError::AuthFailed),
            Behaviour::Silent => return Err(TransportError::Timeout),
            Behaviour::TooBigAbove(limit) if max.get() > limit => {
                return Err(TransportError::TooBig);
            }
            Behaviour::NeverAdvances => {
                return Ok(vec![VarBind {
                    oid: after.clone(),
                    value: Value::Unsigned(0),
                }]);
            }
            _ => {}
        }

        // Exclusive of `after`, as GETBULK is.
        let out: Vec<VarBind> = self
            .mib
            .range(after.clone()..)
            .filter(|(oid, _)| *oid != after)
            .take(max.get() as usize)
            .map(|(oid, value)| VarBind {
                oid: oid.clone(),
                value: value.clone(),
            })
            .collect();

        // A real agent signals the end of the MIB rather than returning nothing.
        if out.is_empty() {
            return Ok(vec![VarBind {
                oid: after.clone(),
                value: Value::EndOfMibView,
            }]);
        }
        Ok(out)
    }
}

/// A fleet, addressed the way the real transport is.
#[derive(Debug, Default)]
pub struct Fleet {
    agents: std::collections::HashMap<std::net::SocketAddr, Agent>,
}

impl Fleet {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, address: std::net::SocketAddr, agent: Agent) {
        self.agents.insert(address, agent);
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.agents.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.agents.is_empty()
    }
}

#[async_trait::async_trait]
impl Transport for Fleet {
    async fn get_bulk(
        &self,
        target: &Target,
        after: &Oid,
        max_repetitions: Repetitions,
    ) -> Result<Vec<VarBind>, TransportError> {
        let Some(agent) = self.agents.get(&target.address) else {
            // Nothing at that address. A real poller sees a timeout, not a refusal —
            // UDP to a host that is not listening usually produces silence.
            return Err(TransportError::Timeout);
        };

        if !agent.latency.is_zero() {
            tokio::time::sleep(agent.latency).await;
        }
        agent.answer(after, max_repetitions)
    }
}
