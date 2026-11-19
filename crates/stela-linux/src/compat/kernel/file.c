// SPDX-License-Identifier: GPL-2.0-only
/* Linux's files, with the interfaces of Linux
 * cc9de5d4b5ab6459f5774946aa3195ad51236fa8 fs/file.c, fs/file_table.c,
 * fs/anon_inodes.c, fs/char_dev.c and fs/libfs.c. Stela has no filesystem:
 * a file is an inode for its driver's mapping and a reference count, its
 * drivers reach it through their file_operations, and a pseudo filesystem's
 * mount is the superblock its dentries take their operations from. */
#include <linux/anon_inodes.h>
#include <linux/cdev.h>
#include <linux/cred.h>
#include <linux/dcache.h>
#include <linux/err.h>
#include <linux/fdtable.h>
#include <linux/file.h>
#include <linux/fs.h>
#include <linux/fs_context.h>
#include <linux/kdev_t.h>
#include <linux/llist.h>
#include <linux/mount.h>
#include <linux/pseudo_fs.h>
#include <linux/shmem_fs.h>
#include <linux/pagemap.h>
#include <linux/uio.h>
#include <linux/writeback.h>
#include <linux/xarray.h>
#include <linux/jiffies.h>
#include <linux/slab.h>
#include <linux/wait.h>
#include <linux/workqueue.h>
#include <stela/file.h>
#include <stela/linux.h>
#include <unsupported.h>

static struct inode *inode_of_mode(umode_t mode)
{
	struct inode *inode = kmalloc_obj(*inode, GFP_KERNEL);

	if (!inode)
		return ERR_PTR(-ENOMEM);
	inode_init_once(inode);
	atomic_set(&inode->i_count, 1);
	inode->i_mode = mode;
	inode->i_mapping = &inode->i_data;
	inode->i_data.host = inode;
	return inode;
}

void stela_linux_compat_iput(struct inode *inode)
{
	if (inode && atomic_dec_and_test(&inode->i_count)) {
		if (S_ISCHR(inode->i_mode))
			cd_forget(inode);
		/* A shmem file's pages go with its inode. */
		if (inode->i_data.nrpages)
			shmem_truncate_range(inode, 0, (uoff_t)-1);
		kfree(inode);
	}
}

void inode_set_bytes(struct inode *inode, loff_t bytes)
{
	inode->i_blocks = (blkcnt_t)(bytes >> 9);
	inode->i_bytes = (unsigned short)(bytes & 511);
}

extern struct kmem_cache *__filp_cache;
struct file *alloc_empty_file(int flags, const struct cred *cred);
void file_free(struct file *file);
void __fput(struct file *file);

/* Private provenance for compatibility inode/dentry allocations. */
#define FMODE_STELA_COMPAT ((__force fmode_t)(1U << 31))
static const struct file_operations empty_file_operations;

/* Takes over the caller's inode reference. */
static struct file *new_file(struct inode *inode, const struct file_operations *fops,
			     unsigned int flags, void *private_data)
{
	if (unlikely(!__filp_cache)) {
		iput(inode);
		return ERR_PTR(-ENODEV);
	}
	struct file *file = alloc_empty_file((int)flags, current_cred());

	if (IS_ERR(file)) {
		iput(inode);
		return file;
	}
	file->f_mode |= FMODE_STELA_COMPAT;
	file->f_inode = inode;
	file->f_mapping = inode->i_mapping;
	file->f_op = fops ? fops : &empty_file_operations;
	file->private_data = private_data;
	return file;
}

static void release(struct file *file)
{
	if (!(file->f_mode & FMODE_STELA_COMPAT)) {
		__fput(file);
		return;
	}

	if (file->f_op && file->f_op->release)
		file->f_op->release(file->f_inode, file);
	if (file->f_op && S_ISCHR(file->f_inode->i_mode)) {
		cdev_put(file->f_inode->i_cdev);
		fops_put(file->f_op);
	}
	iput(file->f_inode);

	struct dentry *dentry = file->f_path.dentry;

	if (dentry) {
		if (dentry->d_op && dentry->d_op->d_release)
			dentry->d_op->d_release(dentry);
		iput(dentry->d_inode);
		kfree(dentry);
		mntput(file->f_path.mnt);
	}
	file_free(file);
}

