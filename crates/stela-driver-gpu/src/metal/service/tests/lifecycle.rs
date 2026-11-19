use super::{Memory, call, connect, cpu_buffer, service, status};
use crate::metal::{
    abi::StorageMode,
    device::{
        EventId,
        fixture::{MockBackend, device_on},
        lifecycle::QuiesceStatus,
    },
    error::Error,
    service::{
        Loss, REOPEN_FAILED, ServiceHost, SharedService,
        wire::{Empty, EventValueRequest, Handle, Operation, Status, Value},
    },
};
use alloc::boxed::Box;
use core::{
    num::NonZeroU64,
    sync::atomic::{AtomicUsize, Ordering},
};
use stela_contracts::gate::{GateService, MemoryRange};

#[test]
fn quiescing_closes_guest_admission_without_losing_a_healthy_device() {
    let mut service = service();
    let connection = connect(&mut service);
    let buffer = cpu_buffer(&mut service, connection, 8192, StorageMode::Shared);
    service.begin_quiesce();
    assert_eq!(service.connect().unwrap_err().status, Status::Invalid);
    assert_eq!(
        service
            .import(
                connection,
                &[MemoryRange {
                    address: 4096,
                    bytes: 4096
                }],
                NonZeroU64::new(2).unwrap()
            )
            .unwrap_err()
            .status,
        Status::Invalid
    );
    assert_eq!(service.device().backend().imports.len(), 1);
    assert_eq!(
        status(call::<_, Handle>(
            &mut service,
            connection,
            Operation::NewEvent,
            &Empty {},
            &mut Memory::default()
        )),
        Status::Closed
    );
    assert!(!service.device().is_lost());
    assert!(!service.quiesced());
    assert_eq!(service.quiesce_step(), QuiesceStatus::GpuIdle);
    assert!(service.quiesced());
    assert_eq!(service.device().current_allocated_size(), 8192);
    assert_eq!(service.retired(), None);
    // Even after idle, the gate still owns the connection and its resources.
    let allocated: Value = call(
        &mut service,
        connection,
        Operation::AllocatedSize,
        &Empty {},
        &mut Memory::default(),
    )
    .unwrap();
    assert_eq!(allocated.value, 8192);
    call::<_, Empty>(
        &mut service,
        connection,
        Operation::Release,
        &Handle {
            handle: buffer.handle,
        },
        &mut Memory::default(),
    )
    .unwrap();
    assert_eq!(service.retired(), Some(NonZeroU64::MIN));
    assert_eq!(service.retired(), None);
    service.disconnect(connection).unwrap();
}

#[test]
fn existing_event_signals_remain_available_while_work_drains() {
    let mut service = service();
    let connection = connect(&mut service);
    let event: Handle = call(
        &mut service,
        connection,
        Operation::NewEvent,
        &Empty {},
        &mut Memory::default(),
    )
    .unwrap();
    let id = service.object::<EventId>(connection, event.handle).unwrap();
    let semaphore = service.device().event_semaphore(id).unwrap();
    let mut commands = service.device().new_command_buffer();
    commands
        .encode_wait_for_event(&mut service.device().resolver(), semaphore, 5)
        .unwrap();
    let serial = service.device().commit(commands).unwrap();
    assert_eq!(
        service.quiesce_step(),
        QuiesceStatus::Pending {
            completed: 0,
            submitted: serial
        }
    );
    call::<_, Empty>(
        &mut service,
        connection,
        Operation::SetEventSignaledValue,
        &EventValueRequest {
            event: event.handle,
            value: 5,
        },
        &mut Memory::default(),
    )
    .unwrap();
    assert_eq!(service.device().backend().semaphores[&semaphore.0], 5);
    assert_eq!(service.device().backend().idle_calls, 0);
    let backend = service.device().backend();
    let timeline = *backend.semaphores.keys().next().unwrap();
    backend.semaphores.insert(timeline, serial);
    assert_eq!(service.quiesce_step(), QuiesceStatus::GpuIdle);
    assert!(!service.device().is_lost());
}

