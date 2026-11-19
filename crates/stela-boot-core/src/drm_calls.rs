//! A DRM driver's own ioctls by name, and which of its error answers are
//! its ordinary reply to a Vulkan driver's probe of what the GPU or the
//! kernel has. Both depend on the driver the file belongs to: every driver
//! numbers its commands from the same base, so one image that holds several
//! drivers must ask the file which one it is.

/// The first number of a driver's own commands (`DRM_COMMAND_BASE`).
pub const COMMAND_BASE: u32 = 0x40;

/// DRM's ioctl type, `'d'`.
const DRM: u32 = b'd' as u32;

/// `DRM_IOCTL_VERSION`: `DRM_IOWR(0x00, struct drm_version)`, 64 bytes on
/// x86-64. Its answer names the driver.
pub const VERSION: u32 = 0xc040_6400;

const ENOENT: i64 = -2;
const ENODEV: i64 = -19;

/// The name of `driver`'s command `command`, its ioctl number less
/// [`COMMAND_BASE`], from the driver's own uAPI header.
pub fn driver_call(driver: &str, command: u32) -> Option<&'static str> {
    let table: &[&'static str] = match driver {
        "amdgpu" => &AMDGPU,
        "xe" => &XE,
        "i915" => &I915,
        "nouveau" => return nouveau(command),
        _ => return None,
    };
    table
        .get(command as usize)
        .copied()
        .filter(|name| !name.is_empty())
}

/// `amdgpu_drm.h`, from `DRM_AMDGPU_GEM_CREATE` (0x00).
const AMDGPU: [&str; 0x16] = [
    "AMDGPU_GEM_CREATE",
    "AMDGPU_GEM_MMAP",
    "AMDGPU_CTX",
    "AMDGPU_BO_LIST",
    "AMDGPU_CS",
    "AMDGPU_INFO",
    "AMDGPU_GEM_METADATA",
    "AMDGPU_GEM_WAIT_IDLE",
    "AMDGPU_GEM_VA",
    "AMDGPU_WAIT_CS",
    "",
    "",
    "",
    "",
    "",
    "",
    "AMDGPU_GEM_OP",
    "AMDGPU_GEM_USERPTR",
    "AMDGPU_WAIT_FENCES",
    "AMDGPU_VM",
    "AMDGPU_FENCE_TO_HANDLE",
    "AMDGPU_SCHED",
];

/// `xe_drm.h`, from `DRM_XE_DEVICE_QUERY` (0x00).
const XE: [&str; 0x10] = [
    "XE_DEVICE_QUERY",
    "XE_GEM_CREATE",
    "XE_GEM_MMAP_OFFSET",
    "XE_VM_CREATE",
    "XE_VM_DESTROY",
    "XE_VM_BIND",
    "XE_EXEC_QUEUE_CREATE",
    "XE_EXEC_QUEUE_DESTROY",
    "XE_EXEC_QUEUE_GET_PROPERTY",
    "XE_EXEC",
    "XE_WAIT_USER_FENCE",
    "XE_OBSERVATION",
    "XE_MADVISE",
    "XE_VM_QUERY_MEM_RANGE_ATTRS",
    "XE_EXEC_QUEUE_SET_PROPERTY",
    "XE_VM_GET_PROPERTY",
];