static LLIST_HEAD(release_pending);

static void release_later(struct work_struct *work)
{
	(void)work;
	struct llist_node *pending = llist_del_all(&release_pending);
	struct file *file, *next;

	llist_for_each_entry_safe(file, next, pending, f_llist)
		release(file);
}

static DECLARE_WORK(release_worker, release_later);

static void release_last_reference(struct file *file)
{
	if (!in_atomic() && !irqs_disabled()) {
		release(file);
		return;
	}

	/* Like Linux's delayed fput, the dead file carries its own queue node.
	 * The empty-to-nonempty producer schedules the drain, including when
	 * new files arrive while an earlier batch is being released. */
	if (llist_add(&file->f_llist, &release_pending))
		schedule_work(&release_worker);
}

void fput(struct file *file)
{
	if (file_ref_put(&file->f_ref))
		release_last_reference(file);
}

void fput_close(struct file *file)
{
	if (file_ref_put_close(&file->f_ref))
		release_last_reference(file);
}

void fput_close_sync(struct file *file)
{
	if (file_ref_put_close(&file->f_ref))
		release(file);
}

struct file *anon_inode_getfile(const char *name, const struct file_operations *fops, void *priv,
				int flags)
{
	(void)name;
	struct inode *inode = inode_of_mode(S_IRUSR | S_IWUSR);

	return IS_ERR(inode) ? ERR_CAST(inode) : new_file(inode, fops, (unsigned int)flags, priv);
}

/* The dentry holds the caller's inode reference, as Linux's does, and the
 * file its own. A failure leaves the caller's reference with the caller. */
struct file *alloc_file_pseudo(struct inode *inode, struct vfsmount *mount, const char *name,
			       int flags, const struct file_operations *fops)
{
	struct dentry *dentry = kzalloc_obj(*dentry);

	if (!dentry)
		return ERR_PTR(-ENOMEM);
	ihold(inode);

	struct file *file = new_file(inode, fops, (unsigned int)flags, NULL);

	if (IS_ERR(file)) {
		kfree(dentry);
		return file;
	}
	dentry->__d_name = (struct qstr)QSTR_INIT((const unsigned char *)name, (u32)strlen(name));
	dentry->d_inode = inode;
	dentry->d_sb = mount->mnt_sb;
	dentry->d_op = mount->mnt_sb->__s_d_op;
	file->__f_path = (struct path){.mnt = mntget(mount), .dentry = dentry};
	return file;
}

/* mm/shmem.c's files as Linux without swap keeps them: each page is
 * zero-filled when first asked for and stays in the file's page cache, held
 * by the cache, until a truncation or the inode's last reference drops it.
 * Nothing is written back, as there is no swap to write to. GEM objects of
 * system memory take their pages from here (i915), or only their size and
 * mapping offset (nouveau). */
static struct folio *shmem_folio(struct address_space *mapping, pgoff_t index, gfp_t gfp)
{
	struct folio *folio, *cached;

	rcu_read_lock();
	folio = xa_load(&mapping->i_pages, index);
	if (folio && !folio_try_get(folio))
		folio = NULL;
	rcu_read_unlock();
	if (folio)
		return folio;
	folio = folio_alloc(gfp | __GFP_ZERO, 0);
	if (!folio)
		return ERR_PTR(-ENOMEM);
	folio->mapping = mapping;
	folio->index = index;
	folio_mark_uptodate(folio);
	/* The cache's reference, beside the caller's. */
	folio_get(folio);
	/* The page cache's lock is taken with interrupts off (XA_FLAGS_LOCK_IRQ),
	 * and its nodes take the request's reclaim behaviour only. */
	xa_lock_irq(&mapping->i_pages);
	cached = __xa_cmpxchg(&mapping->i_pages, index, NULL, folio,
			      gfp & (__GFP_RECLAIM | __GFP_IO | __GFP_FS | __GFP_NOWARN |
				     __GFP_RETRY_MAYFAIL | __GFP_NORETRY));
	if (!cached)
		WRITE_ONCE(mapping->nrpages, mapping->nrpages + 1);
	xa_unlock_irq(&mapping->i_pages);
	if (!cached)
		return folio;
	folio->mapping = NULL;
	folio_put(folio);
	folio_put(folio);
	if (xa_is_err(cached))
		return ERR_PTR(xa_err(cached));
	/* Another caller cached the page first. */
	return shmem_folio(mapping, index, gfp);
}