#[test]
fn shared_quiesce_uses_the_serialized_service_stack() {
    let shared = SharedService::new(
        service(),
        ServiceHost {
            relax: || panic!("nothing waits"),
            relax_within: || panic!("nothing waits"),
            stack: 0x4000,
            run: |stack, call| {
                assert_eq!(stack, 0x4000);
                call();
            },
            report: |_| {},
            note: |_| {},
            ticks: || 0,
            served: |_| {},
            wake: || {},
            bundle: |_| {},
            job_waiting: || {},
        },
    );
    let connection = shared.connect().unwrap();
    shared.begin_quiesce();
    assert!(shared.connect().is_err());
    assert_eq!(shared.quiesce_step(), QuiesceStatus::GpuIdle);
    shared.disconnect(connection);
}

#[test]
fn failed_idle_does_not_close_the_connection_or_release_its_imports() {
    let mut service = service();
    let connection = connect(&mut service);
    cpu_buffer(&mut service, connection, 8192, StorageMode::Shared);
    service.device().backend().idle_error = Some(Error::DeviceLost);
    assert_eq!(
        service.quiesce_step(),
        QuiesceStatus::Unproven(Error::DeviceLost)
    );
    assert!(service.connections.contains_key(&connection));
    service.disconnect(connection).unwrap();
    assert_eq!(service.retired(), None);
    assert_eq!(service.device().backend().imports.len(), 1);
    assert_eq!(service.device().current_allocated_size(), 8192);
}

#[test]
fn a_lost_device_reopens_once_its_last_connection_closes() {
    static REOPENED: AtomicUsize = AtomicUsize::new(0);
    static PREPARED: AtomicUsize = AtomicUsize::new(0);
    let mut service = service().with_reopen(Box::new(|| {
        REOPENED.fetch_add(1, Ordering::Relaxed);
        Ok(device_on(MockBackend::new()))
    }));
    // The platform's setup of the first device runs on the reopened one.
    service.prepare_reopened(Box::new(|device| {
        assert!(!device.is_lost());
        PREPARED.fetch_add(1, Ordering::Relaxed);
    }));
    let first = connect(&mut service);
    let second = connect(&mut service);
    cpu_buffer(&mut service, first, 4096, StorageMode::Shared);
    service.device().mark_lost();
    assert_eq!(
        service.connect().unwrap_err().status,
        Status::DeviceLost,
        "new clients wait while a connection holds an object of the lost device"
    );
    service.disconnect(second).unwrap();
    assert_eq!(REOPENED.load(Ordering::Relaxed), 0);
    service.disconnect(first).unwrap();
    assert_eq!(REOPENED.load(Ordering::Relaxed), 1);
    assert_eq!(PREPARED.load(Ordering::Relaxed), 1);
    assert!(!service.device().is_lost());
    let next = connect(&mut service);
    cpu_buffer(&mut service, next, 4096, StorageMode::Shared);
    assert_eq!(service.device().current_allocated_size(), 4096);
}

