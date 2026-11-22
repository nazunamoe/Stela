/*
 * Host tests of file_rewrite.h and install_report.h, which the kext compiles
 * unchanged. xtask's `guest::app::tests::the_kext_c_code_passes_its_host_tests`
 * builds and runs this with the host's clang, with the gate.h the kext
 * builds against; it prints each failing check.
 *
 * The file model is the kernel's vnode KPI as the kext meets it: an open
 * creates an absent file and leaves an existing one's bytes and length, as
 * vnode_open does whatever O_TRUNC says, and only a resize cuts a tail.
 */
#include <errno.h>
#include <stdio.h>
#include <string.h>

#define CAPACITY (300 * 1024)

struct file {
	int exists;
	int open;
	unsigned long length;
	unsigned char bytes[CAPACITY];
	/* The errno each step answers, and the write call that fails. */
	int fail_open, fail_write, fail_resize, fail_sync, fail_close;
	int failing_write;
	/* A write that leaves bytes unwritten without an error. */
	int short_write;
	int writes, closes, syncs;
};

static int
stela_rewrite_open(void *opaque)
{
	struct file *file = opaque;
	if (file->fail_open) {
		return file->fail_open;
	}
	file->exists = 1;
	file->open = 1;
	return 0;
}

static int
stela_rewrite_write(void *opaque, const unsigned char *bytes, unsigned long offset, int count, int *left)
{
	struct file *file = opaque;
	if (!file->open || offset + (unsigned long)count > CAPACITY) {
		return EFAULT;
	}
	if (++file->writes == file->failing_write) {
		return file->fail_write;
	}
	if (file->short_write && file->writes == file->failing_write + 1) {
		count -= 1;
		*left = 1;
	}
	memcpy(file->bytes + offset, bytes, (size_t)count);
	if (offset + (unsigned long)count > file->length) {
		file->length = offset + (unsigned long)count;
	}
	return 0;
}

static int
stela_rewrite_resize(void *opaque, unsigned long length)
{
	struct file *file = opaque;
	if (file->fail_resize) {
		return file->fail_resize;
	}
	if (length > file->length) {
		memset(file->bytes + file->length, 0, length - file->length);
	}
	file->length = length;
	return 0;
}

static int
stela_rewrite_sync(void *opaque)
{
	struct file *file = opaque;
	file->syncs++;
	return file->fail_sync;
}

static int
stela_rewrite_close(void *opaque)
{
	struct file *file = opaque;
	file->open = 0;
	file->closes++;
	return file->fail_close;
}

#define STELA_REWRITE_SHORT EIO

#include "../file_rewrite.h"
#include "../install_report.h"

static int failures;

