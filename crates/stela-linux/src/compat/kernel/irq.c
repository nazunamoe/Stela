// SPDX-License-Identifier: GPL-2.0-only
/* Linux's interrupt requests, with the interface of Linux
 * cc9de5d4b5ab6459f5774946aa3195ad51236fa8 kernel/irq/manage.c, under Linux's
 * own interrupt descriptors, chips, flow handlers and domains (kernel/irq
 * irqdesc.c, chip.c, handle.c, irqdomain.c, unchanged). There are two kinds
 * of line:
 *
 * An owner's line is one the owner routes (irq_enable, irq_disable), such as
 * a PCI function's. A platform ISR runs no Linux code: it raises the line on
 * the owner's service, which calls stela_linux_irq_run in process context,
 * and the handlers run in a hard-IRQ scope there. Owners number their lines
 * below STELA_LINUX_OWNER_LINES, where no domain allocates. Such a line has
 * no descriptor: its requests, routing and delivery are all in this file, as
 * before Linux's descriptors were built, and Linux code cannot raise it by
 * number.
 *
 * A mapped line is one a driver made in an IRQ domain, with the driver's chip
 * and flow handler. The driver raises it from its own handler
 * (generic_handle_domain_irq), and the flow handler reaches this file through
 * the one action it keeps on the descriptor.
 *
 * Threaded handlers run as work, which may sleep as an IRQ thread does. */
#include <linux/interrupt.h>
#include <linux/irq.h>
#include <linux/irqdesc.h>
#include <linux/mutex.h>
#include <linux/slab.h>
#include <linux/workqueue.h>
#include <irq/internals.h>
#include <stela/atomic_callback.h>
#include <stela/irq.h>
#include <stela/linux.h>

struct irq_line;
struct requested {
	struct list_head node;
	struct irqaction action;
	struct irq_line *line;
	struct work_struct thread;
	bool closing;
	bool thread_active;
	bool thread_joined;
};

static LIST_HEAD(requests);
/* Like the selected original irq_desc request mutex, this serializes only
 * one logical line across real sleeping action-thread joins. Global line
 * metadata remains with the one canonical C image for kernel lifetime. */
struct irq_line {
	struct list_head node;
	unsigned int irq;
	struct mutex request_mutex;
	bool oneshot_masked;
	/* disable_irq's depth: the line is routed only at zero. */
	unsigned int disabled;
	/* What a mapped line's flow handler runs: this file's handlers. */
	struct irqaction bottom;
};
static LIST_HEAD(lines);
static DEFINE_MUTEX(lines_mutex);
static void run_actions(unsigned int irq);

/* The descriptor of a line a driver mapped in a domain, or NULL for an
 * owner's line, which has none. */
static struct irq_desc *mapped_desc(unsigned int irq)
{
	return irq >= STELA_LINUX_OWNER_LINES ? irq_to_desc(irq) : NULL;
}

static struct irq_line *line_owner(unsigned int irq, bool create)
{
	struct irq_line *line;
	mutex_lock(&lines_mutex);
	list_for_each_entry(line, &lines, node)
		if (line->irq == irq) {
			mutex_unlock(&lines_mutex);
			return line;
		}
	line = create ? kzalloc_obj(*line) : NULL;
	if (line) {
		line->irq = irq;
		mutex_init(&line->request_mutex);
		list_add_tail(&line->node, &lines);
	}
	mutex_unlock(&lines_mutex);
	return line;
}

/* Linux's arch hook under freely allocated descriptors, which x86 uses to
 * keep them out of its GSI space: lines drivers map start above the owners'. */
unsigned int arch_dynirq_lower_bound(unsigned int from)
{
	return max(from, STELA_LINUX_OWNER_LINES);
}

static irqreturn_t bottom_handler(int irq, void *line)
{
	(void)line;
	run_actions((unsigned int)irq);
	return IRQ_HANDLED;
}