/// While the platform says a reopen must wait, as while a kernel driver
/// still resets the GPU, no new device is opened and the lost one stays;
/// each connection or close that asked says why, and none counts as a
/// reopen that failed. The first ask after the wait reopens.
#[test]
fn a_lost_device_waits_to_reopen_while_the_platform_holds_it() {
    static REOPENED: AtomicUsize = AtomicUsize::new(0);
    static WAITS: AtomicUsize = AtomicUsize::new(2);
    let mut service = service()
        .with_reopen(Box::new(|| {
            REOPENED.fetch_add(1, Ordering::Relaxed);
            Ok(device_on(MockBackend::new()))
        }))
        .with_reopen_wait(Box::new(|| {
            (WAITS.load(Ordering::Relaxed) > 0).then(|| {
                WAITS.fetch_sub(1, Ordering::Relaxed);
                "its kernel driver is still resetting the GPU"
            })
        }));
    let held = connect(&mut service);
    service.device().mark_lost();
    service.disconnect(held).unwrap();
    assert_eq!(REOPENED.load(Ordering::Relaxed), 0);
    assert_eq!(
        service.connect().unwrap_err().status,
        Status::DeviceLost,
        "a client waits with the lost device while the reopen waits"
    );
    assert_eq!(REOPENED.load(Ordering::Relaxed), 0);
    assert!(service.device().is_lost());
    let refused = core::mem::take(&mut service.refused);
    assert_eq!(refused.len(), 2);
    assert!(refused.iter().all(|reason| reason
        == "the lost device waits to reopen: its kernel driver is still resetting the GPU"));
    // The wait ended: the next client gets a new device.
    let next = connect(&mut service);
    assert_eq!(REOPENED.load(Ordering::Relaxed), 1);
    assert!(!service.device().is_lost());
    cpu_buffer(&mut service, next, 4096, StorageMode::Shared);
}

/// A reopen that waited and then failed spans from the first wait: the
/// platform ends a session no GPU serves no later for the wait, and a wait
/// alone is no failure.
#[test]
fn failures_after_a_wait_span_from_the_wait() {
    fn clock() -> u64 {
        static NOW: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
        NOW.fetch_add(1000, Ordering::Relaxed)
    }
    static WAITS: AtomicUsize = AtomicUsize::new(2);
    let mut service = service()
        .with_reopen(Box::new(|| Err(Error::Unsupported("still resetting"))))
        .with_reopen_wait(Box::new(|| {
            (WAITS.load(Ordering::Relaxed) > 0).then(|| {
                WAITS.fetch_sub(1, Ordering::Relaxed);
                "its kernel driver is still resetting the GPU"
            })
        }));
    service.device().set_clock(clock);
    let held = connect(&mut service);
    service.device().mark_lost();
    // Two waits: no failure yet.
    service.disconnect(held).unwrap();
    assert!(service.connect().is_err());
    assert_eq!(service.loss().failed_reopens, 0);
    // The first failure already spans the wait: the clock was read at
    // each wait and at the failure.
    assert!(service.connect().is_err());
    let first = service.loss();
    assert_eq!(first.failed_reopens, 1);
    assert!(first.failing_ticks >= 2000, "{first:?}");
    assert!(service.connect().is_err());
    let second = service.loss();
    assert_eq!(second.failed_reopens, 2);
    assert!(second.failing_ticks > first.failing_ticks, "{second:?}");
}

/// Why a lost device did not come back is reported by the connections
/// that tried to reopen it: a refused connection serves no call, so the
/// reason waited, unreported, for a call that never came.
#[test]
fn connections_report_why_the_lost_device_did_not_reopen() {
    static REPORTED: AtomicUsize = AtomicUsize::new(0);
    let service = service().with_reopen(Box::new(|| Err(Error::DeviceLost)));
    let shared = SharedService::new(
        service,
        ServiceHost {
            relax: || panic!("nothing waits"),
            relax_within: || panic!("nothing waits"),
            stack: 0x4000,
            run: |_, call| call(),
            report: |reason| {
                if reason.contains("did not reopen") {
                    REPORTED.fetch_add(1, Ordering::Relaxed);
                }
            },
            note: |_| {},
            ticks: || 0,
            served: |_| {},
            wake: || {},
            bundle: |_| {},
            job_waiting: || {},
        },
    );
    let held = shared.connect().unwrap();
    shared.try_turn().unwrap().device().mark_lost();
    // The close of the last connection tries the reopen.
    shared.disconnect(held);
    assert_eq!(REPORTED.load(Ordering::Relaxed), 1);
    // Each later client tries again and is refused; it says why too.
    assert!(shared.connect().is_err());
    assert_eq!(REPORTED.load(Ordering::Relaxed), 2);
    assert!(shared.try_turn().unwrap().refused.is_empty());
}

