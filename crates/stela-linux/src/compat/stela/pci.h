/* SPDX-License-Identifier: GPL-2.0-or-later */
#ifndef STELA_PCI_H
#define STELA_PCI_H
#include <linux/pci.h>

/* A function its owner built, on a bus with the owner's pci_ops. Publish an
 * already initialized/enrolled device; removal drops its registration ref. */
int stela_linux_pci_publish_device(struct pci_dev *dev);
/* Publish an initialized device for other drivers' searches only: no driver
 * is offered it, now or when one registers later. */
int stela_linux_pci_publish_for_searches(struct pci_dev *dev);
bool stela_linux_pci_devices_present(void);
/* Checked original standard capability walker. PCIBIOS failure is distinct
 * from a successful absent capability (*found == 0). No context ownership moves. */
int stela_linux_pci_find_capability(struct pci_dev *dev, int cap, u8 *found);
bool stela_linux_pci_bound(const struct pci_dev *device);
const char *stela_linux_pci_driver_name(const struct pci_dev *device);
/* Whether the owner of an attached function routes its MSI-X table, and
 * how many vectors it has lines for (0 for none or no table). */
bool stela_linux_pci_routes_msix(const struct pci_dev *device);
int stela_linux_pci_msix_lines(struct pci_dev *device);
void stela_linux_pci_shutdown(struct pci_dev *dev);
void stela_linux_pci_remove_device(struct pci_dev *dev);
/* After `driver`'s probe of an attached function failed and its managed
 * resources were released: free the interrupt requests left on its lines,
 * turn its MSI or MSI-X and its bus mastering off, each said once. Nothing
 * for a function no owner attached. */
void stela_linux_pci_quiesce_failed(struct pci_dev *dev, const char *driver);
/* The same after `driver`'s `after` (its failed probe, its unload). */
void stela_linux_pci_quiesce(struct pci_dev *dev, const char *driver, const char *after);
/* Unload the driver bound to the function while Linux runs on, as
 * device_release_driver does, and stop what its remove left
 * (stela_linux_pci_quiesce); the function stays published. Whether a driver
 * held it. */
bool stela_linux_pci_release(struct pci_dev *dev);
/* The function's driver ends the picture the firmware left on its outputs:
 * it resets the function, or starts its display engine, which blanks what
 * the firmware lit. `why` names that step and outlives the boot (a string
 * literal); the first is kept. The host gives the firmware's framebuffer
 * back after a failed probe only when no step ended its picture. */
void stela_linux_pci_screen_ended(struct pci_dev *dev, const char *why);
/* The step that ended the firmware's picture on the function at bus and
 * devfn, or NULL. Read on any processor, at a stop too. */
const char *stela_linux_pci_screen_ended_at(unsigned int bus, unsigned int devfn);

/* A placed memory region of a function. */
struct stela_linux_pci_region {
	u64 start, size;
	unsigned long flags;
};

/* A port above a function, as its owner read it when it enumerated the
 * buses: its address and the first 256 bytes of its configuration, which
 * hold its PCI Express capability and so its link's speeds and width. Linux
 * code is shown it read-only (stela_linux_pci_upstream); the firmware
 * configured it and nothing here changes it. */
struct stela_linux_pci_port {
	unsigned int bus, devfn;
	u32 config[64];
};

_Static_assert(sizeof(struct stela_linux_pci_port) == 264, "Linux PCI port ABI");

/* A physical function its owner enumerated. Configuration accesses return a
 * PCIBIOS result; dma_map returns DMA_MAPPING_ERROR when it cannot map, and
 * dma_limit is the highest address it hands out. Rust retains exact typed
 * access causes in the actual State; a possibly applied failed write traps
 * before returning to an original caller that could ignore a PCIBIOS failure.
 * Real hardware all-ones with a successful access remains success/data. */
struct stela_linux_pci_function {
	void *context;
	int (*read_config)(void *context, int where, int size, u32 *value);
	int (*write_config)(void *context, int where, int size, u32 value);
	dma_addr_t (*dma_map)(void *context, phys_addr_t physical, size_t size,
			      enum dma_data_direction direction, bool pageless);
	void (*dma_unmap)(void *context, dma_addr_t address, size_t size,
			  enum dma_data_direction direction);
	/* Move the function's interrupt from its line to a message and back. The
	 * irq stays the name Linux knows it by; enable returns a negative errno. */
	int (*enable_msi)(void *context);
	void (*disable_msi)(void *context);
	u64 dma_limit;
	unsigned int bus, devfn, irq;
	struct stela_linux_pci_region regions[PCI_STD_NUM_BARS];
	/* Optional ownership transfer: successful attach keeps context through
	 * the final device reference, including references beyond removal.
	 * Called exactly once at quiescent process release in the attach owner.
	 * Nonquiescent or other-owner final release quarantines host/context;
	 * no destructor is invoked from atomic or private RCU callback context. */
	void (*release_context)(void *context);
	/* Original initialized device enrollment; called before any driver probe. */
	int (*enroll)(void *context, struct device *device);
	/* The function's platform ROM, a video BIOS image firmware publishes
	 * apart from the function (as x86 Linux takes the one EFI hands over),
	 * which the attach copies into pci_dev::rom: rom_length 0 for none. */
	const void *rom;
	size_t rom_length;
	/* The expansion ROM BAR where the owner placed it, decoding off, which
	 * the attach keeps as PCI_ROM_RESOURCE: size 0 for none. */
	struct stela_linux_pci_region rom_bar;
	/* The ports between the function and its root complex, nearest first,
	 * which the attach copies: none for a function on a root bus. */
	const struct stela_linux_pci_port *upstream;
	size_t upstream_count;
	/* Optional MSI-X, NULL where the owner routes none: set up to `vectors`
	 * entries of the function's MSI-X table, each masked until its line is
	 * enabled, on consecutive lines from *first_irq. Returns how many it set
	 * up (fewer where its lines run out) or a negative errno. Called again
	 * while enabled, it writes the same entries back, as after a reset.
	 * `vectors` 0 sets up nothing and returns how many it could. */
	int (*enable_msix)(void *context, unsigned int vectors, unsigned int *first_irq);
	void (*disable_msix)(void *context);
};