/* mm/folio-compat.c's. */
void unlock_page(struct page *page)
{
	folio_unlock(page_folio(page));
}

/* Linux answers -EINVAL for a page past the end of the file. */
struct folio *shmem_read_folio_gfp(struct address_space *mapping, pgoff_t index, gfp_t gfp)
{
	if (index >= DIV_ROUND_UP(i_size_read(mapping->host), PAGE_SIZE))
		return ERR_PTR(-EINVAL);
	return shmem_folio(mapping, index, gfp);
}

struct page *shmem_read_mapping_page_gfp(struct address_space *mapping, pgoff_t index, gfp_t gfp)
{
	struct folio *folio = shmem_read_folio_gfp(mapping, index, gfp);

	return IS_ERR(folio) ? ERR_CAST(folio) : folio_file_page(folio, index);
}

/* Zeroes bytes [from, to) of a cached page, if it is cached. */
static void shmem_zero_part(struct address_space *mapping, pgoff_t index, size_t from, size_t to)
{
	struct folio *folio = xa_load(&mapping->i_pages, index);

	if (folio && from < to)
		folio_zero_range(folio, from, to - from);
}

/* Drops the cached pages of [start, end] (end inclusive) and zeroes the
 * parts of the pages at either end that the range covers. */
void shmem_truncate_range(struct inode *inode, loff_t start, uoff_t end)
{
	struct address_space *mapping = inode->i_mapping;
	uoff_t from = (uoff_t)start;
	pgoff_t first = (pgoff_t)DIV_ROUND_UP(from, PAGE_SIZE);
	pgoff_t last = end == (uoff_t)-1 ? ULONG_MAX : (pgoff_t)((end + 1) >> PAGE_SHIFT);
	struct folio *folio;
	unsigned long index;

	if (from & ~PAGE_MASK)
		shmem_zero_part(mapping, (pgoff_t)(from >> PAGE_SHIFT), from & ~PAGE_MASK,
				end >> PAGE_SHIFT == from >> PAGE_SHIFT ? (end & ~PAGE_MASK) + 1 :
									   PAGE_SIZE);
	if (end != (uoff_t)-1 && ((end + 1) & ~PAGE_MASK) && end >> PAGE_SHIFT != from >> PAGE_SHIFT)
		shmem_zero_part(mapping, (pgoff_t)(end >> PAGE_SHIFT), 0, (end & ~PAGE_MASK) + 1);
	if (last == 0 || first > last - 1)
		return;
	xa_for_each_range(&mapping->i_pages, index, folio, first, last - 1) {
		xa_erase_irq(&mapping->i_pages, index);
		WRITE_ONCE(mapping->nrpages, mapping->nrpages - 1);
		folio->mapping = NULL;
		folio_put(folio);
	}
}

/* Without swap, Linux's shmem_writeout marks the folio dirty again and
 * returns it still locked for activation. */
struct swap_iocb;
int shmem_writeout(struct folio *folio, struct swap_iocb **plug, struct list_head *folio_list)
{
	(void)plug;
	(void)folio_list;
	folio_mark_dirty(folio);
	return AOP_WRITEPAGE_ACTIVATE;
}

bool folio_clear_dirty_for_io(struct folio *folio)
{
	return folio_test_clear_dirty(folio);
}

/* A writeback pass over a shmem file: each dirty folio would only be marked
 * dirty again (shmem_writeout above), so the pass has nothing to give and
 * ends at once, leaving every page dirty and cached as Linux does. */
struct folio *writeback_iter(struct address_space *mapping, struct writeback_control *wbc,
			     struct folio *folio, int *error)
{
	(void)mapping;
	(void)wbc;
	(void)folio;
	(void)error;
	return NULL;
}

bool folio_redirty_for_writepage(struct writeback_control *wbc, struct folio *folio)
{
	wbc->pages_skipped += folio_nr_pages(folio);
	return folio_mark_dirty(folio);
}

