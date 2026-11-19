/* SPDX-License-Identifier: GPL-2.0-or-later */
/* Stela's Kconfig. Linux code runs on 64-bit x86 for one owner at a time,
 * without preemption or interrupts (see stela/linux.h): Linux's
 * uniprocessor, non-preemptible configuration of Stela's own arch. Options
 * not listed are disabled, so debug, tracing, KUnit and fault-injection
 * hooks compile out exactly as in a kernel built without them. */
#define CONFIG_64BIT 1
/* Select native architecture ownership hooks in the shared Linux sources. */
#define CONFIG_STELA 1
/* The compiler building this unit, as Kconfig records it: Linux keys its
 * clang diagnostic pragmas (__diag_clang_NN) to this version. */
#ifdef __clang__
#define CONFIG_CLANG_VERSION \
	(__clang_major__ * 10000 + __clang_minor__ * 100 + __clang_patchlevel__)
#endif
#define CONFIG_DEFAULT_HOSTNAME "(none)"
#define CONFIG_MMU 1
#define CONFIG_PCI 1
#define CONFIG_PCI_MSI 1
#define CONFIG_PRINTK 1
#define CONFIG_MESSAGE_LOGLEVEL_DEFAULT 4
#define CONFIG_FW_LOADER 1
/* Real generic IEEE/CRC32C library; no architecture dispatch. */
#define CONFIG_CRC32 1
/* Original XArray ordered/multi-index algorithms, not a layout substitute. */
#define CONFIG_XARRAY_MULTI 1
#define CONFIG_BUG 1
/* x86's WARN flags; asm/bug.h reports each site without a bug table. */
#define CONFIG_GENERIC_BUG 1
#define CONFIG_TINY_RCU 1
#define CONFIG_TINY_SRCU 1
/* Record balanced atomic/RCU sections; this enables no scheduling. */
#define CONFIG_PREEMPT_NONE 1
#define CONFIG_PREEMPT_COUNT 1
#define CONFIG_RT_MUTEXES 1
/* DMA fence array/chain callbacks use the deferred hard-IRQ worker service. */
#define CONFIG_IRQ_WORK 1
/* Linux's interrupt descriptors, chips, flow handlers and domains over the
 * lines owners route (kernel/irq.c). Descriptors are sparse, as on x86, so a
 * line's number has no bound. PCI_MSI selects GENERIC_MSI_IRQ, which selects
 * hierarchical domains, in every Linux configuration. */
#define CONFIG_SPARSE_IRQ 1
#define CONFIG_IRQ_DOMAIN 1
#define CONFIG_IRQ_DOMAIN_HIERARCHY 1
#define CONFIG_GENERIC_MSI_IRQ 1
/* Original task_struct embeds each retained continuation's thread_info. */
#define CONFIG_THREAD_INFO_IN_TASK 1
#define CONFIG_ARCH_DMA_ADDR_T_64BIT 1
/* Each device's owner supplies its dma_map_ops. Stela's arch is coherent, as
 * x86 is, so no DMA sync hooks are configured. */
#define CONFIG_HAS_DMA 1
#define CONFIG_ARCH_HAS_DMA_OPS 1
#define CONFIG_NEED_DMA_MAP_STATE 1
#define CONFIG_HAS_IOMEM 1
/* Port I/O, which Stela's arch has as x86 does (asm/io.h): the display code
 * xe shares with i915 depends on it for the VGA registers. */
#define CONFIG_HAS_IOPORT 1
#define CONFIG_PHYS_ADDR_T_64BIT 1
#define CONFIG_NR_CPUS 1
#define CONFIG_HZ 1000
#define CONFIG_PAGE_SHIFT 12
/* The architecture represents five levels and folds P4D at runtime when the
 * original native root has four. This is an ABI shape, not LA57 admission. */