/* manage.c's __setup_irq and __free_irq for the one action of a mapped line. */
static int attach_mapped(struct irq_line *line, struct irq_desc *desc, const char *name)
{
	struct irq_data *data = irq_desc_get_irq_data(desc);
	struct irq_chip *chip = data->chip;

	if (!chip || chip == &no_irq_chip)
		return -ENOSYS;
	if (chip->irq_request_resources) {
		int error = chip->irq_request_resources(data);

		if (error)
			return error;
	}
	line->bottom = (struct irqaction){
		.handler = bottom_handler,
		.dev_id = line,
		.irq = line->irq,
		.name = name,
	};
	guard(raw_spinlock_irqsave)(&desc->lock);
	int error = irq_activate(desc);

	if (error) {
		if (chip->irq_release_resources)
			chip->irq_release_resources(data);
		return error;
	}
	desc->action = &line->bottom;
	return 0;
}

static void detach_mapped(struct irq_desc *desc)
{
	struct irq_data *data = irq_desc_get_irq_data(desc);

	scoped_guard(raw_spinlock_irqsave, &desc->lock) {
		irq_shutdown(desc);
		desc->action = NULL;
	}
	if (data->chip->irq_release_resources)
		data->chip->irq_release_resources(data);
}

static int enable_line(unsigned int irq, bool *unexposed)
{
	const struct stela_linux_owner *owner = stela_linux_owner();
	struct irq_desc *desc = mapped_desc(irq);

	*unexposed = false;
	if (desc) {
		guard(raw_spinlock_irqsave)(&desc->lock);
		/* A chip that refuses to start has delivered nothing. */
		int error = irq_startup(desc, IRQ_RESEND, IRQ_START_FORCE);

		*unexposed = error != 0;
		return error;
	}
	if (!owner->services->irq_enable) {
		/* No native callback was invoked or route published. */
		*unexposed = true;
		stela_linux_unsupported("interrupt lines");
		return -ENOSYS;
	}
	return owner->services->irq_enable(owner->context, irq, unexposed);
}

static void disable_line(unsigned int irq)
{
	const struct stela_linux_owner *owner = stela_linux_owner();
	struct irq_desc *desc = mapped_desc(irq);

	if (desc) {
		guard(raw_spinlock_irqsave)(&desc->lock);
		irq_disable(desc);
		return;
	}
	if (!owner->services->irq_disable ||
	    owner->services->irq_disable(owner->context, irq))
		stela_linux_fatal("IRQ source mask unavailable; request retained");
}

static void retire_line(unsigned int irq)
{
	const struct stela_linux_owner *owner = stela_linux_owner();
	struct irq_desc *desc = mapped_desc(irq);

	if (desc) {
		detach_mapped(desc);
		return;
	}
	if (!owner->services->irq_retire ||
	    owner->services->irq_retire(owner->context, irq))
		/* Existing no-return fatal/trap boundary: no normal C return,
		 * request unlink/free or caller dev_id reclamation before the trap.
		 * Trap disposition itself is not task cancellation/join evidence. */
		stela_linux_fatal("IRQ action retirement unavailable; request retained");
}

/* Checked by the exact native service epoch join, after free_irq canceled
 * each closing action's real work. A mask/root fence alone never sets this. */
bool stela_linux_irq_actions_joined(unsigned int irq)
{
	struct requested *request;
	bool found = false;
	list_for_each_entry(request, &requests, node)
		if (request->action.irq == irq) {
			found = true;
			if (!request->closing || !request->thread_joined)
				return false;
		}
	return found;
}

/* Every outstanding shared ONESHOT action, including one being canceled,
 * keeps the physical line masked. A closing thread ceases to count only after
 * its real callback returns or cancel_work_sync actually joins/removes it. */
static void maybe_rearm(struct irq_line *line)
{
	struct requested *request;
	bool live = false;
	if (!READ_ONCE(line->oneshot_masked))
		return;
	list_for_each_entry(request, &requests, node) {
		if (request->action.irq != line->irq)
			continue;
		if (!READ_ONCE(request->closing))
			live = true;
		if ((request->action.flags & IRQF_ONESHOT) &&
			READ_ONCE(request->thread_active))
			return;
	}
	if (live) {
		bool unexposed = false;
		/* A line disable_irq holds stays unrouted until enable_irq. */
		if (!line->disabled && enable_line(line->irq, &unexposed))
			stela_linux_fatal("IRQ oneshot rearm unavailable; action retained");
		WRITE_ONCE(line->oneshot_masked, false);
	}
}

/* A threaded action's handler; a oneshot line stays masked until it ends. */
static void run_thread(struct work_struct *work)
{
	struct requested *request = container_of(work, struct requested, thread);
	struct irqaction *action = &request->action;

	action->thread_fn((int)action->irq, action->dev_id);
	WRITE_ONCE(request->thread_active, false);
	if (action->flags & IRQF_ONESHOT)
		maybe_rearm(request->line);
}