/* A write into a shmem file's pages, extending the file past its end as
 * Linux's does. */
static ssize_t shmem_write_iter(struct kiocb *iocb, struct iov_iter *from)
{
	struct address_space *mapping = iocb->ki_filp->f_mapping;
	struct inode *inode = mapping->host;
	loff_t position = iocb->ki_pos;
	ssize_t written = 0;

	while (iov_iter_count(from)) {
		size_t offset = (size_t)position & ~PAGE_MASK;
		size_t bytes = min_t(size_t, PAGE_SIZE - offset, iov_iter_count(from));
		struct folio *folio = shmem_folio(mapping, (pgoff_t)(position >> PAGE_SHIFT),
						  mapping_gfp_mask(mapping));

		if (IS_ERR(folio)) {
			if (!written)
				written = PTR_ERR(folio);
			break;
		}
		size_t copied = copy_folio_from_iter(folio, offset, bytes, from);

		folio_mark_dirty(folio);
		folio_put(folio);
		if (!copied) {
			if (!written)
				written = -EFAULT;
			break;
		}
		position += (loff_t)copied;
		written += (ssize_t)copied;
	}
	if (position > i_size_read(inode))
		i_size_write(inode, position);
	iocb->ki_pos = position;
	return written;
}

static const struct file_operations shmem_file_operations = {
	.write_iter = shmem_write_iter,
};

/* As mm/shmem.c's: a dirty page is only marked so. */
static const struct address_space_operations shmem_aops = {
	.dirty_folio = noop_dirty_folio,
};

/* fs/read_write.c's, through the file's own write. */
ssize_t kernel_write(struct file *file, const void *buf, size_t count, loff_t *pos)
{
	struct kvec iov = {.iov_base = (void *)buf, .iov_len = count};
	struct kiocb kiocb;
	struct iov_iter iter;
	ssize_t written;

	if (!(file->f_mode & FMODE_WRITE))
		return -EBADF;
	if (!file->f_op->write_iter)
		return -EINVAL;
	init_sync_kiocb(&kiocb, file);
	kiocb.ki_pos = *pos;
	iov_iter_kvec(&iter, ITER_SOURCE, &iov, 1, count);
	written = file->f_op->write_iter(&kiocb, &iter);
	if (written > 0)
		*pos = kiocb.ki_pos;
	return written;
}

struct file *shmem_file_setup(const char *name, loff_t size, vma_flags_t flags)
{
	(void)name;
	(void)flags;
	struct inode *inode = inode_of_mode(S_IFREG | S_IRUSR | S_IWUSR);

	if (IS_ERR(inode))
		return ERR_CAST(inode);
	inode->i_size = size;
	inode->i_mapping->a_ops = &shmem_aops;
	mapping_set_gfp_mask(inode->i_mapping, GFP_HIGHUSER_MOVABLE);
	return new_file(inode, &shmem_file_operations, O_RDWR, NULL);
}

struct file *shmem_file_setup_with_mnt(struct vfsmount *mount, const char *name, loff_t size,
				       vma_flags_t flags)
{
	(void)mount;
	return shmem_file_setup(name, size, flags);
}

void stela_linux_fd_close(int fd)
{
	close_fd((unsigned int)fd);
}

int stela_linux_fd_dup(int fd)
{
	struct file *file = fget((unsigned int)fd);

	if (!file)
		return -EBADF;
	int copy = get_unused_fd_flags(0);

	if (copy < 0)
		fput(file);
	else
		fd_install((unsigned int)copy, file);
	return copy;
}

long stela_linux_fd_device(int fd)
{
	struct file *file = fget((unsigned int)fd);

	if (!file)
		return -EBADF;
	long device = file_inode(file)->i_rdev;

	fput(file);
	return device;
}

long long stela_linux_fd_seek(int fd, long long offset, int whence)
{
	struct file *file = fget((unsigned int)fd);
	long long result;

	if (!file)
		return -EBADF;
	if (file->f_op && file->f_op->llseek)
		result = file->f_op->llseek(file, offset, whence);
	else
		result = -ESPIPE;
	fput(file);
	return result;
}

static DECLARE_WAIT_QUEUE_HEAD(stela_linux_waiters);

