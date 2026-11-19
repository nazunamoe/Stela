// SPDX-License-Identifier: GPL-2.0-or-later
/* DRM's cache line write-back, as drivers call it to make memory the CPU
 * wrote through its caches visible to a GPU or display engine that reads
 * past them (i915 for every such object). Linux takes x86's path only under
 * CONFIG_X86; elsewhere it warns once and writes back nothing. Stela's
 * kernel runs on x86-64 only, so it must take x86's path: a warning here,
 * which taints the kernel, is that path missing. */
#include <linux/panic.h>
#include <linux/string.h>
#include <drm/drm_cache.h>
#include <stela/linux.h>
#include <stela/test.h>

static u8 buffer[4 * 64] __aligned(64);

int main(void)
{
	stela_linux_enter(&stela_linux_bare);
	memset(buffer, 0x5a, sizeof(buffer));
	assert(!test_taint(TAINT_WARN));
	/* Unaligned at both ends, as callers pass a field within an object. */
	drm_clflush_virt_range(buffer + 3, sizeof(buffer) - 10);
	drm_clflush_virt_range(buffer, sizeof(buffer));
	assert(!test_taint(TAINT_WARN));
	for (size_t at = 0; at < sizeof(buffer); at++)
		assert(buffer[at] == 0x5a);
	stela_linux_exit();
	printf("Linux DRM cache: x86's line write-back, without the generic warning\n");
	return 0;
}