_Static_assert(sizeof(struct stela_linux_pci_function) == 312, "Linux PCI function ABI");
_Static_assert(__builtin_offsetof(struct stela_linux_pci_function, upstream) == 280,
	       "Linux PCI upstream port ABI");
_Static_assert(__builtin_offsetof(struct stela_linux_pci_function, enable_msix) == 296,
	       "Linux PCI MSI-X ABI");
_Static_assert(__builtin_offsetof(struct stela_linux_pci_function, rom_bar) == 256,
	       "Linux PCI ROM BAR ABI");
_Static_assert(__builtin_offsetof(struct stela_linux_pci_function, release_context) == 224,
	       "Linux PCI context release ABI");

_Static_assert(__builtin_offsetof(struct stela_linux_pci_function, enroll) == 232,
	       "Linux PCI enrollment ABI");

/* Present function to Linux, which binds any driver matching it. NULL reports
 * a negative errno (if error is non-NULL), without taking context ownership. Removal unregisters the device
 * and drops the registration reference; other references keep callbacks live.
 * The attach owner/platform and callback code must outlive every escaped
 * device reference or quarantined final release. */
struct pci_dev *stela_linux_pci_attach(const struct stela_linux_pci_function *function, int *error);
/* What identifies a function its owner keeps to itself: enough for another
 * function's driver to find it by address, class or ID and read who it is. */
struct stela_linux_pci_identity {
	unsigned int bus, devfn;
	u16 vendor, device, subsystem_vendor, subsystem_device;
	/* Configuration register 8: the class in the upper 24 bits. */
	u32 class_revision;
};

_Static_assert(sizeof(struct stela_linux_pci_identity) == 20, "Linux PCI identity ABI");

/* Present a function to searches only (pci_get_class, pci_dev_present,
 * pci_get_domain_bus_and_slot). No driver binds it, it has no resources,
 * interrupt or DMA, its configuration reads fail as for an absent function and
 * a write is refused and reported once. NULL reports a negative errno.
 * stela_linux_pci_remove_device drops the registration reference. */
struct pci_dev *stela_linux_pci_publish_identity(const struct stela_linux_pci_identity *identity,
						 int *error);
/* As stela_linux_pci_publish_identity, with a copy of the first length bytes
 * (at most 256) of the function's configuration as its owner read them:
 * reads inside the copy answer its bytes, others fail as for an absent
 * function, and a write is refused and reported once. */
struct pci_dev *stela_linux_pci_publish_snapshot(const struct stela_linux_pci_identity *identity,
						 const u8 *config, size_t length, int *error);
/* Cumulative final releases quarantined outside the original process owner.
 * Atomic observation only; no reset or deferred reclamation is implied. */
u64 stela_linux_pci_quarantined_functions(void);
/* An attached function's platform ROM copy, which ioremap answers with its
 * kernel address when [start, start + size) lies in it, or NULL; and whether
 * an address iounmap is given is one of those answers. */
void __iomem *stela_linux_pci_rom_map(phys_addr_t start, size_t size);
bool stela_linux_pci_rom_mapped(const volatile void __iomem *address);
/* Cache what Linux's probe reads from a PCI Express function's capability:
 * its flags, device capabilities and supported link speeds. A PCIBIOS error
 * when the function does not answer. */
int stela_linux_pci_express_probe(struct pci_dev *dev);
/* The port above dev, an attached function or a port this returned: Linux's
 * pci_upstream_bridge where a bus has a bridge, else the ports the owner
 * gave the function. NULL above the last one. Linux's own PCI tree here has
 * no bridges, so drivers that walk it see none; a driver asks this where it
 * needs the link the platform gives a function (amdgpu's PCIe speeds). */
struct pci_dev *stela_linux_pci_upstream(struct pci_dev *dev);
/* Free what pci_save_state allocated to keep the function's capabilities. */
void stela_linux_pci_free_saved_state(struct pci_dev *dev);
#endif