void stela_linux_wait(const volatile unsigned int *word, unsigned int value, u64 nanoseconds)
{
	if (nanoseconds == U64_MAX)
		wait_event(stela_linux_waiters, READ_ONCE(*word) != value);
	else
		wait_event_timeout(stela_linux_waiters, READ_ONCE(*word) != value,
				   nsecs_to_jiffies64(nanoseconds) + 1);
}

void stela_linux_wake(void)
{
	wake_up_all(&stela_linux_waiters);
}

long stela_linux_ioctl(int fd, unsigned int request, void *argument)
{
	struct file *file = fget((unsigned int)fd);

	if (!file)
		return -EBADF;
	long result = file->f_op->unlocked_ioctl ?
			      file->f_op->unlocked_ioctl(file, request, (unsigned long)argument) :
			      -ENOTTY;

	fput(file);
	return result;
}

int stela_linux_chrdev_open(dev_t device, unsigned int flags, struct file **result)
{
	if (unlikely(!__filp_cache))
		return -ENXIO;
	struct inode *inode = inode_of_mode(S_IFCHR | S_IRUSR | S_IWUSR);
	if (IS_ERR(inode))
		return (int)PTR_ERR(inode);
	inode->i_rdev = device;
	struct file *file = new_file(inode, &def_chr_fops, flags, NULL);
	if (IS_ERR(file))
		return (int)PTR_ERR(file);
	file->f_mode |= FMODE_READ | FMODE_WRITE;
	int error = def_chr_fops.open(inode, file);
	if (error) {
		/* chrdev_open already dropped any cdev reference it acquired. */
		file->f_op = NULL;
		fput(file);
		return error;
	}
	*result = file;
	return 0;
}

int stela_linux_open(dev_t device, unsigned int flags)
{
	struct file *file;
	int error = stela_linux_chrdev_open(device, flags, &file);

	if (error)
		return error;
	int fd = get_unused_fd_flags(flags);

	if (fd < 0)
		fput(file);
	else
		fd_install((unsigned int)fd, file);
	return fd;
}

/* A new anonymous file under a new descriptor, as fs/anon_inodes.c. */
int anon_inode_getfd(const char *name, const struct file_operations *fops, void *priv, int flags)
{
	int fd = get_unused_fd_flags((unsigned int)flags);

	if (fd < 0)
		return fd;
	struct file *file = anon_inode_getfile(name, fops, priv, flags);

	if (IS_ERR(file)) {
		put_unused_fd((unsigned int)fd);
		return (int)PTR_ERR(file);
	}
	fd_install((unsigned int)fd, file);
	return fd;
}

/* A raw compatibility file has no VFS path through which to reopen it. */
struct file *stela_linux_reopen_unowned_path(void)
{
	stela_linux_unsupported("stela_linux_reopen_unowned_path");
	return ERR_PTR(-EOPNOTSUPP);
}

#ifdef STELA_LINUX_FILE_TEST
#include <linux/init_task.h>
#include <linux/radix-tree.h>
#include <linux/statfs.h>
#include <stela/page.h>
#include <stela/page_test.h>
#include <stela/test.h>

extern void __init chrdev_init(void);
extern void __init dcache_init(void);
extern void __init mnt_cache_init(void);

static unsigned int opens, releases;
static int open_error;

static int test_open(struct inode *inode, struct file *file)
{
	assert(imajor(inode) == 226 && iminor(inode) == 3 && file->f_mapping == &inode->i_data);
	opens++;
	return open_error;
}

static int test_release(struct inode *inode, struct file *file)
{
	(void)inode;
	(void)file;
	releases++;
	return 0;
}

static const struct file_operations fops = {.open = test_open, .release = test_release};

static unsigned int dentries;

static void test_d_release(struct dentry *dentry)
{
	assert(dentry->d_fsdata == &dentries && dentry->d_sb->s_magic == 0x7e57);
	dentries++;
}

static const struct dentry_operations dentry_ops = {.d_release = test_d_release};

struct pin_interleave {
	struct vfsmount *mount;
	struct vfsmount *winner;
	int count;
};
static struct pin_interleave *interleave;

