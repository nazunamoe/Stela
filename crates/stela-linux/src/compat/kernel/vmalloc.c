// SPDX-License-Identifier: GPL-2.0-only
/* Linux's virtually contiguous memory, with the interface of Linux
 * cc9de5d4b5ab6459f5774946aa3195ad51236fa8 mm/vmalloc.c. Linux code under
 * Stela owns no address space, so the owner maps the pages (map_pages); this
 * keeps which pages back each mapping, for vmalloc_to_page and vfree. */
#include <linux/io.h>
#include <linux/llist.h>
#include <linux/mm.h>
#include <linux/mutex.h>
#include <linux/notifier.h>
#include <linux/overflow.h>
#include <linux/rbtree.h>
#include <linux/slab.h>
#include <linux/spinlock.h>
#include <linux/vmalloc.h>
#include <linux/workqueue.h>
#include <stela/catalogue.h>
#include <stela/linux.h>

struct area {
	struct rb_node node;
	const void *address;
	unsigned int count;
	/* vmalloc's pages are its own; vmap's stay its caller's. */
	bool owns_pages;
	struct page *pages[];
};

static struct rb_root areas = RB_ROOT;

static int area_order(const void *key, const struct rb_node *node)
{
	const struct area *area = rb_entry(node, struct area, node);
	const char *address = key, *start = area->address;

	if (address < start)
		return -1;
	return address < start + ((size_t)area->count << PAGE_SHIFT) ? 0 : 1;
}

static bool area_less(struct rb_node *node, const struct rb_node *other)
{
	return rb_entry(node, struct area, node)->address <
	       rb_entry(other, struct area, node)->address;
}

static struct area *area_of(const void *address)
{
	struct rb_node *node = rb_find(address, &areas, area_order);
	return node ? rb_entry(node, struct area, node) : NULL;
}

/* Maps area's pages and records the mapping, or leaves nothing mapped. */
/* mm/vmalloc.c's purge notifiers, kept in priority order: when no mapping
 * space is left, Linux asks them to release mappings they cache and tries
 * once more if they freed any. */
static struct notifier_block *purge_notifiers;
static DEFINE_MUTEX(purge_notifiers_mutex);

int register_vmap_purge_notifier(struct notifier_block *nb)
{
	struct notifier_block **link = &purge_notifiers;

	mutex_lock(&purge_notifiers_mutex);
	while (*link && (*link)->priority >= nb->priority)
		link = &(*link)->next;
	nb->next = *link;
	*link = nb;
	mutex_unlock(&purge_notifiers_mutex);
	return 0;
}

int unregister_vmap_purge_notifier(struct notifier_block *nb)
{
	struct notifier_block **link = &purge_notifiers;
	int error = -ENOENT;

	mutex_lock(&purge_notifiers_mutex);
	for (; *link; link = &(*link)->next)
		if (*link == nb) {
			*link = nb->next;
			error = 0;
			break;
		}
	mutex_unlock(&purge_notifiers_mutex);
	return error;
}

static unsigned long purge_cached_mappings(void)
{
	unsigned long freed = 0;

	mutex_lock(&purge_notifiers_mutex);
	for (struct notifier_block *nb = purge_notifiers; nb; nb = nb->next)
		if (nb->notifier_call(nb, 0, &freed) & NOTIFY_STOP_MASK)
			break;
	mutex_unlock(&purge_notifiers_mutex);
	return freed;
}

static void *map_area(struct area *area, bool write_combining)
{
	const struct stela_linux_owner *owner = stela_linux_owner();
	if (stela_linux_kernel_hosted() || !owner->services->map_pages) {
		stela_linux_unsupported("mapping pages");
		return NULL;
	}
	u64 *physical = kmalloc_array(area->count, sizeof(*physical), GFP_KERNEL);
	if (!physical)
		return NULL;
	for (unsigned int i = 0; i < area->count; i++)
		physical[i] = page_to_phys(area->pages[i]);
	void *address = owner->services->map_pages(owner->context, physical, area->count,
						    write_combining);
	if (!address && purge_cached_mappings())
		address = owner->services->map_pages(owner->context, physical, area->count,
						     write_combining);
	kfree(physical);
	if (address) {
		area->address = address;
		unsigned long flags;
		local_irq_save(flags);
		rb_add(&area->node, &areas, area_less);
		local_irq_restore(flags);
	}
	return address;
}

static struct area *new_area(unsigned int count, bool owns_pages)
{
	struct area *area = kmalloc(struct_size(area, pages, count), GFP_KERNEL);
	if (area) {
		area->count = count;
		area->owns_pages = owns_pages;
	}
	return area;
}

static void free_pages_of(struct area *area, unsigned int count)
{
	while (count)
		__free_page(area->pages[--count]);
}