/// `i915_drm.h`, from `DRM_I915_INIT` (0x00); a number two ioctls share
/// (by their argument's size) carries both names.
const I915: [&str; 0x3d] = [
    "I915_INIT",
    "I915_FLUSH",
    "I915_FLIP",
    "I915_BATCHBUFFER",
    "I915_IRQ_EMIT",
    "I915_IRQ_WAIT",
    "I915_GETPARAM",
    "I915_SETPARAM",
    "I915_ALLOC",
    "I915_FREE",
    "I915_INIT_HEAP",
    "I915_CMDBUFFER",
    "I915_DESTROY_HEAP",
    "I915_SET_VBLANK_PIPE",
    "I915_GET_VBLANK_PIPE",
    "I915_VBLANK_SWAP",
    "",
    "I915_HWS_ADDR",
    "",
    "I915_GEM_INIT",
    "I915_GEM_EXECBUFFER",
    "I915_GEM_PIN",
    "I915_GEM_UNPIN",
    "I915_GEM_BUSY",
    "I915_GEM_THROTTLE",
    "I915_GEM_ENTERVT",
    "I915_GEM_LEAVEVT",
    "I915_GEM_CREATE",
    "I915_GEM_PREAD",
    "I915_GEM_PWRITE",
    "I915_GEM_MMAP",
    "I915_GEM_SET_DOMAIN",
    "I915_GEM_SW_FINISH",
    "I915_GEM_SET_TILING",
    "I915_GEM_GET_TILING",
    "I915_GEM_GET_APERTURE",
    "I915_GEM_MMAP_GTT/OFFSET",
    "I915_GET_PIPE_FROM_CRTC_ID",
    "I915_GEM_MADVISE",
    "I915_OVERLAY_PUT_IMAGE",
    "I915_OVERLAY_ATTRS",
    "I915_GEM_EXECBUFFER2",
    "I915_GET_SPRITE_COLORKEY",
    "I915_SET_SPRITE_COLORKEY",
    "I915_GEM_WAIT",
    "I915_GEM_CONTEXT_CREATE(_EXT)",
    "I915_GEM_CONTEXT_DESTROY",
    "I915_GEM_SET_CACHING",
    "I915_GEM_GET_CACHING",
    "I915_REG_READ",
    "I915_GET_RESET_STATS",
    "I915_GEM_USERPTR",
    "I915_GEM_CONTEXT_GETPARAM",
    "I915_GEM_CONTEXT_SETPARAM",
    "I915_PERF_OPEN",
    "I915_PERF_ADD_CONFIG",
    "I915_PERF_REMOVE_CONFIG",
    "I915_QUERY",
    "I915_GEM_VM_CREATE",
    "I915_GEM_VM_DESTROY",
    "I915_GEM_CREATE_EXT",
];

/// `nouveau_drm.h`, whose numbers have gaps.
fn nouveau(command: u32) -> Option<&'static str> {
    Some(match command {
        0x00 => "NOUVEAU_GETPARAM",
        0x01 => "NOUVEAU_SETPARAM",
        0x02 => "NOUVEAU_CHANNEL_ALLOC",
        0x03 => "NOUVEAU_CHANNEL_FREE",
        0x04 => "NOUVEAU_GROBJ_ALLOC",
        0x05 => "NOUVEAU_NOTIFIEROBJ_ALLOC",
        0x06 => "NOUVEAU_GPUOBJ_FREE",
        0x07 => "NOUVEAU_NVIF",
        0x08 => "NOUVEAU_SVM_INIT",
        0x09 => "NOUVEAU_SVM_BIND",
        0x10 => "NOUVEAU_VM_INIT",
        0x11 => "NOUVEAU_VM_BIND",
        0x12 => "NOUVEAU_EXEC",
        0x13 => "NOUVEAU_GET_ZCULL_INFO",
        0x40 => "NOUVEAU_GEM_NEW",
        0x41 => "NOUVEAU_GEM_PUSHBUF",
        0x42 => "NOUVEAU_GEM_CPU_PREP",
        0x43 => "NOUVEAU_GEM_CPU_FINI",
        0x44 => "NOUVEAU_GEM_INFO",
        _ => return None,
    })
}

/// `DRM_IOCTL_XE_DEVICE_QUERY`: `DRM_IOWR(0x40, struct drm_xe_device_query)`,
/// 40 bytes, its query kind at byte 8.
const XE_DEVICE_QUERY: u32 = 0xc028_6440;
/// `DRM_XE_DEVICE_QUERY_PXP_STATUS`.
const XE_QUERY_PXP_STATUS: u32 = 9;
/// `DRM_IOCTL_XE_VM_GET_PROPERTY`: `DRM_IOWR(0x4f, struct
/// drm_xe_vm_get_property)`, 56 bytes, its VM at byte 8.
const XE_VM_GET_PROPERTY: u32 = 0xc038_644f;

