/* SPDX-License-Identifier: GPL-2.0-only */
/* x86's asm/set_memory.h: changing the memory type of the kernel's own
 * mapping of pages. Stela's arch maps each page with the type its mapping
 * asks for (ioremap_wc, vmap with pgprot_writecombine), so it changes no
 * existing mapping's type; these are declared for the units that include
 * the header, and a unit that calls one imports it. */
#ifndef _ASM_STELA_SET_MEMORY_H
#define _ASM_STELA_SET_MEMORY_H
struct page;
int set_memory_uc(unsigned long addr, int numpages);
int set_memory_wc(unsigned long addr, int numpages);
int set_memory_wb(unsigned long addr, int numpages);
int set_pages_uc(struct page *page, int numpages);
int set_pages_wb(struct page *page, int numpages);
#endif