void *__vmalloc_noprof(unsigned long size, gfp_t gfp_mask)
{
	unsigned long count = PAGE_ALIGN(size) >> PAGE_SHIFT;

	if (!count || count > UINT_MAX)
		return NULL;
	struct area *area = new_area((unsigned int)count, true);
	if (!area)
		return NULL;
	for (unsigned int i = 0; i < count; i++) {
		area->pages[i] = alloc_page(gfp_mask);
		if (!area->pages[i]) {
			free_pages_of(area, i);
			kfree(area);
			return NULL;
		}
	}
	void *address = map_area(area, false);
	if (!address) {
		free_pages_of(area, area->count);
		kfree(area);
	}
	return address;
}

void *vmalloc_noprof(unsigned long size)
{
	return __vmalloc_noprof(size, GFP_KERNEL);
}

void *vzalloc_noprof(unsigned long size)
{
	return __vmalloc_noprof(size, GFP_KERNEL | __GFP_ZERO);
}

/* Pages mapped write-back (PAGE_KERNEL) or write-combining
 * (pgprot_writecombine(PAGE_KERNEL)), as drivers map buffers the CPU fills
 * for the GPU; another protection is refused, said once. */
void *vmap(struct page **pages, unsigned int count, unsigned long flags, pgprot_t prot)
{
	bool write_combining = pgprot_val(prot) == pgprot_val(pgprot_writecombine(PAGE_KERNEL));

	(void)flags;
	if (pgprot_val(prot) != pgprot_val(PAGE_KERNEL) && !write_combining) {
		stela_linux_unsupported("vmap with another protection");
		return NULL;
	}
	struct area *area = new_area(count, false);
	if (!area)
		return NULL;
	memcpy(area->pages, pages, count * sizeof(*pages));
	void *address = map_area(area, write_combining);
	if (!address)
		kfree(area);
	return address;
}

/* Frames without pages of their own (a GPU's stolen memory), which the owner
 * maps as device memory it gave the driver: write-combining or uncached as
 * prot asks, contiguous frames only, as the owner maps one physical range.
 * Each is unmapped by vunmap. */
struct frames {
	struct list_head node;
	const void *address;
};

static LIST_HEAD(frame_mappings);
static DEFINE_SPINLOCK(frame_mappings_lock);

void *vmap_pfn(unsigned long *pfns, unsigned int count, pgprot_t prot)
{
	bool write_combining = (pgprot_val(prot) & _PAGE_PWT) && !(pgprot_val(prot) & _PAGE_PCD);
	struct frames *frames;
	void __iomem *address;

	if (!count)
		return NULL;
	for (unsigned int i = 1; i < count; i++)
		if (pfns[i] != pfns[0] + i) {
			stela_linux_unsupported("vmap_pfn of frames that are not contiguous");
			return NULL;
		}
	frames = kmalloc_obj(*frames);
	if (!frames)
		return NULL;
	address = write_combining ? ioremap_wc(PFN_PHYS(pfns[0]), (size_t)count << PAGE_SHIFT) :
				    ioremap(PFN_PHYS(pfns[0]), (size_t)count << PAGE_SHIFT);
	if (!address) {
		kfree(frames);
		return NULL;
	}
	frames->address = (const void __force *)address;
	spin_lock(&frame_mappings_lock);
	list_add(&frames->node, &frame_mappings);
	spin_unlock(&frame_mappings_lock);
	return (void __force *)address;
}

/* Whether address is a vmap_pfn mapping, which it then forgets. */
static bool unmap_frames(const void *address)
{
	struct frames *frames, *found = NULL;

	spin_lock(&frame_mappings_lock);
	list_for_each_entry(frames, &frame_mappings, node)
		if (frames->address == address) {
			list_del(&frames->node);
			found = frames;
			break;
		}
	spin_unlock(&frame_mappings_lock);
	if (!found)
		return false;
	iounmap((void __iomem __force *)address);
	kfree(found);
	return true;
}

static void unmap(const void *address, bool owns_pages)
{
	if (!address)
		return;
	const struct stela_linux_owner *owner = stela_linux_owner();
	unsigned long flags;
	local_irq_save(flags);
	struct area *area = area_of(address);
	if (WARN_ON(!area || area->address != address || area->owns_pages != owns_pages)) {
		local_irq_restore(flags);
		return;
	}
	rb_erase(&area->node, &areas);
	local_irq_restore(flags);
	owner->services->unmap_pages(owner->context, (void *)address, area->count);
	if (owns_pages)
		free_pages_of(area, area->count);
	kfree(area);
}

void vfree(const void *address)
{
	if (unlikely(in_interrupt())) {
		vfree_atomic(address);
		return;
	}
	unmap(address, true);
}

static LLIST_HEAD(vfree_pending);

static void vfree_work(struct work_struct *work)
{
	(void)work;
	struct llist_node *node, *next;
	llist_for_each_safe(node, next, llist_del_all(&vfree_pending))
		vfree(node);
}

