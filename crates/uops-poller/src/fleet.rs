//! Turning rows in `PostgreSQL` into something the scheduler can hold.
//!
//! # The crossing between tenants happens here, in a loop
//!
//! `TenantScope` has no "all tenants" constructor, deliberately — a query that spans
//! tenants should not be expressible. A single-process poller genuinely does serve every
//! tenant, so it crosses by iterating: one scope, one query and one set of profiles per
//! tenant, visible in [`load`] as a `for` loop. The alternative is one `WHERE` clause
//! somebody eventually copies into a place where it is wrong.
//!
//! # Why a device with a broken address is reported and skipped
//!
//! `mgmt_ip` is text — see `uops_store_pg::pollable` on why it is an identifier and not
//! a column — so it can be anything a user typed. A row that is not an address cannot be
//! polled by any amount of trying, and the useful thing to do with it is say which
//! resource it is so somebody can fix it, once per reload rather than once per tick.

use std::net::SocketAddr;

use uops_core::{ResourceId, TenantId, TenantScope};
use uops_poll::plan::Device;
use uops_profile::Profile;
use uops_store_pg::{PgStore, PollableDevice};

/// The port every SNMP agent listens on unless told otherwise.
///
/// `mgmt_ip` holds an address, not an endpoint: identity resolution matches on where a
/// device *is*, and a port is a property of a protocol rather than of the device. A
/// non-standard port is a per-device override this does not have yet — see STATUS.
pub const SNMP_PORT: u16 = 161;

/// A device that could not be scheduled, and why.
///
/// Carried out of [`load`] rather than logged inside it, so the loop decides how often
/// to say so and the loader stays testable without capturing output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skipped {
    pub tenant: TenantId,
    pub resource: ResourceId,
    pub reason: String,
}

/// What one reload produced.
#[derive(Debug, Default)]
pub struct Fleet {
    pub devices: Vec<(Device, Profile)>,
    pub skipped: Vec<Skipped>,
    /// Tenants that answered. Fewer than the total means one failed — the loop reports
    /// it and keeps the tenants that worked, because one tenant's broken row is not a
    /// reason to stop polling everybody else.
    pub tenants: usize,
}

/// Read every tenant's pollable devices and resolve each to a profile.
///
/// # Errors
///
/// Only when the tenant list itself cannot be read. A failure inside one tenant is
/// recorded in [`Fleet::skipped`] and the rest continue — see the struct's docs.
pub async fn load(store: &PgStore, limit: i64) -> uops_core::Result<Fleet> {
    let mut fleet = Fleet::default();

    for tenant in store.all_tenant_ids().await? {
        let scope = TenantScope::collector(tenant);

        let profiles = match store.profiles_for(tenant).await {
            Ok(p) => p,
            Err(e) => {
                fleet.skipped.push(Skipped {
                    tenant,
                    // Nil, because the failure is the tenant's and not any one device's.
                    resource: ResourceId::from_uuid(uuid::Uuid::nil()),
                    reason: format!("profiles could not be read: {e}"),
                });
                continue;
            }
        };
        let rows = match store.pollable_devices(&scope, limit).await {
            Ok(r) => r,
            Err(e) => {
                fleet.skipped.push(Skipped {
                    tenant,
                    resource: ResourceId::from_uuid(uuid::Uuid::nil()),
                    reason: format!("devices could not be read: {e}"),
                });
                continue;
            }
        };
        fleet.tenants += 1;

        for row in rows {
            match schedulable(&row, &profiles) {
                Ok(pair) => fleet.devices.push(pair),
                Err(reason) => fleet.skipped.push(Skipped {
                    tenant,
                    resource: row.resource_id,
                    reason,
                }),
            }
        }
    }

    Ok(fleet)
}

/// One row, as a device and the profile it will be polled under.
fn schedulable(row: &PollableDevice, profiles: &[Profile]) -> Result<(Device, Profile), String> {
    let address = endpoint(&row.address)?;

    let profile = uops_poll::poller::profile_for(
        profiles,
        row.profile_key.as_deref(),
        row.sysobjectid.as_deref(),
    )
    .ok_or_else(|| {
        // Only reachable when `generic-snmp` is missing, since it matches anything;
        // which means the profile table was never seeded or somebody disabled it.
        "no profile matched and there is no generic-snmp to fall back to".to_owned()
    })?;

    Ok((
        Device {
            tenant: row.tenant_id,
            resource: row.resource_id,
            // The nil uuid is what "no site" means on the telemetry side — see
            // uops_poll::sample::Subject, which owns that decision.
            site: row
                .site_id
                .unwrap_or_else(|| uops_core::SiteId::from_uuid(uuid::Uuid::nil())),
            address,
            credential: row.credential,
        },
        profile.clone(),
    ))
}

