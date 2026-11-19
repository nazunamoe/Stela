/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _ASM_STELA_PROCESSOR_H
#define _ASM_STELA_PROCESSOR_H
/* The processor services Linux code uses, from x86's asm/processor.h. */
#include <linux/compiler.h>
#include <linux/types.h>
#include <asm/cpufeature.h>
#include <asm/special_insns.h>

/* Runtime owns the actual saved CPU frame and retained stack. This Linux
 * task metadata contains no duplicate architecture context switch state. */
struct thread_struct {
};
/* There is genuinely no duplicate CPU frame in this native metadata. This
 * initializer does not construct x86 thread.sp, pt_regs or ret_from_fork. */
#define INIT_THREAD {}
/* The original static task template is not an executing native stack. Only
 * actual Runtime Service attachment installs its kept stack, once. */
#define INIT_TASK_STACK ((void *)0)


/* The CPU model facts Linux code here reads: family 6, past the i386, and
 * x86-64's 64-byte CLFLUSH line (CPUID 1, EBX bits 15:8, on every x86-64
 * processor). */
static const struct {
	u8 x86;
	u16 x86_clflush_size;
} boot_cpu_data __maybe_unused = {.x86 = 6, .x86_clflush_size = 64};

/* x86's struct cpuinfo_x86, with the fields Linux code here reads: the
 * maker, family and model of the processor, decoded from CPUID as Linux's
 * early CPU identification decodes them. Every processor is the boot
 * processor's kind; the values are asm/processor.h's. */
struct cpuinfo_x86 {
	u8 x86_vendor;
	u8 x86;
	u8 x86_model;
};
#define X86_VENDOR_INTEL 0
#define X86_VENDOR_AMD 2
#define X86_VENDOR_HYGON 9
#define X86_VENDOR_UNKNOWN 0xff
struct cpuinfo_x86 *stela_linux_cpu_data(unsigned int cpu);
#define cpu_data(cpu) (*stela_linux_cpu_data(cpu))

struct pt_regs;
struct task_struct;
/* Nor does a thread keep user registers. */
struct pt_regs *task_pt_regs(struct task_struct *task);

/* Busy-wait loops relax with x86's PAUSE, which is also a compiler barrier
 * as Linux's is. */
static __always_inline void cpu_relax(void)
{
	asm volatile("pause" ::: "memory");
}
#endif