static DECLARE_WORK(vfree_worker, vfree_work);

void vfree_atomic(const void *address)
{
	/* As in Linux, the relinquished mapping supplies the queue node. */
	if (address && llist_add((struct llist_node *)address, &vfree_pending))
		schedule_work(&vfree_worker);
}

void vunmap(const void *address)
{
	if (address && unmap_frames(address))
		return;
	unmap(address, false);
}

bool is_vmalloc_addr(const void *x)
{
	return area_of(x);
}

struct page *vmalloc_to_page(const void *address)
{
	struct area *area = area_of(address);
	if (!area)
		return NULL;
	return area->pages[((const char *)address - (const char *)area->address) >> PAGE_SHIFT];
}

/* As mm/slub.c does: slab first, without its warnings or retries when the
 * request is large, then pages that need not be contiguous. */
void *__kvmalloc_node_noprof(DECL_KMALLOC_PARAMS(size, b, token), unsigned long align,
			     gfp_t flags, int node)
{
	(void)node;
	if (WARN_ON_ONCE(align > PAGE_SIZE))
		return NULL;
	if (align <= ARCH_KMALLOC_MINALIGN) {
		gfp_t attempt = flags;
		if (size > PAGE_SIZE) {
			attempt |= __GFP_NOWARN;
			if (!(attempt & __GFP_RETRY_MAYFAIL))
				attempt |= __GFP_NORETRY;
			attempt &= ~__GFP_NOFAIL;
		}
		void *object = __kmalloc_noprof(PASS_TOKEN_PARAMS(size, token), attempt);
		if (object || size <= PAGE_SIZE)
			return object;
	}
	if (!gfpflags_allow_blocking(flags) || WARN_ON_ONCE(size > INT_MAX))
		return NULL;
	return __vmalloc_noprof(size, flags);
}

void *kvrealloc_node_align_noprof(const void *object, DECL_TOKEN_PARAMS(size, token),
				  unsigned long align, gfp_t flags, int node)
{
	if (!is_vmalloc_addr(object)) {
		void *resized = krealloc_node_align_noprof(object, PASS_TOKEN_PARAMS(size, token),
							   align, flags | __GFP_NOWARN, node);
		if (resized)
			return resized;
	}
	void *fresh = __kvmalloc_node_noprof(PASS_KMALLOC_PARAMS(size, NULL, token), align, flags,
					     node);
	if (!fresh || !object)
		return fresh;
	const struct area *area = area_of(object);
	size_t old_size = area ? (size_t)area->count << PAGE_SHIFT : ksize(object);
	memcpy(fresh, object, min(old_size, size));
	kvfree(object);
	return fresh;
}

#ifdef STELA_LINUX_VMALLOC_TEST
#include <stela/page.h>
#include <stela/test.h>

#define PAGES 8UL
#define PHYSICAL 0x40000000ULL

static u8 memory[PAGES * PAGE_SIZE] __aligned(PAGE_SIZE);
static size_t next_page;
static u64 pin_physical[PAGES];
static unsigned int released;
static bool exhausted;

static int validate(void *context, u32 gfp, unsigned int order, bool atomic)
{
	(void)context;
	(void)atomic;
	return !order && stela_linux_page_request_basic(gfp) ? 0 : -EOPNOTSUPP;
}

static int allocate(void *context, u32 gfp, unsigned int order, bool atomic,
		    struct stela_linux_page_allocation *allocation)
{
	(void)context;
	(void)gfp;
	(void)order;
	(void)atomic;
	if (exhausted || next_page == PAGES)
		return -ENOMEM;
	size_t page = next_page++;
	pin_physical[page] = PHYSICAL + page * PAGE_SIZE;
	*allocation = (struct stela_linux_page_allocation){
		.pin = &pin_physical[page],
		.cpu_base = memory + page * PAGE_SIZE,
		.physical_pages = &pin_physical[page],
		.page_count = 1,
	};
	return 0;
}

static void release(void *context, void *pin)
{
	(void)context;
	(void)pin;
	released++;
}

static const struct stela_linux_page_ops ops = {
	.validate = validate,
	.allocate = allocate,
	.release = release,
};

static u8 alias[2 * PAGE_SIZE];
static bool aliasing;

/* The pages are contiguous here, so the window already maps them; a second
 * mapping of them lies elsewhere, as the owner's would. */
static bool combined;

static void *map_pages(void *context, const u64 *physical, size_t count, bool write_combining)
{
	(void)context;
	combined = write_combining;
	for (size_t i = 1; i < count; i++)
		assert(physical[i] == physical[0] + i * PAGE_SIZE);
	return aliasing ? alias : memory + (physical[0] - PHYSICAL);
}

static unsigned int unmapped;

