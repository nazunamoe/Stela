// SPDX-License-Identifier: GPL-2.0-only
/* Explicit native initialization order for original kernel resource caches. */
#include <linux/device.h>
#include <linux/errno.h>
#include <linux/export.h>
#include <linux/ioport.h>
#include <linux/sched/exec_state.h>
#include <linux/string.h>
#include <linux/user_namespace.h>
#include <drm/intel/i915_drm.h>
#include <stela/linux.h>
#include <stela/page.h>
#include <stela/resources.h>

/* Stela's owners assign device resources, so nothing is requested from this
 * tree; its span is x86-64's 52 physical address bits. */
struct resource iomem_resource = {
	.name = "PCI mem",
	.end = BIT_ULL(52) - 1,
	.flags = IORESOURCE_MEM,
};

/* arch/x86/kernel/early-quirks.c's: the graphics memory the firmware
 * reserved for an integrated Intel GPU. Stela's arch runs no early quirks;
 * the boot finds it from the same registers before any driver registers
 * (stela_linux_set_graphics_stolen). Empty, it says none is reserved. */
struct resource intel_graphics_stolen_res = DEFINE_RES_MEM(0, 0);
EXPORT_SYMBOL(intel_graphics_stolen_res);

void stela_linux_set_graphics_stolen(u64 base, u64 size)
{
	intel_graphics_stolen_res = (struct resource)DEFINE_RES_MEM_NAMED(base, size,
									   "Graphics Stolen Memory");
}

/* kernel/resource.c's, over a tree that holds no map here: the boot gave
 * the device's owner the range, so it is the caller's alone and the region
 * is only recorded for the device's lifetime. */
struct resource *__devm_request_region(struct device *dev, struct resource *parent,
				       resource_size_t start, resource_size_t n, const char *name)
{
	struct resource *region;

	if (!n || start + n - 1 < start || start < parent->start || start + n - 1 > parent->end)
		return NULL;
	region = devm_kzalloc(dev, sizeof(*region), GFP_KERNEL);
	if (!region)
		return NULL;
	region->name = name;
	region->start = start;
	region->end = start + n - 1;
	region->flags = resource_type(parent) | IORESOURCE_BUSY;
	region->parent = parent;
	return region;
}

/* kernel/resource.c's: a resource leaves the tree. Nothing is in this tree
 * (__devm_request_region records regions apart), so only the link to the
 * parent goes. */
int release_resource(struct resource *old)
{
	if (!old->parent)
		return -EINVAL;
	old->parent = NULL;
	return 0;
}

static_assert(!IS_ENABLED(CONFIG_USER_NS));
static_assert(sizeof(struct stela_linux_resource_request) == 16);
static_assert(offsetof(struct stela_linux_resource_request, page_generation) == 8);
/* Original kernel/user.c body, also retained by its normal subsys_initcall. */
int uid_cache_init(void);

enum resource_state { UNINITIALIZED, INITIALIZING, INITIALIZED, FAILED };
static enum resource_state state;

int stela_linux_resources_initialize(const struct stela_linux_resource_request *request)
{
    (void)stela_linux_owner();
    if (!stela_linux_quiescent())
        return -EBUSY;
    if (!request)
        return -EINVAL;
    u32 header[2];
    memcpy(header, request, sizeof(header));
    if (header[0] != STELA_LINUX_RESOURCES_V1 || header[1] != sizeof(*request))
        return -EINVAL;
    if (state != UNINITIALIZED)
        return -EBUSY;
    int error = stela_linux_page_catalog_retain_kernel(request->page_generation);
    if (error)
        return error;
    /* Every subsequent path retains the catalog and initial root objects.
     * Original SLAB_PANIC failures stop; they are not retryable partial init. */
    state = INITIALIZING;
    error = uid_cache_init();
    if (error) {
        state = FAILED;
        return error;
    }
    exec_state_init();
    state = INITIALIZED;
    return 0;
}
