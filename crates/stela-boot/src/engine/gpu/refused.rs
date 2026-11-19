//! The first DRM calls Linux refuses a Mesa driver, one line each: the ioctl
//! by name with what it asked for, or the mapping, and the errno. A Vulkan
//! driver turns most such refusals into one generic result
//! (`VK_ERROR_OUT_OF_DEVICE_MEMORY`), so these lines say which call it was.
//! Interrupted calls, which the C library repeats, are not refusals, nor is
//! a driver's ordinary answer to a probe of what its GPU has. A driver's own
//! command is named by the driver of the file, which the file is asked: one
//! image holds several drivers that number their commands alike.
use crate::output;
use core::{
    ffi::{c_int, c_void},
    ptr,
    sync::atomic::{AtomicU32, Ordering},
};
use stela_boot_core::drm_calls;

/// How many refusals are shown; one more line says that more were not.
const SHOWN: u32 = 96;
static REFUSED: AtomicU32 = AtomicU32::new(0);

/// DRM's ioctl type, `'d'`.
const DRM: u32 = b'd' as u32;
/// What [`Input`] keeps of an argument: every amdgpu input it decodes.
const KEPT: usize = 64;

const EINTR: i64 = -4;
const EAGAIN: i64 = -11;
/// Linux's `ERESTARTSYS`, which a client may see from an interrupted wait.
const ERESTARTSYS: i64 = -512;
/// Linux's `ETIME`: a bounded wait that ran out, as the completion
/// waiter's idle waits all do; never a refusal.
const ETIME: i64 = -62;

/// An ioctl argument's first bytes as the caller passed them: the kernel
/// writes its answer over the same structure. Only amdgpu's are decoded.
pub(super) struct Input {
    bytes: [u8; KEPT],
}

#[cfg_attr(not(feature = "amdgpu"), allow(dead_code))]
impl Input {
    fn u32(&self, at: usize) -> u32 {
        u32::from_le_bytes(self.bytes[at..at + 4].try_into().expect("four bytes"))
    }

    fn u64(&self, at: usize) -> u64 {
        u64::from_le_bytes(self.bytes[at..at + 8].try_into().expect("eight bytes"))
    }
}

/// Whether refusals are still shown, so a call's input is worth keeping.
fn showing() -> bool {
    REFUSED.load(Ordering::Relaxed) <= SHOWN
}

/// `argument`'s input before ioctl `request` runs, while refusals are shown.
///
/// # Safety
/// `argument` is what `request` says it is: its size in bytes, as the kernel
/// reads or writes it.
pub(super) unsafe fn before(request: u32, argument: *const c_void) -> Option<Input> {
    if !showing() || argument.is_null() {
        return None;
    }
    let size = ((request >> 16) & 0x3fff) as usize;
    let mut bytes = [0; KEPT];
    // SAFETY: the caller's: the argument holds the request's size.
    unsafe {
        ptr::copy_nonoverlapping(argument.cast::<u8>(), bytes.as_mut_ptr(), size.min(KEPT));
    }
    Some(Input { bytes })
}

/// Takes a refusal's turn: whether to show it, after saying once that the
/// rest are not.
fn turn() -> bool {
    let index = REFUSED.fetch_add(1, Ordering::Relaxed);
    if index == SHOWN {
        output::arguments(format_args!(
            "GPU DRM refused more calls; the first {SHOWN} are above\n"
        ));
    }
    index < SHOWN
}

