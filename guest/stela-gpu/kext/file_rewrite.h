/*
 * The steps of rewriting a file in place, in C without a library, so the
 * kext (files.cpp) and the host's tests (tests/file_rewrite.c) run the same
 * code. A failure names its step by the gate's `BUNDLE_STEP_` values.
 *
 * The kernel's vnode_open creates a file for O_CREAT but leaves an existing
 * one's length alone whatever O_TRUNC says: it is vn_open_auth alone, and
 * the open syscall (open1) is what sets the length to 0 after vn_open_auth
 * returns. So a rewrite sets the length itself, after the bytes: a shorter
 * file written over a longer one otherwise keeps the longer one's tail.
 *
 * The includer defines, over its file interface and before including this:
 *   stela_rewrite_open(file): opens or creates the file for writing
 *   stela_rewrite_write(file, bytes, offset, count, &left): writes `count`
 *     bytes at `offset`, `left` of them unwritten
 *   stela_rewrite_resize(file, length): sets the length
 *   stela_rewrite_sync(file): flushes the bytes and length to the disk
 *   stela_rewrite_close(file): closes what the open opened
 * each answering 0 or an errno, and STELA_REWRITE_SHORT, the errno a write
 * that left bytes unwritten without an error answers.
 */
#pragma once

#include <stela/gate.h>

/* The bytes written at a time. */
#define STELA_REWRITE_CHUNK (64 * 1024)

/*
 * Writes `length` bytes of `bytes` as the whole of `file`. On failure, answers
 * the errno and sets `step` to the step that failed; the file is then neither
 * the old content nor the new, and the next rewrite replaces it.
 */
static inline int
stela_rewrite(void *file, const unsigned char *bytes, unsigned long length, unsigned *step)
{
	*step = STELA_GATE_BUNDLE_STEP_OPEN;
	int error = stela_rewrite_open(file);
	if (error) {
		return error;
	}
	*step = STELA_GATE_BUNDLE_STEP_WRITE;
	for (unsigned long offset = 0; !error && offset < length;) {
		unsigned long rest = length - offset;
		int count = rest < STELA_REWRITE_CHUNK ? (int)rest : STELA_REWRITE_CHUNK;
		int left = 0;
		error = stela_rewrite_write(file, bytes + offset, offset, count, &left);
		if (!error && left != 0) {
			error = STELA_REWRITE_SHORT;
		}
		offset += (unsigned long)count;
	}
	if (!error) {
		*step = STELA_GATE_BUNDLE_STEP_LENGTH;
		error = stela_rewrite_resize(file, length);
	}
	if (!error) {
		*step = STELA_GATE_BUNDLE_STEP_SYNC;
		error = stela_rewrite_sync(file);
	}
	int closed = stela_rewrite_close(file);
	if (!error && closed) {
		*step = STELA_GATE_BUNDLE_STEP_CLOSE;
		error = closed;
	}
	return error;
}
