//! The scheduling loop's own properties.
//!
//! Each part below it is tested on its own. What is only visible here is what happens
//! *between* them: that a reload does not disturb devices it did not change, that a
//! removed device stops being polled, and that a tick dispatches without waiting.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use uops_core::{CredentialRef, ResourceId, SiteId, TenantId};
use uops_poll::executor::{Executor, Limits};
use uops_poll::plan::Device;
use uops_poll::poller::{JobKey, Schedule, TickReport, run_tick, tasks};
use uops_profile::{Profile, builtin};

fn device(n: u16) -> Device {
    Device {
        tenant: TenantId::new(),
        resource: ResourceId::new(),
        site: SiteId::new(),
        address: format!("10.0.0.{}:161", n % 250 + 1).parse().unwrap(),
        credential: Some(CredentialRef::new()),
    }
}

fn profile(key: &str) -> Profile {
    builtin::all()
        .unwrap()
        .into_iter()
        .find(|p| p.id == key)
        .expect("built-in")
}

/// Advance `seconds` ticks, returning every key that came due.
fn run(schedule: &mut Schedule, seconds: u64) -> Vec<JobKey> {
    let mut all = Vec::new();
    let mut due = Vec::new();
    for _ in 0..seconds {
        schedule.due(&mut due);
        all.extend(due.iter().copied());
    }
    all
}

#[test]
fn loading_a_fleet_schedules_every_device() {
    let mut schedule = Schedule::new();
    let fleet: Vec<_> = (0..50u16)
        .map(|n| (device(n), profile("generic-snmp")))
        .collect();

    let (added, removed) = schedule.reload(&fleet);
    assert_eq!((added, removed), (50, 0));
    assert_eq!(schedule.devices(), 50);

    // generic-snmp plans five jobs per device: scalars, interface columns, discovery,
    // identity, availability.
    assert_eq!(schedule.live_jobs(), 250);
}

#[test]
fn a_reload_leaves_unchanged_devices_where_they_are() {
    // The property worth having. Rebuilding the wheel on every reload would re-jitter
    // the whole fleet each time a customer adds one switch — the same load spike a
    // restart causes, for a much sillier reason.
    //
    // Compared as two schedules rather than two windows of one. An earlier version ran
    // sixty ticks, added a device, ran sixty more and compared the counts; with jitter a
    // job whose interval lands at 54s fires twice in some minutes and once in others, so
    // that compared two different questions and failed on a schedule that was correct.
    let fleet: Vec<_> = (0..20u16)
        .map(|n| (device(n), profile("generic-snmp")))
        .collect();

    let ticks_of = |devices: &[(Device, Profile)]| -> Vec<(u64, JobKey)> {
        let mut schedule = Schedule::new();
        schedule.reload(devices);
        let mut fired = Vec::new();
        let mut due = Vec::new();
        for t in 1..=300u64 {
            schedule.due(&mut due);
            // Only the jobs belonging to the original twenty, which keep their keys
            // because keys are handed out in reload order and the new device is
            // appended. Twenty devices at five jobs each.
            fired.extend(due.iter().filter(|k| **k < 100).map(|k| (t, *k)));
        }
        fired
    };

    let alone = ticks_of(&fleet);

    let mut larger = fleet.clone();
    larger.push((device(99), profile("generic-snmp")));
    let alongside = ticks_of(&larger);

    assert!(!alone.is_empty(), "the fixture must fire something");
    assert_eq!(
        alone, alongside,
        "adding a device moved when the existing ones poll"
    );
}

#[test]
fn a_removed_device_stops_being_polled() {
    let mut schedule = Schedule::new();
    let fleet: Vec<_> = (0..5u16)
        .map(|n| (device(n), profile("generic-snmp")))
        .collect();
    schedule.reload(&fleet);
    assert_eq!(schedule.live_jobs(), 25);

    let kept: Vec<_> = fleet[..4].to_vec();
    let (added, removed) = schedule.reload(&kept);
    assert_eq!((added, removed), (0, 1));
    assert_eq!(
        schedule.live_jobs(),
        20,
        "the gone device's jobs are retired"
    );

    // And nothing it owned comes due again. The wheel has no removal — an entry
    // reschedules itself forever — so this is the assertion that the retirement set
    // actually works.
    let gone = fleet[4].0.resource;
    let fired = run(&mut schedule, 120);
    for key in fired {
        let job = schedule.job(key).expect("a due key must name a job");
        assert_ne!(job.device, gone, "a removed device was polled");
    }
}