#define CHECK(condition)                                                       \
	do {                                                                   \
		if (!(condition)) {                                            \
			printf("%s:%d: %s\n", __FILE__, __LINE__, #condition); \
			failures++;                                            \
		}                                                              \
	} while (0)

static struct file file;
static unsigned char old[CAPACITY], new[CAPACITY];

/* A file that holds `length` bytes of `old`, with no failures planned. */
static void
existing(unsigned long length)
{
	memset(&file, 0, sizeof file);
	file.exists = 1;
	file.length = length;
	memcpy(file.bytes, old, length);
}

static int
holds(const unsigned char *bytes, unsigned long length)
{
	return file.length == length && !memcmp(file.bytes, bytes, length);
}

/*
 * A plugin written over one 16 bytes longer, as a stela.efi with an older
 * plugin met a newer one's: the longer one's tail stayed, so the file read
 * back 16 bytes long and the boot stopped.
 */
static void
shorter_over_longer(void)
{
	unsigned step = 0;
	existing(CAPACITY);
	CHECK(!stela_rewrite(&file, new, CAPACITY - 16, &step));
	CHECK(holds(new, CAPACITY - 16));
	CHECK(file.closes == 1 && file.syncs == 1 && !file.open);
	/* Every size against every other, around the chunk. */
	const unsigned long sizes[] = {1, STELA_REWRITE_CHUNK - 1, STELA_REWRITE_CHUNK, STELA_REWRITE_CHUNK + 1,
	    2 * STELA_REWRITE_CHUNK + 16, CAPACITY};
	for (unsigned i = 0; i < sizeof sizes / sizeof sizes[0]; i++) {
		for (unsigned j = 0; j < sizeof sizes / sizeof sizes[0]; j++) {
			existing(sizes[i]);
			CHECK(!stela_rewrite(&file, new, sizes[j], &step));
			CHECK(holds(new, sizes[j]));
		}
	}
}

static void
creates_an_absent_file(void)
{
	unsigned step = 0;
	memset(&file, 0, sizeof file);
	CHECK(!stela_rewrite(&file, new, 100, &step));
	CHECK(file.exists && holds(new, 100));
}

/* Each step's failure names that step and its errno, and closes once. */
static void
failures_name_their_step(void)
{
	unsigned step = 0;
	existing(1000);
	file.fail_open = ENOENT;
	CHECK(stela_rewrite(&file, new, 10, &step) == ENOENT && step == STELA_GATE_BUNDLE_STEP_OPEN);
	CHECK(file.closes == 0 && holds(old, 1000));

	existing(1000);
	file.failing_write = 2;
	file.fail_write = ENOSPC;
	CHECK(stela_rewrite(&file, new, 3 * STELA_REWRITE_CHUNK, &step) == ENOSPC);
	CHECK(step == STELA_GATE_BUNDLE_STEP_WRITE && file.writes == 2 && file.closes == 1 && !file.syncs);

	existing(1000);
	file.failing_write = 0;
	file.short_write = 1;
	CHECK(stela_rewrite(&file, new, 3 * STELA_REWRITE_CHUNK, &step) == STELA_REWRITE_SHORT);
	CHECK(step == STELA_GATE_BUNDLE_STEP_WRITE && file.writes == 1 && file.closes == 1);

	existing(1000);
	file.fail_resize = EPERM;
	CHECK(stela_rewrite(&file, new, 10, &step) == EPERM && step == STELA_GATE_BUNDLE_STEP_LENGTH);
	CHECK(file.closes == 1 && !file.syncs);

	existing(1000);
	file.fail_sync = EIO;
	CHECK(stela_rewrite(&file, new, 10, &step) == EIO && step == STELA_GATE_BUNDLE_STEP_SYNC);
	CHECK(file.closes == 1);

	existing(1000);
	file.fail_close = EIO;
	CHECK(stela_rewrite(&file, new, 10, &step) == EIO && step == STELA_GATE_BUNDLE_STEP_CLOSE);

	/* The first failure is the one named, not the close after it. */
	existing(1000);
	file.fail_resize = EROFS;
	file.fail_close = EIO;
	CHECK(stela_rewrite(&file, new, 10, &step) == EROFS && step == STELA_GATE_BUNDLE_STEP_LENGTH);
}

/* The report word is the one stela_contracts::gate::BundleReport reads. */
static void
reports(void)
{
	CHECK(stela_bundle_failure(STELA_GATE_BUNDLE_STEP_WRITE, ENOSPC) == 0x1c0504ULL);
	CHECK(stela_bundle_failure(STELA_GATE_BUNDLE_STEP_DIFFERS, 0) == 0xa04ULL);
	CHECK(stela_bundle_failure(STELA_GATE_BUNDLE_STEP_ALLOCATE, ENOMEM) == 0xc0104ULL);
	for (unsigned step = STELA_GATE_BUNDLE_STEP_ALLOCATE; step <= STELA_GATE_BUNDLE_STEP_DIFFERS; step++) {
		CHECK(strcmp(stela_bundle_step_name(step), "installing") != 0);
	}
	CHECK(!strcmp(stela_bundle_step_name(0), "installing"));
}

int
main(void)
{
	for (unsigned long i = 0; i < CAPACITY; i++) {
		old[i] = (unsigned char)(i * 7 + 1);
		new[i] = (unsigned char)(i * 13 + 5);
	}
	shorter_over_longer();
	creates_an_absent_file();
	failures_name_their_step();
	reports();
	if (failures) {
		return 1;
	}
	printf("ok\n");
	return 0;
}