/// A process that keeps an empty connection open no longer keeps every
/// later client from the GPU: the device reopens for new clients, and the
/// old connection stays lost.
#[test]
fn a_lost_device_reopens_while_an_empty_connection_stays_open() {
    let mut service = service().with_reopen(Box::new(|| Ok(device_on(MockBackend::new()))));
    let idle = connect(&mut service);
    let busy = connect(&mut service);
    cpu_buffer(&mut service, busy, 4096, StorageMode::Shared);
    service.device().mark_lost();
    service.disconnect(busy).unwrap();
    assert!(!service.device().is_lost());
    let next = connect(&mut service);
    cpu_buffer(&mut service, next, 4096, StorageMode::Shared);
    assert_eq!(service.device().current_allocated_size(), 4096);
    assert_eq!(
        status(call::<_, Handle>(
            &mut service,
            idle,
            Operation::NewEvent,
            &Empty {},
            &mut Memory::default()
        )),
        Status::DeviceLost
    );
    service.disconnect(idle).unwrap();
    assert!(!service.device().is_lost());
}

/// The lost device's backend, which the reopened device replaces: retired
/// by default, kept unreleased for a driver whose release may wait without
/// bound ([`Service::keep_lost_devices`], RADV's). Answers the devices still
/// retiring and the references to the lost backend's probe.
fn lost_backend_after_reopen(keep: bool) -> (usize, usize) {
    let probe = alloc::sync::Arc::new(core::sync::atomic::AtomicBool::new(false));
    let mut service = service().with_reopen(Box::new(|| Ok(device_on(MockBackend::new()))));
    if keep {
        service = service.keep_lost_devices();
    }
    service.device().backend().pipeline_gate = Some(probe.clone());
    let connection = connect(&mut service);
    service.device().mark_lost();
    service.disconnect(connection).unwrap();
    assert!(!service.device().is_lost());
    (
        service.retiring_devices(),
        alloc::sync::Arc::strong_count(&probe),
    )
}

#[test]
fn a_reopened_device_retires_the_lost_one() {
    assert_eq!(lost_backend_after_reopen(false).0, 1);
}

#[test]
fn a_reopened_device_keeps_the_lost_one_when_asked() {
    assert_eq!(lost_backend_after_reopen(true), (0, 2));
}

/// A reopened device of a driver that keeps its lost devices serves only
/// once an empty submission on it signaled ([`Service::check_reopened`]):
/// until then each client that asks waits, and none counts as a failure.
#[test]
fn a_reopened_device_serves_once_its_first_submission_signals() {
    fn clock() -> u64 {
        static NOW: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
        NOW.fetch_add(1000, Ordering::Relaxed)
    }
    let mut service = service()
        .with_reopen(Box::new(|| Ok(device_on(MockBackend::new()))))
        .keep_lost_devices()
        .check_reopened(1_000_000);
    service.device().set_clock(clock);
    let held = connect(&mut service);
    service.device().mark_lost();
    service.disconnect(held).unwrap();
    assert!(
        service.device().is_lost(),
        "its first submission has not signaled"
    );
    assert_eq!(service.connect().unwrap_err().status, Status::DeviceLost);
    assert_eq!(service.loss().failed_reopens, 0);
    let refused = core::mem::take(&mut service.refused);
    assert_eq!(refused.len(), 2);
    assert!(refused.iter().all(|reason| reason
        == "the lost device waits to reopen: the reopened device's first submission has not \
            signaled yet"));
    let probing = service.probing.as_mut().unwrap();
    let semaphore = probing.semaphore;
    assert_eq!(probing.device.backend().submissions.len(), 1);
    crate::metal::vulkan::backend::Backend::signal_semaphore(
        probing.device.backend(),
        semaphore,
        1,
    )
    .unwrap();
    let next = connect(&mut service);
    assert!(!service.device().is_lost());
    assert!(service.probing.is_none());
    cpu_buffer(&mut service, next, 4096, StorageMode::Shared);
}