/// Reports ioctl `request` on `fd` if Linux refused it with `result`.
pub(super) fn control(fd: c_int, request: u32, input: Option<&Input>, result: i64) {
    if result >= 0 || matches!(result, EINTR | EAGAIN | ERESTARTSYS) {
        return;
    }
    if result == ETIME && request & 0xff == 0xca && (request >> 8) & 0xff == DRM {
        return;
    }
    // A driver's normal answer to a probe of what the GPU has.
    let query = input.map(|input| input.u32(12));
    if stela_boot_core::gpu_requests::probe_answer(request, query, result) {
        return;
    }
    let command = drm_calls::driver_command(request);
    let mut buffer = [0; 16];
    // Asked only while refusals are shown, when the input was kept.
    let driver = match (command, input) {
        (Some(_), Some(_)) => driver_of(fd, &mut buffer),
        _ => None,
    };
    if let (Some(driver), Some(input)) = (driver, input)
        && drm_calls::probe_answer(driver, request, &input.bytes, result)
    {
        return;
    }
    if !turn() {
        return;
    }
    let drm = (request >> 8) & 0xff == DRM;
    let name = match (command, driver) {
        (Some(command), Some(driver)) => drm_calls::driver_call(driver, command),
        (Some(_), None) => None,
        (None, _) if drm => name(request & 0xff),
        (None, _) => None,
    };
    let mut detail = alloc::string::String::new();
    if let (Some(command), Some(input)) = (command, input)
        && driver == Some("amdgpu")
    {
        describe(command, input, &mut detail);
    }
    match name {
        Some(name) => output::arguments(format_args!(
            "GPU DRM ioctl {name} ({request:#010x}) fd={fd}{detail} refused: {}\n",
            Errno(result)
        )),
        None => output::arguments(format_args!(
            "GPU DRM ioctl {request:#010x} fd={fd} refused: {}\n",
            Errno(result)
        )),
    }
}

/// Whether a call's `result` is an error: `-4095..=-1`, as Linux's
/// `IS_ERR_VALUE`. A mapping's address in the upper half of the address
/// space reads as a negative number and is none.
pub(super) fn is_error(result: i64) -> bool {
    (-4095..=-1).contains(&result)
}

/// Reports a mapping of `size` bytes of `fd` at `offset` Linux refused with
/// `result`; a mapped address is not reported.
pub(super) fn map(fd: c_int, offset: u64, size: usize, result: i64) {
    if is_error(result) && turn() {
        output::arguments(format_args!(
            "GPU DRM mmap fd={fd} offset={offset:#x} bytes={size:#x} refused: {}\n",
            Errno(result)
        ));
    }
}

/// A negative errno, with its name when it is a common one.
struct Errno(i64);

impl core::fmt::Display for Errno {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let name = match -self.0 {
            1 => "EPERM",
            2 => "ENOENT",
            5 => "EIO",
            9 => "EBADF",
            12 => "ENOMEM",
            13 => "EACCES",
            14 => "EFAULT",
            16 => "EBUSY",
            19 => "ENODEV",
            22 => "EINVAL",
            28 => "ENOSPC",
            34 => "ERANGE",
            38 => "ENOSYS",
            62 => "ETIME",
            95 => "EOPNOTSUPP",
            110 => "ETIMEDOUT",
            125 => "ECANCELED",
            _ => "",
        };
        write!(formatter, "{}", self.0)?;
        if !name.is_empty() {
            write!(formatter, " ({name})")?;
        }
        Ok(())
    }
}

/// DRM's own ioctls by number; a driver's are named by its driver.
fn name(number: u32) -> Option<&'static str> {
    Some(match number {
        0x00 => "VERSION",
        0x09 => "GEM_CLOSE",
        0x0a => "GEM_FLINK",
        0x0b => "GEM_OPEN",
        0x0c => "GET_CAP",
        0x0d => "SET_CLIENT_CAP",
        0x2d => "PRIME_HANDLE_TO_FD",
        0x2e => "PRIME_FD_TO_HANDLE",
        0xbf => "SYNCOBJ_CREATE",
        0xc0 => "SYNCOBJ_DESTROY",
        0xc1 => "SYNCOBJ_HANDLE_TO_FD",
        0xc2 => "SYNCOBJ_FD_TO_HANDLE",
        0xc3 => "SYNCOBJ_WAIT",
        0xc4 => "SYNCOBJ_RESET",
        0xc5 => "SYNCOBJ_SIGNAL",
        0xca => "SYNCOBJ_TIMELINE_WAIT",
        0xcb => "SYNCOBJ_QUERY",
        0xcc => "SYNCOBJ_TRANSFER",
        0xcd => "SYNCOBJ_TIMELINE_SIGNAL",
        0xcf => "SYNCOBJ_EVENTFD",
        0xd1 => "SET_CLIENT_NAME",
        _ => return None,
    })
}