/* The primary handler of an action with only a threaded one. */
static irqreturn_t wake_thread(int irq, void *dev_id)
{
	(void)irq;
	(void)dev_id;
	return IRQ_WAKE_THREAD;
}

static int request_irq_locked(struct irq_line *line, unsigned int irq, irq_handler_t handler, irq_handler_t thread_fn,
			 unsigned long flags, const char *name, void *dev)
{
	if (in_interrupt())
		return -EINVAL;
	if (irq == IRQ_NOTCONNECTED)
		return -ENOTCONN;
	if ((flags & IRQF_SHARED) && !dev)
		return -EINVAL;
	if (!handler && !thread_fn)
		return -EINVAL;
	struct requested *other;
	bool first = true;
	list_for_each_entry(other, &requests, node) {
		if (other->action.irq != irq)
			continue;
		if (!(flags & other->action.flags & IRQF_SHARED))
			return -EBUSY;
		first = false;
	}
	struct requested *request = kzalloc_obj(*request);
	if (!request)
		return -ENOMEM;
	request->line = line;
	request->action = (struct irqaction){
		.handler = handler ? handler : wake_thread,
		.thread_fn = thread_fn,
		.dev_id = dev,
		.irq = irq,
		.flags = (unsigned int)flags,
		.name = name,
	};
	INIT_WORK(&request->thread, run_thread);
	/* Publish the fully initialized action before enable can deliver a
	 * synchronous notification or yield to this owner's real service task. */
	struct irq_desc *mapped = first ? mapped_desc(irq) : NULL;
	if (mapped) {
		int error = attach_mapped(line, mapped, name);

		if (error) {
			kfree(request);
			return error;
		}
	}
	list_add_tail(&request->node, &requests);
	/* As manage.c: a line requested without enabling starts one disable
	 * deep, and enable_irq routes it. */
	if (first && (flags & IRQF_NO_AUTOEN))
		line->disabled++;
	if (first && !line->disabled) {
		bool unexposed = false;
		int error = enable_line(irq, &unexposed);
		if (error) {
			if (!unexposed)
				stela_linux_fatal("IRQ enable uncertain; published request retained");
			/* Only the exact unused native owners/service epoch proof permits
			 * returning an error that lets the caller reclaim dev_id. */
			list_del(&request->node);
			kfree(request);
			if (mapped)
				detach_mapped(mapped);
			return error;
		}
	}
	return 0;
}

int request_threaded_irq(unsigned int irq, irq_handler_t handler, irq_handler_t thread_fn,
						unsigned long flags, const char *name, void *dev)
{
	if (in_interrupt())
		return -EINVAL;
	/* A number above the owners' is a line only while a driver has it
	 * mapped; without a descriptor Linux has no such interrupt. */
	if (irq >= STELA_LINUX_OWNER_LINES && !mapped_desc(irq))
		return -EINVAL;
	struct irq_line *line = line_owner(irq, true);
	if (!line)
		return -ENOMEM;
	mutex_lock(&line->request_mutex);
	int result = request_irq_locked(line, irq, handler, thread_fn, flags, name, dev);
	mutex_unlock(&line->request_mutex);
	return result;
}

static const void *free_irq_locked(unsigned int irq, void *dev_id)
{
	struct requested *request;
	/* Primary callbacks are atomic, and joining one's own IRQ work would
	 * deadlock. Never turn either invalid caller into a completed free. */
	if (in_interrupt())
		stela_linux_fatal("free_irq in interrupt context");
	list_for_each_entry(request, &requests, node) {
		if (request->action.irq != irq || request->action.dev_id != dev_id)
			continue;
		if (current_work() == &request->thread)
			stela_linux_fatal("free_irq from its own IRQ thread");
		const char *name = request->action.name;
		bool last = true;
		struct requested *other;
		list_for_each_entry(other, &requests, node)
			if (other != request && other->action.irq == irq)
				last = false;
		/* Canonical single-owner service CPU serializes primary callbacks.
		 * Keep the node/dev_id through real thread and native epoch joins. */
		WRITE_ONCE(request->closing, true);
		if (last)
			disable_line(irq);
		cancel_work_sync(&request->thread);
		WRITE_ONCE(request->thread_active, false);
		WRITE_ONCE(request->thread_joined, true);
		if (last)
			retire_line(irq);
		struct irq_line *line = request->line;
		list_del(&request->node);
		kfree(request);
		if (last) {
			WRITE_ONCE(line->oneshot_masked, false);
			/* The next request starts from manage.c's fresh depth. */
			line->disabled = 0;
		} else {
			maybe_rearm(line);
		}
		return name;
	}
	WARN(1, "Trying to free already-free IRQ %u\n", irq);
	return NULL;
}

