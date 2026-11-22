/*
 * The gate's BUNDLE report of a failed installation of the Metal plugin, in C
 * without a library, so the kext (install.cpp) and the host's tests
 * (tests/file_rewrite.c) run the same code: the step that failed, by the
 * gate's `BUNDLE_STEP_` values, and the kernel's errno.
 */
#pragma once

#include <stela/gate.h>

/* The gate's BUNDLE report of a failure at `step` with `error`. */
static inline unsigned long long
stela_bundle_failure(unsigned step, int error)
{
	return STELA_GATE_BUNDLE_FAILED | (unsigned long long)(step & 0xff) << STELA_GATE_BUNDLE_STEP_SHIFT |
	    (unsigned long long)(error & 0xffff) << STELA_GATE_BUNDLE_ERRNO_SHIFT;
}

/* What the kext was doing at `step`, for its log line. */
static inline const char *
stela_bundle_step_name(unsigned step)
{
	switch (step) {
	case STELA_GATE_BUNDLE_STEP_ALLOCATE:
		return "allocating for";
	case STELA_GATE_BUNDLE_STEP_PATH:
		return "spelling the path of";
	case STELA_GATE_BUNDLE_STEP_READ:
		return "reading";
	case STELA_GATE_BUNDLE_STEP_OPEN:
		return "opening";
	case STELA_GATE_BUNDLE_STEP_WRITE:
		return "writing";
	case STELA_GATE_BUNDLE_STEP_LENGTH:
		return "setting the length of";
	case STELA_GATE_BUNDLE_STEP_SYNC:
		return "flushing";
	case STELA_GATE_BUNDLE_STEP_CLOSE:
		return "closing";
	case STELA_GATE_BUNDLE_STEP_VERIFY:
		return "reading back";
	case STELA_GATE_BUNDLE_STEP_DIFFERS:
		return "checking";
	default:
		return "installing";
	}
}