/// The name of the DRM driver behind `fd`, which its `DRM_IOCTL_VERSION`
/// answer writes into `buffer`; none for a file of no DRM driver.
fn driver_of(fd: c_int, buffer: &mut [u8; 16]) -> Option<&str> {
    /// `struct drm_version` (`uapi/drm/drm.h`).
    #[repr(C)]
    struct Version {
        major: c_int,
        minor: c_int,
        patch: c_int,
        name_bytes: usize,
        name: *mut u8,
        date_bytes: usize,
        date: *mut u8,
        description_bytes: usize,
        description: *mut u8,
    }
    let mut version = Version {
        major: 0,
        minor: 0,
        patch: 0,
        name_bytes: buffer.len(),
        name: buffer.as_mut_ptr(),
        date_bytes: 0,
        date: ptr::null_mut(),
        description_bytes: 0,
        description: ptr::null_mut(),
    };
    // SAFETY: entered on the Linux task of the call being reported, as that
    // call was; the argument is `struct drm_version` and its one buffer
    // outlives the call.
    let result =
        unsafe { stela_linux::client::control(fd, drm_calls::VERSION, (&raw mut version).cast()) };
    if result < 0 {
        return None;
    }
    // The answer gives the name's whole length and copies what fits.
    let bytes = version.name_bytes.min(buffer.len());
    core::str::from_utf8(&buffer[..bytes]).ok()
}

/// What amdgpu command `command` asked for, for the commands whose refusal
/// a Vulkan driver reports as out of memory, written after a space each.
#[cfg(feature = "amdgpu")]
fn describe(command: u32, input: &Input, out: &mut alloc::string::String) {
    use core::fmt::Write;
    // `struct drm_amdgpu_*` of `uapi/drm/amdgpu_drm.h`, at their offsets.
    let _ = match command {
        // gem_create_in: bo_size, alignment, domains, domain_flags.
        0x00 => write!(
            out,
            " size={:#x} alignment={:#x} domains={:#x} flags={:#x}",
            input.u64(0),
            input.u64(8),
            input.u64(16),
            input.u64(24)
        ),
        // gem_mmap_in: handle.
        0x01 => write!(out, " handle={}", input.u32(0)),
        // ctx_in: op, flags, ctx_id, priority.
        0x02 => write!(
            out,
            " op={} flags={:#x} ctx={} priority={}",
            input.u32(0),
            input.u32(4),
            input.u32(8),
            input.u32(12) as i32
        ),
        // info: return_pointer, return_size, query, then the query's own.
        0x05 => write!(
            out,
            " query={:#x} size={} arguments={:#x},{:#x}",
            input.u32(12),
            input.u32(8),
            input.u32(16),
            input.u32(20)
        ),
        // gem_va: handle, _pad, operation, flags, va_address, offset_in_bo,
        // map_size.
        0x08 => write!(
            out,
            " handle={} op={} flags={:#x} va={:#x} offset={:#x} size={:#x}",
            input.u32(0),
            input.u32(8),
            input.u32(12),
            input.u64(16),
            input.u64(24),
            input.u64(32)
        ),
        // gem_op: handle, op, value.
        0x10 => write!(
            out,
            " handle={} op={} value={:#x}",
            input.u32(0),
            input.u32(4),
            input.u64(8)
        ),
        // vm_in: op, flags.
        0x13 => write!(out, " op={} flags={:#x}", input.u32(0), input.u32(4)),
        _ => Ok(()),
    };
}

#[cfg(not(feature = "amdgpu"))]
fn describe(_: u32, _: &Input, _: &mut alloc::string::String) {}