/// A driver that checks a reopened device's first submission but releases
/// lost devices (ANV's): the lost device retires once the reopened one
/// serves, and none is kept.
#[test]
fn a_checked_reopen_retires_the_lost_device_once_it_serves() {
    let mut service = service()
        .with_reopen(Box::new(|| Ok(device_on(MockBackend::new()))))
        .check_reopened(u64::MAX);
    let held = connect(&mut service);
    service.device().mark_lost();
    service.disconnect(held).unwrap();
    assert!(
        service.device().is_lost(),
        "its first submission has not signaled"
    );
    assert_eq!(service.retiring_devices(), 0);
    let probing = service.probing.as_mut().unwrap();
    let semaphore = probing.semaphore;
    crate::metal::vulkan::backend::Backend::signal_semaphore(
        probing.device.backend(),
        semaphore,
        1,
    )
    .unwrap();
    let next = connect(&mut service);
    assert!(!service.device().is_lost());
    assert_eq!(service.retiring_devices(), 1, "the lost device retires");
    service.disconnect(next).unwrap();
}

/// A reopened device whose first submission does not signal in its bound,
/// as one whose GPU hangs again, fails the reopen; the next client tries a
/// new one.
#[test]
fn a_reopened_device_whose_first_submission_hangs_fails_the_reopen() {
    fn clock() -> u64 {
        static NOW: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
        NOW.fetch_add(1000, Ordering::Relaxed)
    }
    static REOPENED: AtomicUsize = AtomicUsize::new(0);
    let mut service = service()
        .with_reopen(Box::new(|| {
            REOPENED.fetch_add(1, Ordering::Relaxed);
            Ok(device_on(MockBackend::new()))
        }))
        .keep_lost_devices()
        .check_reopened(1500);
    service.device().set_clock(clock);
    let held = connect(&mut service);
    service.device().mark_lost();
    // Submitted and looked at within the bound: it waits.
    service.disconnect(held).unwrap();
    assert_eq!(service.loss().failed_reopens, 0);
    // Past it: the reopen failed.
    assert!(service.connect().is_err());
    let loss = service.loss();
    assert_eq!(loss.failed_reopens, 1);
    assert!(service.device().is_lost());
    assert!(service.probing.is_none());
    assert!(service.refused.last().unwrap().starts_with(REOPEN_FAILED));
    assert!(
        loss.error.unwrap().contains("did not signal in time"),
        "the failure names the hang"
    );
    assert_eq!(REOPENED.load(Ordering::Relaxed), 1);
    // The next client reopens again.
    assert!(service.connect().is_err());
    assert_eq!(REOPENED.load(Ordering::Relaxed), 2);
}

/// A reopened device whose first submission the kernel driver refused
/// fails the reopen with that error.
#[test]
fn a_reopened_device_whose_first_submission_is_refused_fails_the_reopen() {
    let mut service = service()
        .with_reopen(Box::new(|| {
            let mut backend = MockBackend::new();
            backend.refuse_next = Some(Error::DeviceLost);
            Ok(device_on(backend))
        }))
        .keep_lost_devices()
        .check_reopened(u64::MAX);
    let held = connect(&mut service);
    service.device().mark_lost();
    service.disconnect(held).unwrap();
    assert_eq!(service.loss().failed_reopens, 0);
    let probing = service.probing.as_mut().unwrap();
    let semaphore = probing.semaphore;
    crate::metal::vulkan::backend::Backend::signal_semaphore(
        probing.device.backend(),
        semaphore,
        1,
    )
    .unwrap();
    assert!(service.connect().is_err());
    let loss = service.loss();
    assert_eq!(loss.failed_reopens, 1);
    assert_eq!(loss.error.as_deref(), Some("DeviceLost"));
    assert!(service.probing.is_none());
}

