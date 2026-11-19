// SPDX-License-Identifier: GPL-2.0-or-later
/* A dma-buf's descriptor and its fences as ANV asks for them on i915 before
 * it creates a device (`i915_gem_supports_dma_buf_sync_file`): the buffer is
 * exported to a descriptor, its fences come out as a sync file
 * (DMA_BUF_IOCTL_EXPORT_SYNC_FILE) and a sync file's fence goes back in
 * (DMA_BUF_IOCTL_IMPORT_SYNC_FILE), each through the descriptor table and
 * the ioctl entry Stela's clients reach Linux by, and every descriptor's
 * close releases what it held. dma-buf.c and dma-fence.c are the pinned
 * upstream files, included here so what only their initcalls make (the
 * buffers' filesystem mount, the signaled stub fence an idle buffer's
 * export hands out) is made by this program, as the image's start makes
 * it. */
/* The upstream file as Kbuild builds it, without this fixture's stricter
 * warnings. */
#pragma clang diagnostic push
#pragma clang diagnostic ignored "-Wconversion"
#pragma clang diagnostic ignored "-Wsign-conversion"
#pragma clang diagnostic ignored "-Wshorten-64-to-32"
#pragma clang diagnostic ignored "-Wunused-parameter"
#pragma clang diagnostic ignored "-Wsign-compare"
#pragma clang diagnostic ignored "-Wmissing-field-initializers"
#include "../../../../../third_party/linux/drivers/dma-buf/dma-buf.c"
#include "../../../../../third_party/linux/drivers/dma-buf/dma-fence.c"
#pragma clang diagnostic pop
#include <linux/init_task.h>
#include <linux/radix-tree.h>
#include <stela/allocation.h>
#include <stela/file.h>
#include <stela/linux.h>
#include <stela/page_test.h>
#include <stela/test.h>

extern void __init dcache_init(void);
extern void __init mnt_cache_init(void);
void kvfree_rcu_cb(struct rcu_head *head);

static unsigned int releases;

/* The fixture is single-threaded and has no RCU or workqueue workers, as
 * the descriptor table's own fixture (`kernel/file.c`). */
static struct rcu_head *retiring[32];
static rcu_callback_t retirement[ARRAY_SIZE(retiring)];
static unsigned int queued;

struct task_struct *__wrap_stela_linux_current(void)
{
	return &init_task;
}

void __wrap_call_rcu(struct rcu_head *head, rcu_callback_t callback)
{
	assert(queued < ARRAY_SIZE(retiring));
	retiring[queued] = head;
	retirement[queued++] = callback;
}

int __wrap___cond_resched(void)
{
	return 0;
}

void __wrap_synchronize_rcu(void)
{
}

bool __wrap_queue_work_on(int cpu, struct workqueue_struct *queue, struct work_struct *work)
{
	(void)cpu, (void)queue;
	work->func(work);
	return true;
}

static struct sg_table *test_map(struct dma_buf_attachment *attachment,
				 enum dma_data_direction direction)
{
	(void)attachment, (void)direction;
	return ERR_PTR(-ENODEV);
}

static void test_unmap(struct dma_buf_attachment *attachment, struct sg_table *table,
		       enum dma_data_direction direction)
{
	(void)attachment, (void)table, (void)direction;
}

static void test_release(struct dma_buf *buffer)
{
	(void)buffer;
	releases++;
}

static const struct dma_buf_ops test_ops = {
	.map_dma_buf = test_map,
	.unmap_dma_buf = test_unmap,
	.release = test_release,
};

static const char *test_name(struct dma_fence *fence)
{
	(void)fence;
	return "test";
}

static const struct dma_fence_ops fence_ops = {
	.get_driver_name = test_name,
	.get_timeline_name = test_name,
};

static s64 test_now_ns(void *context)
{
	(void)context;
	return 0;
}

static bool test_realtime_at(void *context, s64 monotonic, u32 mode, u32 tick_ns,
			     s64 *seconds, u32 *nanoseconds)
{
	(void)context, (void)monotonic, (void)mode, (void)tick_ns;
	*seconds = 0;
	*nanoseconds = 0;
	return true;
}

/* The client's memory is this program's, as a client's is reached through
 * the owner's copies. */
static unsigned long test_copy(void *context, void *to, const void *from, unsigned long size)
{
	(void)context;
	memcpy(to, from, size);
	return 0;
}

/* The fence of the sync file `fd` names, signaled or not. */
static bool sync_file_signaled(int fd)
{
	struct dma_fence *fence = sync_file_get_fence(fd);

	assert(fence);
	bool signaled = dma_fence_is_signaled(fence);

	dma_fence_put(fence);
	return signaled;
}