#[test]
fn removing_and_re_adding_a_device_schedules_it_again() {
    // A device decommissioned and then brought back, or a flapping `mgmt_ip`. The
    // retirement set must not make it unschedulable forever.
    let mut schedule = Schedule::new();
    let one = (device(1), profile("generic-snmp"));

    schedule.reload(std::slice::from_ref(&one));
    schedule.reload(&[]);
    assert_eq!(schedule.devices(), 0);

    let (added, _) = schedule.reload(std::slice::from_ref(&one));
    assert_eq!(added, 1);

    let fired = run(&mut schedule, 120);
    assert!(
        fired.iter().any(|k| schedule.job(*k).is_some()),
        "a re-added device must be polled again"
    );
}

#[test]
fn due_keys_resolve_to_tasks_with_their_device() {
    let mut schedule = Schedule::new();
    let d = device(7);
    schedule.reload(&[(d.clone(), profile("generic-snmp"))]);

    let mut due = Vec::new();
    for _ in 0..120 {
        schedule.due(&mut due);
        if !due.is_empty() {
            break;
        }
    }
    assert!(
        !due.is_empty(),
        "something must come due within two minutes"
    );

    let resolved = tasks(&schedule, &due);
    assert_eq!(resolved.len(), due.len());
    assert!(resolved.iter().all(|t| t.device.resource == d.resource));
    assert!(resolved.iter().all(|t| t.subject().tenant == d.tenant));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tick_runs_its_tasks_and_counts_what_they_wrote() {
    let mut schedule = Schedule::new();
    let fleet: Vec<_> = (0..8u16)
        .map(|n| (device(n), profile("generic-snmp")))
        .collect();
    schedule.reload(&fleet);

    let executor = Executor::new(Limits::default());
    let calls = Arc::new(AtomicUsize::new(0));

    let mut due = Vec::new();
    let mut total = TickReport::default();
    for _ in 0..60 {
        schedule.due(&mut due);
        let batch = tasks(&schedule, &due);
        if batch.is_empty() {
            continue;
        }
        let calls = Arc::clone(&calls);
        let report = run_tick(&executor, batch, move |_task| {
            let calls = Arc::clone(&calls);
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok::<usize, ()>(3)
            }
        })
        .await;
        total.due += report.due;
        total.ok += report.ok;
        total.samples += report.samples;
    }

    assert!(total.due > 0, "nothing came due in a minute");
    assert_eq!(total.ok, total.due, "every task succeeded");
    assert_eq!(calls.load(Ordering::SeqCst), total.due);
    assert_eq!(
        total.samples,
        total.due * 3,
        "samples are counted, not assumed"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failing_task_is_counted_and_does_not_stop_the_others() {
    // One unreachable device in a tick must not prevent the rest of that tick from
    // running — the loop-level half of "a dead device does not delay healthy ones".
    let mut schedule = Schedule::new();
    let fleet: Vec<_> = (0..10u16)
        .map(|n| (device(n), profile("generic-snmp")))
        .collect();
    schedule.reload(&fleet);
    let doomed = fleet[0].0.resource;

    let executor = Executor::new(Limits::default());
    let mut due = Vec::new();
    let mut ok = 0;
    let mut failed = 0;

    for _ in 0..60 {
        schedule.due(&mut due);
        let batch = tasks(&schedule, &due);
        if batch.is_empty() {
            continue;
        }
        let report = run_tick(&executor, batch, move |task| async move {
            if task.device.resource == doomed {
                Err(())
            } else {
                Ok(1)
            }
        })
        .await;
        ok += report.ok;
        failed += report.failed;
    }

    assert!(failed > 0, "the doomed device must have failed");
    assert!(ok > 0, "the healthy devices must still have polled");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_hanging_task_is_cut_off_and_the_tick_still_returns() {
    // The budget, at the loop level. Without it a single hung device holds a slot and
    // eventually the whole fleet behind it.
    let mut schedule = Schedule::new();
    schedule.reload(&[(device(1), profile("generic-snmp"))]);

    let executor = Executor::new(Limits {
        global: 4,
        per_device: 1,
        device_budget: Duration::from_millis(100),
    });

    let mut due = Vec::new();
    for _ in 0..120 {
        schedule.due(&mut due);
        let batch = tasks(&schedule, &due);
        if batch.is_empty() {
            continue;
        }
        let started = std::time::Instant::now();
        let report = run_tick(&executor, batch, |_task| async {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            Ok::<usize, ()>(0)
        })
        .await;

        assert_eq!(report.budget_exhausted, report.due);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the tick waited for a hung device: {:?}",
            started.elapsed()
        );
        return;
    }
    panic!("nothing came due in two minutes");
}