/// `DRM_IOCTL_I915_GETPARAM`: `DRM_IOWR(0x46, drm_i915_getparam_t)`, 16
/// bytes, its parameter at byte 0.
const I915_GETPARAM: u32 = 0xc010_6446;
/// `I915_PARAM_IRQ_ACTIVE`, a parameter i915 answers `-ENODEV` for.
const I915_PARAM_IRQ_ACTIVE: u32 = 1;
/// `I915_PARAM_PXP_STATUS`.
const I915_PARAM_PXP_STATUS: u32 = 58;

/// Whether `driver`'s answer `result` to ioctl `request`, whose argument
/// began with `input` as the caller passed it, is the driver's ordinary
/// reply to a probe a Vulkan driver makes on every device and handles:
/// not a refusal worth a line.
///
/// - xe, `DEVICE_QUERY` of `PXP_STATUS`, `-ENODEV`: ANV asks on every
///   graphics 12 and later GPU whether protected content can run
///   (`xe_gem_supports_protected_exec_queue`). xe has none without the MEI
///   GSC proxy driver, which Stela's Linux does not build, and says so with
///   `-ENODEV` (`xe_pxp_get_readiness_status`).
/// - xe, `VM_GET_PROPERTY` of VM `u32::MAX`, `-ENOENT`: on Xe2 and later
///   ANV detects VM fault reporting by asking for a VM no file has
///   (`xe_gem_supports_get_vm_faults`); `-ENOENT` is the answer it wants.
/// - i915, `GETPARAM` of `PXP_STATUS`, `-ENODEV`: ANV asks on every GPU
///   whether protected content can run (`i915_gem_supports_protected_context`)
///   and takes `-ENODEV` as no; i915 has no PXP without
///   `CONFIG_DRM_I915_PXP` and the MEI drivers (`intel_pxp_get_readiness_status`).
/// - i915, `GETPARAM` of `IRQ_ACTIVE`, `-ENODEV`: Mesa's i915 device info
///   asks for the parameter numbered by the boolean answer to
///   `HAS_CONTEXT_ISOLATION` (`intel/dev/i915/intel_device_info.c`, a getparam
///   inside a getparam's argument), which is 1 on every GPU i915 is handed; i915
///   retired that parameter and answers `-ENODEV`, which Mesa ignores.
pub fn probe_answer(driver: &str, request: u32, input: &[u8], result: i64) -> bool {
    let word = |at: usize| {
        input
            .get(at..at + 4)
            .map(|bytes| u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    };
    match (driver, request, result) {
        ("xe", XE_DEVICE_QUERY, ENODEV) => word(8) == Some(XE_QUERY_PXP_STATUS),
        ("xe", XE_VM_GET_PROPERTY, ENOENT) => word(8) == Some(u32::MAX),
        ("i915", I915_GETPARAM, ENODEV) => {
            matches!(word(0), Some(I915_PARAM_PXP_STATUS | I915_PARAM_IRQ_ACTIVE))
        }
        _ => false,
    }
}

/// The driver command `request` is, if it is DRM's ioctl type and past
/// DRM's own commands.
pub fn driver_command(request: u32) -> Option<u32> {
    let number = request & 0xff;
    ((request >> 8) & 0xff == DRM)
        .then(|| number.checked_sub(COMMAND_BASE))
        .flatten()
        .filter(|command| *command < 0x60)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `_IOC(read | write, 'd', number, size)`, as Linux's `DRM_IOWR`.
    fn iowr(number: u32, size: u32) -> u32 {
        3 << 30 | size << 16 | DRM << 8 | number
    }

    #[test]
    fn a_driver_command_is_named_by_the_driver_of_its_file() {
        // The same number is a different call for each driver: the
        // development image holds all four.
        let request = iowr(COMMAND_BASE, 40);
        let command = driver_command(request).unwrap();
        assert_eq!(command, 0);
        assert_eq!(driver_call("xe", command), Some("XE_DEVICE_QUERY"));
        assert_eq!(driver_call("amdgpu", command), Some("AMDGPU_GEM_CREATE"));
        assert_eq!(driver_call("i915", command), Some("I915_INIT"));
        assert_eq!(driver_call("nouveau", command), Some("NOUVEAU_GETPARAM"));
        assert_eq!(driver_call("vgem", command), None);
        // The last of each table, and the gaps.
        assert_eq!(driver_call("xe", 0x0f), Some("XE_VM_GET_PROPERTY"));
        assert_eq!(driver_call("xe", 0x10), None);
        assert_eq!(driver_call("i915", 0x29), Some("I915_GEM_EXECBUFFER2"));
        assert_eq!(driver_call("i915", 0x3c), Some("I915_GEM_CREATE_EXT"));
        assert_eq!(driver_call("i915", 0x10), None);
        assert_eq!(driver_call("amdgpu", 0x0a), None);
        assert_eq!(driver_call("amdgpu", 0x15), Some("AMDGPU_SCHED"));
        assert_eq!(driver_call("nouveau", 0x44), Some("NOUVEAU_GEM_INFO"));
        // DRM's own commands and other ioctl types are no driver's.
        assert_eq!(driver_command(VERSION), None);
        assert_eq!(driver_command(0xc008_6200 | 0x02), None);
    }

    #[test]
    fn the_request_codes_are_the_uapi_headers() {
        assert_eq!(VERSION, iowr(0x00, 64));
        assert_eq!(XE_DEVICE_QUERY, iowr(COMMAND_BASE, 40));
        assert_eq!(XE_VM_GET_PROPERTY, iowr(COMMAND_BASE + 0x0f, 56));
        assert_eq!(I915_GETPARAM, iowr(COMMAND_BASE + 0x06, 16));
    }

    #[test]
    fn i915s_answers_to_anvs_parameter_probes_are_not_refusals() {
        let param = |number: u32| {
            let mut argument = [0u8; 16];
            argument[..4].copy_from_slice(&number.to_le_bytes());
            argument
        };
        for number in [I915_PARAM_PXP_STATUS, I915_PARAM_IRQ_ACTIVE] {
            assert!(probe_answer("i915", I915_GETPARAM, &param(number), ENODEV));
            // Another errno or another driver is a refusal.
            assert!(!probe_answer("i915", I915_GETPARAM, &param(number), -22));
            assert!(!probe_answer("xe", I915_GETPARAM, &param(number), ENODEV));
        }
        // Any other parameter's -ENODEV is (the chipset ID, slice mask).
        for number in [4, 46] {
            assert!(!probe_answer("i915", I915_GETPARAM, &param(number), ENODEV));
        }
        assert!(!probe_answer("i915", I915_GETPARAM, &[], ENODEV));
    }

    #[test]
    fn xes_answers_to_anvs_feature_probes_are_not_refusals() {
        let mut query = [0u8; 40];
        query[8..12].copy_from_slice(&XE_QUERY_PXP_STATUS.to_le_bytes());
        assert!(probe_answer("xe", XE_DEVICE_QUERY, &query, ENODEV));
        // Another query, another errno or another driver is a refusal.
        assert!(!probe_answer("xe", XE_DEVICE_QUERY, &query, -22));
        assert!(!probe_answer("amdgpu", XE_DEVICE_QUERY, &query, ENODEV));
        let mut config = [0u8; 40];
        config[8..12].copy_from_slice(&2u32.to_le_bytes());
        assert!(!probe_answer("xe", XE_DEVICE_QUERY, &config, ENODEV));

        let mut faults = [0u8; 56];
        faults[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(probe_answer("xe", XE_VM_GET_PROPERTY, &faults, ENOENT));
        let mut real = [0u8; 56];
        real[8..12].copy_from_slice(&1u32.to_le_bytes());
        assert!(!probe_answer("xe", XE_VM_GET_PROPERTY, &real, ENOENT));
        assert!(!probe_answer(
            "xe",
            XE_VM_GET_PROPERTY,
            &faults[..8],
            ENOENT
        ));
    }
}