int main(void)
{
	static const struct stela_linux_services services = {
		.now_ns = test_now_ns, .realtime_at = test_realtime_at,
		.copy_from_user = test_copy, .copy_to_user = test_copy,
	};
	static const struct stela_linux_owner owner = {.services = &services};
	stela_linux_enter(&owner);
	stela_test_register_pages();
	radix_tree_init();
	vfs_caches_init_early();
	dcache_init();
	inode_init();
	mnt_cache_init();
	files_init();
	assert(!dma_fence_init_stub());
	assert(!dma_buf_init());

	DEFINE_DMA_BUF_EXPORT_INFO(info);
	info.ops = &test_ops;
	info.size = 64 << 10;
	info.flags = O_RDWR;
	info.priv = &releases;
	struct dma_buf *buffer = dma_buf_export(&info);

	assert(!IS_ERR(buffer));
	int fd = dma_buf_fd(buffer, O_CLOEXEC);

	assert(fd >= 0);

	/* No fence yet: the sync file holds one already signaled. */
	struct dma_buf_export_sync_file exported = {.flags = DMA_BUF_SYNC_RW, .fd = -1};

	assert(!stela_linux_ioctl(fd, DMA_BUF_IOCTL_EXPORT_SYNC_FILE, &exported));
	assert(exported.fd >= 0 && exported.fd != fd && sync_file_signaled(exported.fd));
	/* ANV's probe imports the file it exported, for reads and writes. */
	struct dma_buf_import_sync_file imported = {.flags = DMA_BUF_SYNC_RW, .fd = exported.fd};

	assert(!stela_linux_ioctl(fd, DMA_BUF_IOCTL_IMPORT_SYNC_FILE, &imported));
	stela_linux_fd_close(exported.fd);

	/* A fence not yet signaled, imported as a write: a read must wait for
	 * it, and its signal ends the wait the exported file stands for. */
	static DEFINE_SPINLOCK(lock);
	struct dma_fence *fence = kzalloc(sizeof(*fence), GFP_KERNEL);

	assert(fence);
	dma_fence_init(fence, &fence_ops, &lock, dma_fence_context_alloc(1), 1);
	struct sync_file *pending = sync_file_create(fence);

	assert(pending);
	int pending_fd = get_unused_fd_flags(O_CLOEXEC);

	assert(pending_fd >= 0);
	fd_install((unsigned int)pending_fd, pending->file);
	imported = (struct dma_buf_import_sync_file){.flags = DMA_BUF_SYNC_WRITE, .fd = pending_fd};
	assert(!stela_linux_ioctl(fd, DMA_BUF_IOCTL_IMPORT_SYNC_FILE, &imported));
	stela_linux_fd_close(pending_fd);
	exported = (struct dma_buf_export_sync_file){.flags = DMA_BUF_SYNC_READ, .fd = -1};
	assert(!stela_linux_ioctl(fd, DMA_BUF_IOCTL_EXPORT_SYNC_FILE, &exported));
	assert(!sync_file_signaled(exported.fd));
	dma_fence_signal(fence);
	assert(sync_file_signaled(exported.fd));
	stela_linux_fd_close(exported.fd);

	/* Flags outside the uAPI's, and a descriptor that is no sync file, are
	 * refused as Linux refuses them. */
	exported = (struct dma_buf_export_sync_file){.flags = 4, .fd = -1};
	assert(stela_linux_ioctl(fd, DMA_BUF_IOCTL_EXPORT_SYNC_FILE, &exported) == -EINVAL);
	imported = (struct dma_buf_import_sync_file){.flags = DMA_BUF_SYNC_RW, .fd = fd};
	assert(stela_linux_ioctl(fd, DMA_BUF_IOCTL_IMPORT_SYNC_FILE, &imported) == -EINVAL);

	/* The buffer's last descriptor's close releases it, with the fences
	 * its reservation held. */
	assert(!releases);
	stela_linux_fd_close(fd);
	assert(releases == 1);
	dma_fence_put(fence);
	for (unsigned int index = 0; index < queued; index++) {
		/* kvfree_rcu_cb frees only inside the RCU drain's callback
		 * context, which this fixture has none of: free what it would. */
		if (retirement[index] == kvfree_rcu_cb) {
			struct stela_linux_allocation allocation;

			assert(stela_linux_allocation_range(retiring[index],
							    sizeof(*retiring[index]), &allocation));
			kvfree(allocation.object);
		} else {
			retirement[index](retiring[index]);
		}
	}
	stela_linux_exit();
	printf("Linux dma-buf sync files passed\n");
	return 0;
}
