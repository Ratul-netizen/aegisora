//! `InProcessBus` runs the shipped conformance suite.
//!
//! SPEC M0 acceptance criteria: "`InProcessBus` passes the `TelemetryBus` conformance test
//! suite (written once, run against both impls)". This file is the whole of the first
//! half of that — eleven named tests, none of them written here.
//!
//! When `NatsBus` arrives, its crate gets a file exactly this size.

use async_trait::async_trait;
use uops_bus::conformance::BusFactory;
use uops_bus::{InProcessBus, conformance_suite};

struct InProcess;

#[async_trait]
impl BusFactory for InProcess {
    type Bus = InProcessBus;

    async fn create(&self, capacity: usize) -> Self::Bus {
        InProcessBus::new(capacity)
    }
}

conformance_suite!(InProcess);