#[test]
fn a_device_that_does_not_reopen_stays_lost() {
    let mut service = service().with_reopen(Box::new(|| Err(Error::OutOfMemory)));
    let connection = connect(&mut service);
    service.device().mark_lost();
    service.disconnect(connection).unwrap();
    assert!(service.device().is_lost());
    assert_eq!(service.connect().unwrap_err().status, Status::DeviceLost);
    assert!(service.refused.pop().unwrap().contains("did not reopen"));
}

/// The platform ends a session no GPU serves by what the service counts:
/// each reopen that failed since the loss, over how long by the platform's
/// clock, with the last error, and none once a reopen gave a device.
#[test]
fn failed_reopens_count_in_a_row_until_one_gives_a_device() {
    fn clock() -> u64 {
        static NOW: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
        NOW.fetch_add(1000, Ordering::Relaxed)
    }
    let tries = alloc::sync::Arc::new(AtomicUsize::new(0));
    let reopen = {
        let tries = tries.clone();
        Box::new(move || match tries.fetch_add(1, Ordering::Relaxed) {
            0 | 1 => Err(Error::DeviceLost),
            _ => Ok(device_on(MockBackend::new())),
        })
    };
    let mut service = service().with_reopen(reopen);
    service.device().set_clock(clock);
    assert_eq!(service.loss(), Loss::default());
    let connection = connect(&mut service);
    service.device().mark_lost();
    // Lost, with no reopen tried while a connection holds the device.
    assert_eq!(
        service.loss(),
        Loss {
            lost: true,
            ..Loss::default()
        }
    );
    service.disconnect(connection).unwrap();
    // One failed reopen spans no time yet.
    assert_eq!(
        service.loss(),
        Loss {
            lost: true,
            failed_reopens: 1,
            failing_ticks: 0,
            error: Some("DeviceLost".into())
        }
    );
    assert!(service.connect().is_err());
    let loss = service.loss();
    assert_eq!((loss.lost, loss.failed_reopens), (true, 2));
    assert_eq!(loss.error.as_deref(), Some("DeviceLost"));
    assert!(loss.failing_ticks >= 1000, "{loss:?}");
    assert!(
        service
            .refused
            .iter()
            .all(|reason| reason.starts_with(REOPEN_FAILED))
    );
    connect(&mut service);
    assert_eq!(service.loss(), Loss::default());
}

/// A replaced lost device is dropped only once its backend's idle steps
/// end, one step per call that may recover, so no call holds its
/// processor for the whole idle bound.
#[test]
fn a_replaced_device_retires_one_idle_step_per_call() {
    use crate::metal::vulkan::backend::IdleStep;
    let mut service = service().with_reopen(Box::new(|| Ok(device_on(MockBackend::new()))));
    service.device().backend().idle_steps.extend([
        IdleStep::Waiting,
        IdleStep::Waiting,
        IdleStep::Idle,
    ]);
    let first = connect(&mut service);
    service.device().mark_lost();
    service.disconnect(first).unwrap();
    assert!(!service.device().is_lost());
    assert_eq!(service.retiring_devices(), 1);
    // Each later call steps once: waiting, waiting, then idle and dropped.
    let next = connect(&mut service);
    assert_eq!(service.retiring_devices(), 1);
    service.disconnect(next).unwrap();
    assert_eq!(service.retiring_devices(), 1);
    connect(&mut service);
    assert_eq!(service.retiring_devices(), 0);
}

/// An unproven idle step drops the lost device at once: its backend keeps
/// what work may still use without waiting again.
#[test]
fn an_unproven_replaced_device_is_dropped_at_its_step() {
    use crate::metal::vulkan::backend::IdleStep;
    let mut service = service().with_reopen(Box::new(|| Ok(device_on(MockBackend::new()))));
    service
        .device()
        .backend()
        .idle_steps
        .push_back(IdleStep::Unproven);
    let first = connect(&mut service);
    service.device().mark_lost();
    service.disconnect(first).unwrap();
    assert_eq!(service.retiring_devices(), 1);
    connect(&mut service);
    assert_eq!(service.retiring_devices(), 0);
}
