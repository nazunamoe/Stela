/* SPDX-License-Identifier: GPL-2.0-or-later */
/* An i915 render node with no GPU behind it, for host tests of ANV over i915:
 * Mesa's own i915 no-op shim (src/intel/tools/intel_noop_drm_shim.c, MIT,
 * compiled here unchanged) on Mesa's DRM shim, with each driver ioctl it
 * answers counted. At exit one line names them by the names i915_driver.c
 * registers them under: "i915 shim: ioctls GETPARAM=n ..." for the test to
 * compare with what Stela's i915 serves on a render node.
 *
 * The shim's device description from a JSON file (INTEL_STUB_GPU_JSON) needs
 * Mesa's generated serializer, which the host test does not build: that
 * input is refused with the reason on stderr, and the device comes from
 * INTEL_STUB_GPU_DEVICE_ID as the test sets it. */
#include <stdio.h>

#define drm_shim_driver_init mesa_i915_noop_driver_init
#include "tools/intel_noop_drm_shim.c"
#undef drm_shim_driver_init

void drm_shim_driver_init(void);

bool intel_device_info_from_json(const char *path, struct intel_device_info *devinfo)
{
	(void)devinfo;
	fprintf(stderr, "i915 shim: a device from JSON (%s) is not supported here\n", path);
	return false;
}

/* i915_driver.c's names, by ioctl number: it registers the numbers of
 * Mesa's EXECBUFFER2, MMAP_GTT and CONTEXT_CREATE as GEM_EXECBUFFER2_WR,
 * GEM_MMAP_OFFSET and GEM_CONTEXT_CREATE_EXT. */
static const char *const names[ARRAY_SIZE(driver_ioctls)] = {
	[DRM_I915_GETPARAM] = "GETPARAM",
	[DRM_I915_QUERY] = "QUERY",
	[DRM_I915_GET_RESET_STATS] = "GET_RESET_STATS",
	[DRM_I915_GEM_CREATE] = "GEM_CREATE",
	[DRM_I915_GEM_CREATE_EXT] = "GEM_CREATE_EXT",
	[DRM_I915_GEM_MMAP] = "GEM_MMAP",
	[DRM_I915_GEM_MMAP_GTT] = "GEM_MMAP_OFFSET",
	[DRM_I915_GEM_SET_TILING] = "GEM_SET_TILING",
	[DRM_I915_GEM_CONTEXT_CREATE] = "GEM_CONTEXT_CREATE_EXT",
	[DRM_I915_GEM_CONTEXT_DESTROY] = "GEM_CONTEXT_DESTROY",
	[DRM_I915_GEM_CONTEXT_GETPARAM] = "GEM_CONTEXT_GETPARAM",
	[DRM_I915_GEM_CONTEXT_SETPARAM] = "GEM_CONTEXT_SETPARAM",
	[DRM_I915_GEM_EXECBUFFER2] = "GEM_EXECBUFFER2_WR",
	[DRM_I915_GEM_USERPTR] = "GEM_USERPTR",
	[DRM_I915_GEM_GET_APERTURE] = "GEM_GET_APERTURE",
	[DRM_I915_REG_READ] = "REG_READ",
	[DRM_I915_GEM_SET_DOMAIN] = "GEM_SET_DOMAIN",
	[DRM_I915_GEM_GET_CACHING] = "GEM_GET_CACHING",
	[DRM_I915_GEM_SET_CACHING] = "GEM_SET_CACHING",
	[DRM_I915_GEM_GET_TILING] = "GEM_GET_TILING",
	[DRM_I915_GEM_MADVISE] = "GEM_MADVISE",
	[DRM_I915_GEM_WAIT] = "GEM_WAIT",
	[DRM_I915_GEM_BUSY] = "GEM_BUSY",
	[DRM_I915_LOAD_STUB_DEVINFO] = "LOAD_STUB_DEVINFO",
};
static ioctl_fn_t answered[ARRAY_SIZE(driver_ioctls)];
static unsigned counts[ARRAY_SIZE(driver_ioctls)];

static int counted(int fd, unsigned long request, void *arg)
{
	unsigned number = _IOC_NR(request) - DRM_COMMAND_BASE;

	counts[number]++;
	return answered[number](fd, request, arg);
}

static void report(void)
{
	fprintf(stderr, "i915 shim: ioctls");
	for (unsigned i = 0; i < ARRAY_SIZE(counts); i++)
		if (counts[i])
			fprintf(stderr, " %s=%u", names[i] ? names[i] : "UNNAMED", counts[i]);
	fprintf(stderr, "\n");
}

void drm_shim_driver_init(void)
{
	mesa_i915_noop_driver_init();
	/* The shim refused the device: no driver ioctls to count. */
	if (shim_device.driver_ioctls != driver_ioctls)
		return;
	for (unsigned i = 0; i < ARRAY_SIZE(driver_ioctls); i++) {
		answered[i] = driver_ioctls[i];
		if (answered[i])
			driver_ioctls[i] = counted;
	}
	atexit(report);
}