#define CONFIG_PGTABLE_LEVELS 5
#define CONFIG_ILLEGAL_POINTER_VALUE 0xdead000000000000
/* asm/fpu.h: floating-point units run inside kernel_fpu_begin/end sections. */
#define CONFIG_ARCH_HAS_KERNEL_FPU_SUPPORT 1
/* nouveau selects the I2C core for its buses and encoders. */
#define CONFIG_I2C 1
#define CONFIG_I2C_ALGOBIT 1
/* nouveau and what its Kconfig entry selects. */
#define CONFIG_DRM 1
#define CONFIG_DRM_NOUVEAU 1
/* nouveau's panel backlight, as its Kconfig defaults: an eDP panel's DPCD
 * backlight is enabled at registration and at each mode set, and a PWM one
 * is registered at the level read back (nouveau_backlight.c). */
#define CONFIG_DRM_NOUVEAU_BACKLIGHT 1
#define CONFIG_DRM_TTM 1
#define CONFIG_DRM_TTM_HELPER 1
#define CONFIG_DRM_EXEC 1
#define CONFIG_DRM_GPUVM 1
#define CONFIG_DRM_SCHED 1
#define CONFIG_DRM_KMS_HELPER 1
#define CONFIG_DRM_DISPLAY_HELPER 1
#define CONFIG_DRM_DISPLAY_DP_HELPER 1
#define CONFIG_DRM_DISPLAY_HDMI_HELPER 1
#define CONFIG_DMA_SHARED_BUFFER 1
#define CONFIG_SYNC_FILE 1
/* amdgpu with its display core, and what its Kconfig entry selects.
 * DRM_AMD_DC under clang depends on X86_64 for the stack its bandwidth
 * calculation takes there; Stela's arch compiles for x86-64, so the
 * dependency's reason holds. */
#define CONFIG_DRM_AMDGPU 1
/* amdgpu's support of Southern Islands and Sea Islands GPUs
 * (DRM_AMDGPU_SI, DRM_AMDGPU_CIK) stays off: RADV's device on those
 * graphics generations cannot serve the Metal device, so Stela leaves
 * their GPUs out (stela-driver-core's `amd_metal`). */
#define CONFIG_DRM_AMD_DC 1
#define CONFIG_DRM_AMD_DC_FP 1
#define CONFIG_DRM_DISPLAY_DSC_HELPER 1
#define CONFIG_DRM_DISPLAY_HDCP_HELPER 1
#define CONFIG_DRM_BUDDY 1
#define CONFIG_GPU_BUDDY 1
#define CONFIG_DRM_SUBALLOC_HELPER 1
#define CONFIG_CRC16 1
#define CONFIG_INTERVAL_TREE 1
/* In-kernel DRM clients: the client interfaces, and the selection of the
 * default clients, of which none is configured (no fbdev emulation and no
 * log client), so DRM_CLIENT_LIB stays unselected. */
#define CONFIG_DRM_CLIENT 1
#define CONFIG_DRM_CLIENT_SELECTION 1
#define CONFIG_DRM_PANEL_BACKLIGHT_QUIRKS 1
/* The hwmon, backlight and power supply classes. No attribute file exists
 * (no SYSFS), so a device of one is what its driver keeps of it. */
#define CONFIG_HWMON 1
#define CONFIG_BACKLIGHT_CLASS_DEVICE 1
#define CONFIG_POWER_SUPPLY 1
/* Helper objects xe's entry selects besides those above: GPU SVM. VMAP_PFN
 * and REGMAP (which xe selects because I2C is set) build nothing here: a
 * register map is refused (kernel/unsupported.c). DEV_COREDUMP follows
 * WANT_DEV_COREDUMP only with ALLOW_DEV_COREDUMP, which is for systems where a
 * dump can be read; nothing reads sysfs here, so it is not set and a dump's
 * data is freed where it would be published. */
#define CONFIG_DRM_GPUSVM 1
#define CONFIG_VMAP_PFN 1
#define CONFIG_REGMAP 1
#define CONFIG_WANT_DEV_COREDUMP 1
/* Buses and helpers a driver's entry selects: xe's selects panels, the MIPI
 * DSI bus and the auxiliary bus. No driver of the image registers on either
 * bus. */