/// Parse a `mgmt_ip` value into somewhere to send a packet.
///
/// Accepts a bare address (the normal case, given the default port) and an explicit
/// `host:port`. IPv6 needs its brackets in the second form, which is the standard
/// spelling and what `SocketAddr` parses.
fn endpoint(address: &str) -> Result<SocketAddr, String> {
    let trimmed = address.trim();
    if let Ok(ip) = trimmed.parse::<std::net::IpAddr>() {
        return Ok(SocketAddr::new(ip, SNMP_PORT));
    }
    trimmed
        .parse::<SocketAddr>()
        .map_err(|_| format!("mgmt_ip {trimmed:?} is not an address"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_address_gets_the_snmp_port() {
        assert_eq!(
            endpoint("10.0.0.1").unwrap(),
            "10.0.0.1:161".parse::<SocketAddr>().unwrap()
        );
        // IPv6 without brackets is a bare address, not a host:port — the colons are the
        // address. Getting this the other way round would parse 2001:db8::1 as host
        // "2001:db8:" port ":1" and fail, or worse, succeed against something else.
        assert_eq!(
            endpoint("2001:db8::1").unwrap(),
            "[2001:db8::1]:161".parse::<SocketAddr>().unwrap()
        );
    }

    #[test]
    fn an_explicit_port_is_kept() {
        assert_eq!(
            endpoint("10.0.0.1:1161").unwrap(),
            "10.0.0.1:1161".parse::<SocketAddr>().unwrap()
        );
        assert_eq!(
            endpoint("[2001:db8::1]:1161").unwrap(),
            "[2001:db8::1]:1161".parse::<SocketAddr>().unwrap()
        );
    }

    #[test]
    fn whitespace_does_not_make_a_device_unpollable() {
        // A trailing space in a form field is not a reason to stop monitoring a switch.
        assert_eq!(
            endpoint("  10.0.0.1  ").unwrap(),
            "10.0.0.1:161".parse::<SocketAddr>().unwrap()
        );
    }

    #[test]
    fn something_that_is_not_an_address_says_so_with_the_value() {
        // The message is the whole point: it is what somebody reads to find the row.
        let err = endpoint("switch-1.example.com").unwrap_err();
        assert!(err.contains("switch-1.example.com"), "{err}");
        assert!(endpoint("").is_err());
    }

    #[test]
    fn a_pin_wins_and_an_unknown_sysobjectid_falls_back() {
        let profiles = uops_profile::builtin::all().expect("built-ins");
        let row = |key: Option<&str>, sysoid: Option<&str>| PollableDevice {
            tenant_id: TenantId::new(),
            resource_id: ResourceId::new(),
            site_id: None,
            address: "10.0.0.1".to_owned(),
            credential: None,
            profile_id: None,
            profile_key: key.map(ToOwned::to_owned),
            sysobjectid: sysoid.map(ToOwned::to_owned),
        };

        // A pin beats a sysObjectID that says something else.
        let (_, profile) = schedulable(
            &row(Some("linux-snmp"), Some("1.3.6.1.4.1.9.1.1")),
            &profiles,
        )
        .expect("pinned");
        assert_eq!(profile.id, "linux-snmp");

        // An OID no profile claims, and one that is not an OID at all, both land on the
        // generic profile rather than dropping the device.
        for sysoid in [Some("1.3.6.1.4.1.99999.1"), Some("not an oid"), None] {
            let (_, profile) = schedulable(&row(None, sysoid), &profiles).expect("fallback");
            assert_eq!(profile.id, "generic-snmp", "sysoid {sysoid:?}");
        }
    }

    #[test]
    fn a_device_with_no_site_still_gets_one() {
        // MetricRow.site_id is not nullable; the nil uuid is what "no site" means there.
        // A device that lost its rows because nobody had assigned it to a site would be
        // a silent hole in the telemetry.
        let profiles = uops_profile::builtin::all().expect("built-ins");
        let (device, _) = schedulable(
            &PollableDevice {
                tenant_id: TenantId::new(),
                resource_id: ResourceId::new(),
                site_id: None,
                address: "10.0.0.1".to_owned(),
                credential: None,
                profile_id: None,
                profile_key: None,
                sysobjectid: None,
            },
            &profiles,
        )
        .expect("schedulable");
        assert_eq!(device.site.into_uuid(), uuid::Uuid::nil());
    }
}