static int test_init_fs_context(struct fs_context *context)
{
	struct pseudo_fs_context *pseudo = init_pseudo(context, 0x7e57);

	if (!pseudo)
		return -ENOMEM;
	pseudo->dops = &dentry_ops;
	/* Model a second pin publishing while the first is creating a mount. */
	if (interleave) {
		struct pin_interleave *overlap = interleave;

		interleave = NULL;
		assert(!simple_pin_fs(context->fs_type, &overlap->mount, &overlap->count));
		overlap->winner = overlap->mount;
	}
	return 0;
}

static struct file_system_type test_fs = {
	.name = "test",
	.init_fs_context = test_init_fs_context,
	.kill_sb = kill_anon_super,
};

/* Component-only binding for the original fdtable and resource limits. */
struct task_struct *__wrap_stela_linux_current(void);
struct task_struct *__wrap_stela_linux_current(void)
{
	return &init_task;
}

/* The fixture is single-threaded and has no RCU or workqueue workers. */
static struct rcu_head *retiring[32];
static rcu_callback_t retirement[ARRAY_SIZE(retiring)];
static unsigned int queued, grace_periods;

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
	grace_periods++;
}

bool __wrap_queue_work_on(int cpu, struct workqueue_struct *queue, struct work_struct *work)
{
	(void)cpu;
	(void)queue;
	work->func(work);
	return true;
}

static s64 test_now_ns(void *context)
{
	(void)context;
	return 0;
}

static bool test_realtime_at(void *context, s64 monotonic, u32 mode, u32 tick_ns,
			     s64 *seconds, u32 *nanoseconds)
{
	(void)context;
	(void)monotonic;
	(void)mode;
	(void)tick_ns;
	*seconds = 1700000000;
	*nanoseconds = 0;
	return true;
}