#define CONFIG_DRM_PANEL 1
#define CONFIG_DRM_MIPI_DSI 1
#define CONFIG_AUXILIARY_BUS 1
/* Every address space carries notifiers, for a driver that mirrors a client's
 * memory: DRM_GPUSVM, which xe's entry selects, selects both, and the
 * interval tree a ranged notifier is kept in (INTERVAL_TREE, above). Nothing
 * subscribes one here (kernel/unsupported.c says why). */
#define CONFIG_MMU_NOTIFIER 1
#define CONFIG_HMM_MIRROR 1

/* xe with display. DRM_XE_DISPLAY depends on DRM_XE=m, so that a built-in
 * i915 and xe never both define the display code they share; Stela builds no
 * modules and links no i915, so both are built in. Selected by xe's entry and
 * left out because nothing compiled reads them: SHMEM and TMPFS (as for TTM),
 * RELAY, and DRM_CLIENT_SELECTION, which acts only with DRM_FBDEV_EMULATION.
 * ACPI is not set, so its conditional selects (ACPI_VIDEO, ACPI_WMI, INPUT,
 * BACKLIGHT_CLASS_DEVICE) do not apply. DRM_XE_GPUSVM depends on ZONE_DEVICE
 * and DRM_XE_DP_TUNNEL on USB4, which are not set. */
#define CONFIG_DRM_XE 1
#define CONFIG_DRM_XE_DISPLAY 1
/* xe's ID table marks Tiger Lake through Meteor Lake and Arc A-series as
 * experimental and binds them only when this names them. i915 registers first
 * and binds those by default, as upstream does; xe is the fallback for one
 * whose i915 probe fails, which only this lets it bind, so it names every
 * device of the table. */
#define CONFIG_DRM_XE_FORCE_PROBE "*"

/* i915 with display, for the Intel GPUs before Xe2. Its entry depends on
 * X86, which names Linux's x86 arch; Stela's arch compiles for x86-64 with
 * x86's instructions and port I/O, so the dependency's reason holds, while
 * X86 itself stays unset. Of what i915 keys to it, the stolen memory the
 * arch's early quirks find is served; the chipset GTT driver of graphics
 * before Sandy Bridge and the SoC sideband bus of Bay Trail and Cherry
 * Trail are not built, as no driver here is handed those GPUs. Linux's
 * default force probe: none. */
#define CONFIG_DRM_I915 1
#define CONFIG_DRM_I915_FORCE_PROBE ""
#define CONFIG_DRM_I915_USERPTR 1

/* The drivers' parameter and scheduling defaults, as Linux's Kconfig sets them. */
#define CONFIG_DRM_I915_REQUEST_TIMEOUT 20000
#define CONFIG_DRM_I915_FENCE_TIMEOUT 10000
#define CONFIG_DRM_I915_USERFAULT_AUTOSUSPEND 250
#define CONFIG_DRM_I915_HEARTBEAT_INTERVAL 2500
#define CONFIG_DRM_I915_PREEMPT_TIMEOUT 640
#define CONFIG_DRM_I915_PREEMPT_TIMEOUT_COMPUTE 7500
#define CONFIG_DRM_I915_MAX_REQUEST_BUSYWAIT 8000
#define CONFIG_DRM_I915_STOP_TIMEOUT 100
#define CONFIG_DRM_I915_TIMESLICE_DURATION 1
#define CONFIG_DRM_XE_JOB_TIMEOUT_MAX 10000
#define CONFIG_DRM_XE_JOB_TIMEOUT_MIN 1
#define CONFIG_DRM_XE_TIMESLICE_MAX 10000000
#define CONFIG_DRM_XE_TIMESLICE_MIN 1
#define CONFIG_DRM_XE_PREEMPT_TIMEOUT 640000
#define CONFIG_DRM_XE_PREEMPT_TIMEOUT_MAX 10000000
#define CONFIG_DRM_XE_PREEMPT_TIMEOUT_MIN 1
#define CONFIG_NOUVEAU_DEBUG 5
#define CONFIG_NOUVEAU_DEBUG_DEFAULT 3