const void *free_irq(unsigned int irq, void *dev_id)
{
	struct requested *request;
	if (in_interrupt())
		stela_linux_fatal("free_irq in interrupt context");
	/* Check self-join before acquiring a mutex another free_irq may hold
	 * while cancel_work_sync waits for this very IRQ thread. */
	list_for_each_entry(request, &requests, node)
		if (request->action.irq == irq && request->action.dev_id == dev_id &&
			current_work() == &request->thread)
			stela_linux_fatal("free_irq from its own IRQ thread");
	struct irq_line *line = line_owner(irq, false);
	if (!line) {
		WARN(1, "Trying to free already-free IRQ %u\n", irq);
		return NULL;
	}
	mutex_lock(&line->request_mutex);
	const void *name = free_irq_locked(irq, dev_id);
	mutex_unlock(&line->request_mutex);
	return name;
}

/* What a driver whose probe failed left requested on one of its function's
 * lines: each request is freed as free_irq frees it, so the line is masked,
 * its threads joined and its route retired before the function goes to the
 * next driver or stays unbound. The handler and dev_id may belong to code
 * that is gone; free_irq calls neither. */
unsigned int stela_linux_irq_free_line(unsigned int irq)
{
	unsigned int freed = 0;

	for (;;) {
		struct requested *request;
		void *dev_id = NULL;
		bool found = false;

		list_for_each_entry(request, &requests, node)
			if (request->action.irq == irq && !request->closing) {
				dev_id = request->action.dev_id;
				found = true;
				break;
			}
		if (!found)
			return freed;
		free_irq(irq, dev_id);
		freed++;
	}
}

/* A line's primary handlers run to completion in the one context Linux code
 * runs in, so none is in progress in a caller that may sleep. What may still
 * run is an action's thread, which is work: wait for each, as Linux waits
 * for the line's active threads. */
void synchronize_irq(unsigned int irq)
{
	struct irq_line *line = line_owner(irq, false);
	struct requested *request;

	if (in_interrupt())
		stela_linux_fatal("synchronize_irq in interrupt context");
	if (!line)
		return;
	mutex_lock(&line->request_mutex);
	list_for_each_entry(request, &requests, node) {
		if (request->action.irq != irq)
			continue;
		if (current_work() == &request->thread)
			stela_linux_fatal("synchronize_irq from the line's own IRQ thread");
		flush_work(&request->thread);
	}
	mutex_unlock(&line->request_mutex);
}

/* Linux code here runs on one owner at a time and a line's primary handlers
 * run in that owner's hard-IRQ scope, so none is in flight while the caller
 * runs; what is left is whether a threaded handler of the line is active,
 * which Linux answers with false. */
bool synchronize_hardirq(unsigned int irq)
{
	struct irq_line *line = line_owner(irq, false);
	struct requested *request;
	bool idle = true;

	if (!line)
		return true;
	mutex_lock(&line->request_mutex);
	list_for_each_entry(request, &requests, node)
		if (request->line == line && READ_ONCE(request->thread_active))
			idle = false;
	mutex_unlock(&line->request_mutex);
	return idle;
}

/* Run a line's primary handlers. The caller is in a hard-IRQ scope. */
static void run_actions(unsigned int irq)
{
	struct requested *request;

	list_for_each_entry(request, &requests, node) {
		struct irqaction *action = &request->action;
		if (action->irq != irq || READ_ONCE(request->closing) ||
		    ((action->flags & IRQF_ONESHOT) && READ_ONCE(request->thread_active)) ||
		    !(action->handler((int)irq, action->dev_id) & IRQ_WAKE_THREAD))
			continue;
		if (WARN_ON_ONCE(!action->thread_fn))
			continue;
		if (action->flags & IRQF_ONESHOT) {
			disable_line(irq);
			WRITE_ONCE(request->line->oneshot_masked, true);
		}
		WRITE_ONCE(request->thread_active, true);
		queue_work(system_highpri_wq, &request->thread);
	}
}

