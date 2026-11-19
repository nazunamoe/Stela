/* SPDX-License-Identifier: GPL-2.0-only */
/* x86's asm/smp.h as Linux code here reads it: writing back and
 * invalidating every processor's caches (kernel/cache.c). */
#ifndef _ASM_STELA_SMP_H
#define _ASM_STELA_SMP_H
void wbinvd_on_all_cpus(void);
#endif
