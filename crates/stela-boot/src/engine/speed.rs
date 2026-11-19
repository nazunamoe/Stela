//! The host's speed apart from its clock, once per core class before the
//! PEs start (`stela_boot_core::host_speed`): the boot processor measures
//! its class at the engine's start, and the first AP of each other class
//! once every AP started (`executor::start`), outside any AP's startup
//! second. Each calibration ends within `host_speed::BOUND_MS`. The `HOST
//! SPEED` lines follow `HOST PERFORMANCE`.
use alloc::{boxed::Box, string::String, vec::Vec};
use core::sync::atomic::{AtomicPtr, AtomicU32, AtomicU64, Ordering};
use stela_boot_core::host_performance::{Class, Features};
use stela_boot_core::host_speed::{Speed, buffer_bytes, calibrate, host_levels};

static SPEEDS: [AtomicPtr<Speed>; 4] = [const { AtomicPtr::new(core::ptr::null_mut()) }; 4];
/// The classes a processor took to measure, one bit each.
static CLAIMED: AtomicU32 = AtomicU32::new(0);
/// The boot processor's single-core clock (`GUEST TIMEOUTS core_hz`).
static SINGLE_CORE_HZ: AtomicU64 = AtomicU64::new(0);

/// Keep the boot-time single-core clock for `HOST CLOCK ALL BUSY`.
pub(super) fn set_single_core_hz(hz: u64) {
    SINGLE_CORE_HZ.store(hz, Ordering::Relaxed);
}

pub(super) fn single_core_hz() -> u64 {
    SINGLE_CORE_HZ.load(Ordering::Relaxed)
}

/// The calling processor's core class.
pub(crate) fn class() -> Class {
    Features::read().class
}

/// Take `class` to measure: false when a processor of it has.
pub(crate) fn claim(class: Class) -> bool {
    let bit = 1 << class.index();
    CLAIMED.fetch_or(bit, Ordering::AcqRel) & bit == 0
}

/// Measure the boot processor's class (`claim`, `measure`).
pub(crate) fn calibrate_once() {
    let class = class();
    if claim(class) {
        measure(class);
    }
}

/// Measure the calling processor, of `class`, within the calibration's
/// bound. The buffer is taken from the heap and given back; where the heap
/// cannot give 32 MiB the memory figures are left out.
pub(crate) fn measure(class: Class) {
    let (_, l3) = host_levels();
    let mut bytes = buffer_bytes(l3.map(|cache| cache.bytes));
    let mut buffer: Vec<u64> = Vec::new();
    while bytes >= 32 << 20 {
        if buffer.try_reserve_exact(bytes as usize / 8).is_ok() {
            break;
        }
        bytes /= 2;
    }
    let words = if buffer.capacity() == 0 {
        0
    } else {
        bytes as usize / 8
    };
    let speed = calibrate(
        &mut buffer,
        words,
        crate::firmware::tsc_hz(),
        crate::apic::rdtsc,
    );
    drop(buffer);
    SPEEDS[class.index()].store(Box::into_raw(Box::new(speed)), Ordering::Release);
}

/// One `HOST SPEED` line per class measured.
pub(super) fn lines() -> String {
    let mut lines = String::new();
    for class in Class::ALL {
        let speed = SPEEDS[class.index()].load(Ordering::Acquire);
        if !speed.is_null() {
            // SAFETY: published once from a leaked box, never freed.
            lines.push_str(&unsafe { &*speed }.line(class.name()));
        }
    }
    lines
}