void stela_linux_irq_run(unsigned int irq)
{
	struct stela_linux_atomic_callback scope;

	stela_linux_atomic_callback_begin(&scope);
	run_actions(irq);
	stela_linux_atomic_callback_end(&scope);
}

/* manage.c's disable depth. A held line is not routed: an owner's by its
 * owner, a mapped one by its chip. */
void disable_irq(unsigned int irq)
{
	struct irq_line *line = line_owner(irq, false);
	struct requested *request;
	bool requested = false;

	if (in_interrupt())
		stela_linux_fatal("disable_irq in interrupt context");
	if (!line)
		return;
	mutex_lock(&line->request_mutex);
	list_for_each_entry(request, &requests, node)
		requested |= request->action.irq == irq && !request->closing;
	if (!line->disabled++ && requested && !READ_ONCE(line->oneshot_masked))
		disable_line(irq);
	mutex_unlock(&line->request_mutex);
	synchronize_irq(irq);
}

void enable_irq(unsigned int irq)
{
	struct irq_line *line = line_owner(irq, false);
	struct requested *request;
	bool requested = false;

	if (!line)
		return;
	mutex_lock(&line->request_mutex);
	list_for_each_entry(request, &requests, node)
		requested |= request->action.irq == irq && !request->closing;
	if (!line->disabled) {
		WARN(1, "Unbalanced enable for IRQ %u\n", irq);
	} else if (!--line->disabled && requested && !READ_ONCE(line->oneshot_masked)) {
		bool unexposed = false;

		if (enable_line(irq, &unexposed))
			stela_linux_fatal("IRQ enable after disable_irq unavailable; action retained");
	}
	mutex_unlock(&line->request_mutex);
}

/* Adapted from manage.c: set a line's trigger through its chip, masked
 * while it changes when the chip asks for that. */
int __irq_set_trigger(struct irq_desc *desc, unsigned long flags)
{
	struct irq_chip *chip = desc->irq_data.chip;
	int ret, unmask = 0;

	if (!chip || !chip->irq_set_type) {
		/* IRQF_TRIGGER_* but the PIC does not support multiple flow-types? */
		pr_debug("No set_type function for IRQ %d (%s)\n", irq_desc_get_irq(desc),
			 chip ? (chip->name ? : "unknown") : "unknown");
		return 0;
	}
	if (chip->flags & IRQCHIP_SET_TYPE_MASKED) {
		if (!irqd_irq_masked(&desc->irq_data))
			mask_irq(desc);
		if (!irqd_irq_disabled(&desc->irq_data))
			unmask = 1;
	}
	/* Mask all flags except trigger mode */
	flags &= IRQ_TYPE_SENSE_MASK;
	ret = chip->irq_set_type(&desc->irq_data, (unsigned int)flags);
	switch (ret) {
	case IRQ_SET_MASK_OK:
	case IRQ_SET_MASK_OK_DONE:
		irqd_clear(&desc->irq_data, IRQD_TRIGGER_MASK);
		irqd_set(&desc->irq_data, (unsigned int)flags);
		fallthrough;
	case IRQ_SET_MASK_OK_NOCOPY:
		flags = irqd_get_trigger_type(&desc->irq_data);
		irq_settings_set_trigger_mask(desc, (u32)flags);
		irqd_clear(&desc->irq_data, IRQD_LEVEL);
		irq_settings_clr_level(desc);
		if (flags & IRQ_TYPE_LEVEL_MASK) {
			irq_settings_set_level(desc);
			irqd_set(&desc->irq_data, IRQD_LEVEL);
		}
		ret = 0;
		break;
	default:
		pr_err("Setting trigger mode %lu for irq %u failed (%pS)\n", flags,
		       irq_desc_get_irq(desc), chip->irq_set_type);
	}
	if (unmask)
		unmask_irq(desc);
	return ret;
}

/* manage.c: a nested handler's thread ended; wake synchronize_irq's waiters. */
void wake_threads_waitq(struct irq_desc *desc)
{
	if (atomic_dec_and_test(&desc->threads_active))
		wake_up(&desc->wait_for_threads);
}
