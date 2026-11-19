/* SPDX-License-Identifier: GPL-2.0-only */
/* x86's cache line write-back instructions (asm/special_insns.h), which
 * x86's asm/processor.h gives every unit. CLFLUSH is on every x86-64
 * processor. CLFLUSHOPT, where the processor has it, writes back the same
 * line without CLFLUSH's ordering against other write-backs; its callers
 * order it with full barriers, as on x86, whose alternatives choose between
 * the two the same way. */
#ifndef _ASM_STELA_SPECIAL_INSNS_H
#define _ASM_STELA_SPECIAL_INSNS_H
#include <linux/compiler.h>
#include <linux/types.h>

static __always_inline void clflush(volatile void *line)
{
	asm volatile("clflush %0" : "+m"(*(volatile char *)line));
}

/* Whether the processor has CLFLUSHOPT, read once (kernel/cpu.c). */
bool stela_linux_has_clflushopt(void);

static __always_inline void clflushopt(volatile void *line)
{
	if (stela_linux_has_clflushopt())
		asm volatile("clflushopt %0" : "+m"(*(volatile char *)line));
	else
		clflush(line);
}
#endif
