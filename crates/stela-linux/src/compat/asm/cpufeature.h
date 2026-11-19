/* SPDX-License-Identifier: GPL-2.0-only */
/* x86's processor feature tests (asm/cpufeature.h) for the features Linux
 * code here reads, numbered as x86's asm/cpufeatures.h numbers them: word
 * times 32 plus the bit. Each is the CPUID bit of the processor Linux runs
 * on, read when asked, since Stela's arch keeps no boot-time copy. */
#ifndef _ASM_STELA_CPUFEATURE_H
#define _ASM_STELA_CPUFEATURE_H
#include <linux/types.h>

/* Word 0: CPUID 1, EDX. */
#define X86_FEATURE_PAT		(0 * 32 + 16)
#define X86_FEATURE_CLFLUSH	(0 * 32 + 19)
/* Word 4: CPUID 1, ECX. */
#define X86_FEATURE_XMM4_1	(4 * 32 + 19)
#define X86_FEATURE_HYPERVISOR	(4 * 32 + 31)
/* Word 9: CPUID 7 subleaf 0, EBX. */
#define X86_FEATURE_CLFLUSHOPT	(9 * 32 + 23)

/* Whether the processor has `feature`, one of the numbers above. */
bool stela_linux_cpu_has(unsigned int feature);

#define boot_cpu_has(feature) stela_linux_cpu_has(feature)
#define static_cpu_has(feature) stela_linux_cpu_has(feature)
#define cpu_feature_enabled(feature) stela_linux_cpu_has(feature)

#endif
