/*
 * The Metal plugin the kext carries (stela/bundle.h), installed over the one
 * in /Library/GPUBundles before the accelerator is published, so Metal only
 * ever loads the plugin whose code this boot's trust cache lists.
 *
 * The kernel lets a kext open, create and write files but not create
 * directories or rename. The kext rewrites a stale file in place, which keeps
 * its inode with its restricted flag and extended attributes, and creates a
 * missing one; a plugin whose directories are absent it reports instead.
 *
 * A failed rewrite leaves a file that is neither the old plugin nor the new,
 * which the boot then stops for, naming the step and errno. Keeping the old
 * bytes would not let this boot go on, since its trust cache lists only the
 * new plugin's code, and every later boot finds the file unlike its own and
 * rewrites it whole, so nothing a failure leaves outlasts the next boot.
 */
#include "StelaGPU.h"
#include "app_delivery.h"
#include "compression.h"
#include "files.h"
#include "install_report.h"

#include <IOKit/IOLib.h>
#include <kern/thread.h>
#include <stela/bundle.h>
#include <stela/gate.h>
#include <sys/errno.h>

/*
 * The plugin on the Data volume, which /Library is a firmlink to. Looking
 * there waits for the volume to mount, where /Library would find the system
 * volume's directory beneath it.
 */
static const char kPlugins[] = "/System/Volumes/Data/Library/GPUBundles";
static const char kPlugin[] = "/System/Volumes/Data/Library/GPUBundles/StelaGPU.bundle/";
/* How often the installer looks for the Data volume, in milliseconds. */
static const unsigned kMountPoll = 50;

/* Logs a failure at `step` of `path` with `error` and answers its report. */
static uint64_t
failed(unsigned step, const char *path, int error)
{
	if (step == STELA_GATE_BUNDLE_STEP_DIFFERS) {
		IOLog("StelaGPU: %s reads back unlike stela.efi's after it was written\n", path);
	} else {
		IOLog("StelaGPU: %s %s failed: %d\n", stela_bundle_step_name(step), path, error);
	}
	return stela_bundle_failure(step, error);
}

/* Installs each file that differs, answering the gate's BUNDLE report. */
static uint64_t
install(vfs_context_t context, char *buffer)
{
	uint64_t report = STELA_GATE_BUNDLE_CURRENT;
	for (const stela_bundle_file &file : stela_bundle_files) {
		char path[256];
		int written = snprintf(path, sizeof path, "%s%s", kPlugin, file.path);
		if (written < 0 || size_t(written) >= sizeof path) {
			return failed(STELA_GATE_BUNDLE_STEP_PATH, file.path, ENAMETOOLONG);
		}
		bool same = false;
		int error = stelaMatches(path, file.sha256, file.length, context, buffer, &same);
		if (same) {
			continue;
		}
		if (error && error != ENOENT) {
			return failed(STELA_GATE_BUNDLE_STEP_READ, path, error);
		}
		unsigned step = 0;
		error = stelaWriteFile(path, file.bytes, file.length, file.mode, context, &step);
		if (error == ENOENT && step == STELA_GATE_BUNDLE_STEP_OPEN) {
			IOLog("StelaGPU: %s cannot be created: its directory is absent\n", path);
			return STELA_GATE_BUNDLE_MISSING;
		}
		if (error) {
			return failed(step, path, error);
		}
		error = stelaMatches(path, file.sha256, file.length, context, buffer, &same);
		if (error) {
			return failed(STELA_GATE_BUNDLE_STEP_VERIFY, path, error);
		}
		if (!same) {
			return failed(STELA_GATE_BUNDLE_STEP_DIFFERS, path, 0);
		}
		IOLog("StelaGPU: installed %s from stela.efi\n", path);
		report = STELA_GATE_BUNDLE_UPDATED;
	}
	return report;
}

bool
StelaGPU::isStopping()
{
	IOLockLock(lock);
	bool stopped = stopping;
	IOLockUnlock(lock);
	return stopped;
}

void
StelaGPU::installPlugin(void *parameter, wait_result_t)
{
	auto gpu = static_cast<StelaGPU *>(parameter);
	uint64_t report = stela_bundle_failure(STELA_GATE_BUNDLE_STEP_ALLOCATE, ENOMEM);
	vfs_context_t context = vfs_context_create(nullptr);
	auto buffer = static_cast<char *>(IOMallocData(kStelaChunk));
	bool stopping = false;
	if (context && buffer) {
		vnode_t plugins = NULLVP;
		while (!(stopping = gpu->isStopping()) && vnode_lookup(kPlugins, 0, &plugins, context) != 0) {
			IOSleep(kMountPoll);
		}
		if (!stopping) {
			vnode_put(plugins);
			report = install(context, buffer);
		}
	} else {
		IOLog("StelaGPU: the plugin's installer cannot allocate\n");
	}
	IOLockLock(gpu->lock);
	stopping = gpu->stopping;
	if (!stopping) {
		gpu->write(STELA_GATE_BUNDLE, report);
	}
	IOLockUnlock(gpu->lock);
	if (!stopping && (report == STELA_GATE_BUNDLE_CURRENT || report == STELA_GATE_BUNDLE_UPDATED)) {
		gpu->registerService();
	}
	// After the accelerator, which nothing about the app delays. The Data
	// volume is mounted whenever the plugin's report was reached unstopped.
	if (!stopping && context && buffer) {
		gpu->setProperty(STELA_APP_PROPERTY, stelaDeliverApp(context, buffer));
		stelaTellNoCompression(context, buffer);
	}
	if (buffer) {
		IOFreeData(buffer, kStelaChunk);
	}
	if (context) {
		vfs_context_rele(context);
	}
	gpu->release();
}