static void unmap_pages(void *context, void *address, size_t count)
{
	(void)context;
	(void)address;
	unmapped += (unsigned int)count;
}

static unsigned int refusals;

static void unsupported(void *context, const char *operation)
{
	(void)context;
	assert(!strcmp(operation, "vmap with another protection") ||
	       !strcmp(operation, "vmap_pfn of frames that are not contiguous"));
	refusals++;
}

static u8 device[3 * PAGE_SIZE] __aligned(PAGE_SIZE);
static u64 device_physical;
static size_t device_size;
static bool device_combined;

static void *map_device(void *context, u64 physical, size_t size, bool write_combining)
{
	(void)context;
	device_physical = physical;
	device_size = size;
	device_combined = write_combining;
	return device;
}

static unsigned int device_unmaps;

static void unmap_device(void *context, void *address)
{
	(void)context;
	assert(address == device);
	device_unmaps++;
}

static const struct stela_linux_services services = {
	.map_pages = map_pages,
	.unmap_pages = unmap_pages,
	.map_device = map_device,
	.unmap_device = unmap_device,
	.unsupported = unsupported,
};
static const struct stela_linux_owner owner = {.services = &services};

int main(void)
{
	stela_linux_enter(&owner);
	const struct stela_linux_page_reservation reservation = {
		.generation = 1, .physical_base = PHYSICAL, .cpu_base = memory, .page_count = PAGES};
	assert(!stela_linux_page_catalog_register(&reservation, &ops, NULL));

	u8 *zeroed = vzalloc(2 * PAGE_SIZE - 1);
	assert(zeroed == memory && !zeroed[PAGE_SIZE + 5] && is_vmalloc_addr(zeroed + PAGE_SIZE));
	assert(!is_vmalloc_addr(zeroed + 2 * PAGE_SIZE) && !is_vmalloc_addr(&next_page));
	struct page *second = vmalloc_to_page(zeroed + PAGE_SIZE + 5);
	assert(page_to_phys(second) == PHYSICAL + PAGE_SIZE);
	assert(!vmalloc_to_page(&next_page));

	struct page *pages[] = {vmalloc_to_page(zeroed), second};
	aliasing = true;
	u8 *again = vmap(pages, 2, VM_MAP, PAGE_KERNEL);
	aliasing = false;
	assert(again == alias && vmalloc_to_page(again + PAGE_SIZE) == second);
	vunmap(again);
	assert(unmapped == 2 && is_vmalloc_addr(zeroed) && !combined);
	/* Write-combining, as a driver maps what the CPU fills for its GPU. */
	aliasing = true;
	again = vmap(pages, 2, VM_MAP, pgprot_writecombine(PAGE_KERNEL));
	aliasing = false;
	assert(again == alias && combined);
	vunmap(again);
	assert(unmapped == 4);
	assert(!vmap(pages, 2, VM_MAP, PAGE_NONE) && refusals == 1);
	unmapped = 2;

	/* Contiguous frames without pages map as the owner's device memory. */
	unsigned long frames[] = {0x9000, 0x9001, 0x9002};
	u8 *stolen = vmap_pfn(frames, 3, pgprot_writecombine(PAGE_KERNEL_IO));
	assert(stolen == device && device_physical == 0x9000ULL << PAGE_SHIFT);
	assert(device_size == 3 * PAGE_SIZE && device_combined);
	vunmap(stolen);
	assert(device_unmaps == 1 && unmapped == 2);
	assert(vmap_pfn(frames, 2, pgprot_noncached(PAGE_KERNEL_IO)) == device && !device_combined);
	vunmap(device);
	frames[1] = 0x9005;
	assert(!vmap_pfn(frames, 3, pgprot_writecombine(PAGE_KERNEL_IO)) && refusals == 2);

	/* A request the pages cannot cover undoes what it allocated. */
	exhausted = false;
	next_page = PAGES - 1;
	assert(!vmalloc(2 * PAGE_SIZE));
	next_page = 2;

	/* Small requests stay in slab; kvfree frees either kind. */
	void *small = kvmalloc(32, GFP_KERNEL);
	assert(small && !is_vmalloc_addr(small));
	kvfree(small);
	void *large = vmalloc(PAGE_SIZE);
	assert(is_vmalloc_addr(large));
	kvfree(large);
	vfree(NULL);
	vfree(zeroed);
	assert(unmapped == 5);

	/* A slab object keeps its bytes when it grows. */
	u8 *grown = kvmalloc(16, GFP_KERNEL);
	grown[15] = 0x5a;
	u8 *resized = kvrealloc(grown, 32, GFP_KERNEL);
	assert(resized && !is_vmalloc_addr(resized) && resized[15] == 0x5a);
	kvfree(resized);
	stela_linux_exit();
	printf("Linux vmalloc over the owner's page mappings passed\n");
	return 0;
}
#endif