int main(void)
{
	static const struct stela_linux_services services = {
		.now_ns = test_now_ns, .realtime_at = test_realtime_at,
	};
	static const struct stela_linux_owner owner = {.services = &services};
	stela_linux_enter(&owner);

	/* The final put closes the reference before its owner can free it. */
	file_ref_t reference;
	file_ref_init(&reference, 1);
	assert(file_ref_get(&reference));
	assert(!file_ref_put(&reference));
	assert(file_ref_put(&reference));
	assert((unsigned long)atomic_long_read(&reference.refcnt) == FILE_REF_DEAD);
	assert(!file_ref_get(&reference));

	stela_test_register_pages();
	radix_tree_init();
	vfs_caches_init_early();
	dcache_init();
	inode_init();
	mnt_cache_init();
	files_init();
	chrdev_init();

	/* An unopened VFS file owns only its file allocation and credentials. */
	struct file *unopened = alloc_empty_file(O_RDONLY, current_cred());
	assert(!IS_ERR(unopened) && !unopened->f_inode && !unopened->f_path.dentry);
	fput(unopened);
	assert(!file_ref_get(&unopened->f_ref));

	/* Recycle the same type-stable slot without publishing stale fields. */
	struct file *first = anon_inode_getfile("first", NULL, &opens, O_RDWR);
	assert(!IS_ERR(first) && first->f_cred == current_cred());
	first->f_pos = 99;
	fput(first);
	assert(!file_ref_get(&first->f_ref));
	struct file *reused = anon_inode_getfile("reused", NULL, NULL, O_RDONLY);
	assert(reused == first && !reused->f_pos && !reused->private_data);
	get_file(reused);
	fput_close_sync(reused);
	assert(file_count(reused) == 1);
	fput_close_sync(reused);
	assert(!file_ref_get(&reused->f_ref));

	struct file *file;

	assert(stela_linux_chrdev_open(MKDEV(226, 3), 0, &file) == -ENXIO);
	assert(!__register_chrdev(226, 0, 8, "test", &fops));
	assert(stela_linux_chrdev_open(MKDEV(226, 8), 0, &file) == -ENXIO);
	open_error = -EPERM;
	assert(stela_linux_chrdev_open(MKDEV(226, 3), 0, &file) == -EPERM);
	assert(opens == 1 && !releases);
	open_error = 0;
	assert(!stela_linux_chrdev_open(MKDEV(226, 3), 0, &file));

	/* A descriptor holds its own reference; closing it drops that one. */
	int fd = get_unused_fd_flags(O_CLOEXEC);
	assert(fd == 0 && !fget(0) && get_unused_fd_flags(0) == 1);
	put_unused_fd(1);
	fd_install(0, get_file(file));
	assert(get_close_on_exec(0));
	struct file *looked_up = fget_task(&init_task, 0);
	assert(looked_up == file);
	fput(looked_up);
	struct fd borrowed = fdget(0);
	assert(fd_file(borrowed) == file);
	fdput(borrowed);
	assert(fd_empty(fdget(5)));
	stela_linux_fd_close(0);
	assert(!fget(0) && !releases);
	/* Cross the embedded table boundary and preserve the installed entry. */
	assert(get_unused_fd_flags(0) == 0);
	fd_install(0, get_file(file));
	for (unsigned int index = 1; index <= NR_OPEN_DEFAULT; index++)
		assert(get_unused_fd_flags(O_CLOEXEC) == (int)index);
	looked_up = fget_task(&init_task, 0);
	assert(looked_up == file && !get_close_on_exec(0));
	fput(looked_up);
	stela_linux_fd_close(0);
	fd_install(NR_OPEN_DEFAULT, get_file(file));
	assert(get_close_on_exec(NR_OPEN_DEFAULT));
	looked_up = fget_task(&init_task, NR_OPEN_DEFAULT);
	assert(looked_up == file);
	fput(looked_up);
	stela_linux_fd_close(NR_OPEN_DEFAULT);
	for (unsigned int index = 1; index < NR_OPEN_DEFAULT; index++)
		put_unused_fd(index);

	fput(file);
	assert(releases == 1);

	/* A client's descriptor, by what its system calls would do. */
	fd = stela_linux_open(MKDEV(226, 3), 0);
	assert(fd >= 0 && stela_linux_fd_device(fd) == MKDEV(226, 3));
	int copy = stela_linux_fd_dup(fd);
	assert(copy >= 0 && copy != fd);
	assert(stela_linux_ioctl(fd, 7, NULL) == -ENOTTY);
	stela_linux_fd_close(fd);
	assert(releases == 1);
	__unregister_chrdev(226, 0, 8, "test");
	assert(stela_linux_chrdev_open(MKDEV(226, 3), 0, &file) == -ENXIO);
	stela_linux_fd_close(copy);
	assert(releases == 2);
	assert(stela_linux_fd_dup(fd) == -EBADF && stela_linux_fd_device(fd) == -EBADF);
	assert(stela_linux_ioctl(fd, 7, NULL) == -EBADF);
	releases = 1;

	struct file *anonymous = anon_inode_getfile("test", &fops, &opens, O_RDWR);
	assert(!IS_ERR(anonymous) && anonymous->private_data == &opens);
	fput(anonymous);
	assert(releases == 2);

	struct file *object = shmem_file_setup("test", 8192, EMPTY_VMA_FLAGS);
	assert(!IS_ERR(object) && file_inode(object)->i_size == 8192);
	/* Its pages: zeroed when first read, the same page after, none past
	 * the end; writes land in them and extend the file. */
	struct address_space *pages = object->f_mapping;
	struct folio *leading = shmem_read_folio_gfp(pages, 0, GFP_KERNEL);
	assert(!IS_ERR(leading) && pages->nrpages == 1 && folio_ref_count(leading) == 2);
	assert(!memchr_inv(folio_address(leading), 0, PAGE_SIZE));
	assert(shmem_read_folio_gfp(pages, 0, GFP_KERNEL) == leading && folio_ref_count(leading) == 3);
	folio_put(leading);
	assert(PTR_ERR(shmem_read_folio_gfp(pages, 2, GFP_KERNEL)) == -EINVAL);
	loff_t position = PAGE_SIZE - 2;
	assert(kernel_write(object, "abcd", 4, &position) == 4 && position == PAGE_SIZE + 2);
	assert(pages->nrpages == 2 && !memcmp((char *)folio_address(leading) + PAGE_SIZE - 2, "ab", 2));
	struct page *second = shmem_read_mapping_page_gfp(pages, 1, GFP_KERNEL);
	assert(!IS_ERR(second) && !memcmp(page_address(second), "cd", 2));
	put_page(second);
	position = 3 * PAGE_SIZE;
	assert(kernel_write(object, "e", 1, &position) == 1);
	assert(file_inode(object)->i_size == 3 * PAGE_SIZE + 1 && pages->nrpages == 3);
	/* A truncation zeroes the partial pages it covers and drops the rest. */
	shmem_truncate_range(file_inode(object), PAGE_SIZE - 1, (uoff_t)-1);
	assert(pages->nrpages == 1 && folio_ref_count(leading) == 2);
	assert(!memcmp((char *)folio_address(leading) + PAGE_SIZE - 2, "a\0", 2));
	assert(shmem_writeout(leading, NULL, NULL) == AOP_WRITEPAGE_ACTIVATE && folio_test_dirty(leading));
	assert(folio_clear_dirty_for_io(leading) && !folio_test_dirty(leading));
	folio_put(leading);
	/* The last reference drops the cached page with the inode. */
	fput(object);

	struct pin_interleave overlap = {};
	interleave = &overlap;
	assert(!simple_pin_fs(&test_fs, &overlap.mount, &overlap.count));
	assert(!interleave && overlap.mount == overlap.winner && overlap.count == 2);
	simple_release_fs(&overlap.mount, &overlap.count);
	assert(overlap.mount == overlap.winner && overlap.count == 1);
	simple_release_fs(&overlap.mount, &overlap.count);
	assert(!overlap.mount && !overlap.count);

	struct vfsmount *mount = NULL;
	int count = 0;
	assert(!simple_pin_fs(&test_fs, &mount, &count) && mount && count == 1);
	struct vfsmount *pinned = mount;
	dev_t first_device = mount->mnt_sb->s_dev;
	assert(first_device && !MAJOR(first_device));
	struct vfsmount *other = kern_mount(&test_fs);
	assert(!IS_ERR(other) && other->mnt_sb->s_dev != first_device);
	assert(other->mnt_root && d_is_dir(other->mnt_root));
	kern_unmount(other);
	assert(grace_periods == 1);
	assert(!simple_pin_fs(&test_fs, &mount, &count) && mount == pinned && count == 2);
	simple_release_fs(&mount, &count);
	assert(mount == pinned);
	simple_release_fs(&mount, &count);
	assert(!mount && !count);

	/* A pseudo file's dentry is released after its file, with its inode. */
	mount = kern_mount(&test_fs);
	assert(!IS_ERR(mount) && mount->mnt_sb->s_dev == first_device);
	struct inode *inode = alloc_anon_inode(mount->mnt_sb);
	inode_set_bytes(inode, 1025);
	assert(inode->i_blocks == 2 && inode->i_bytes == 1);
	struct file *pseudo = alloc_file_pseudo(inode, mount, "dmabuf", O_RDWR, &fops);
	assert(!IS_ERR(pseudo) && pseudo->f_mode & FMODE_WRITE);
	struct kstatfs stats = {0};
	assert(!simple_statfs(pseudo->f_path.dentry, &stats));
	assert(stats.f_type == 0x7e57 && stats.f_bsize == PAGE_SIZE && stats.f_namelen == NAME_MAX);
	assert(pseudo->f_path.mnt == mount && !strcmp((const char *)pseudo->f_path.dentry->d_name.name, "dmabuf"));
	pseudo->f_path.dentry->d_fsdata = &dentries;
	kern_unmount(mount);
	assert(grace_periods == 2 && mount->mnt_sb->s_dev == first_device);
	fput(pseudo);
	assert(releases == 3 && dentries == 1);

	char name[8];
	assert(!strcmp(dynamic_dname(name, sizeof(name), "/%s", "ab"), "/ab"));
	assert(PTR_ERR(dynamic_dname(name, 3, "/%s", "ab")) == -ENAMETOOLONG);
	char long_name[96];
	char *formatted = dynamic_dname(long_name, sizeof(long_name), "%080u", 1U);
	assert(formatted == long_name + 15 && strlen(formatted) == 80 && formatted[79] == '1');
	for (unsigned int i = 0; i < queued; i++)
		retirement[i](retiring[i]);
	stela_linux_exit();
	printf("Linux files and descriptors passed\n");
	return 0;
}
#endif
