// SPDX-License-Identifier: GPL-2.0-or-later
/* A physical function as a Linux pci_dev. Stela's PCI owner enumerates and
 * places functions, so this only presents one: configuration space and DMA go
 * through its owner's callbacks, as an x86 host bridge's pci_ops and IOMMU's
 * dma_map_ops would. x86 caches snoop DMA, so no operation syncs. */
#include <linux/dma-map-ops.h>
#include <linux/mm.h>
#include <linux/pci.h>
#include <linux/scatterlist.h>
#include <linux/slab.h>
#include <stela/catalogue.h>
#include <stela/irq.h>
#include <stela/linux.h>
#include <stela/pci.h>

struct host_function {
	struct stela_linux_pci_function function;
	const struct stela_linux_owner *owner;
	struct pci_bus bus;
	struct pci_dev dev;
	struct host_function *quarantined_next;
	/* The copy of the function's platform ROM, and the next attached
	 * function's with one. */
	struct page *rom;
	struct host_function *rom_next;
	/* The ports above it, and the next attached function's with any. */
	struct upstream_port *ports;
	unsigned int port_count;
	struct host_function *ports_next;
	/* The lines of the function's MSI-X vectors while dev.msix_enabled. */
	unsigned int msix_first, msix_count;
};

/* A port above an attached function, kept as its owner read it. */
struct upstream_port {
	struct pci_bus bus;
	struct pci_dev dev;
	char name[16];
	u32 config[64];
};

/* The functions with ports above them. Only the serialized owner attaches
 * and releases. */
static struct host_function *port_hosts;

/* A port answers reads from the copy of its configuration; past its first
 * 256 bytes it answers as a function without extended space. */
static int read_port(struct pci_bus *bus, unsigned int devfn, int where, int size, u32 *value)
{
	const struct upstream_port *port = bus->sysdata;

	if (devfn != port->dev.devfn || where < 0 ||
	    (size_t)where + (size_t)size > sizeof(port->config))
		return PCIBIOS_DEVICE_NOT_FOUND;
	*value = 0;
	memcpy(value, (const u8 *)port->config + where, (size_t)size);
	return 0;
}

static int write_port(struct pci_bus *bus, unsigned int devfn, int where, int size, u32 value)
{
	(void)bus;
	(void)devfn;
	(void)where;
	(void)size;
	(void)value;
	stela_linux_unsupported("a PCI write to a port above a function: the ports stay as the firmware set them");
	return PCIBIOS_SET_FAILED;
}

static struct pci_ops port_ops = {.read = read_port, .write = write_port};

/* At most the depth of a PCI Express hierarchy that any GPU sits in: a root
 * port, a platform switch and a GPU's own switch, with room to spare. */
#define MAX_UPSTREAM_PORTS 8

static int attach_ports(struct host_function *host, const struct stela_linux_pci_port *ports,
			size_t count)
{
	if (!count)
		return 0;
	if (count > MAX_UPSTREAM_PORTS)
		count = MAX_UPSTREAM_PORTS;
	host->ports = kcalloc(count, sizeof(*host->ports), GFP_KERNEL);
	if (!host->ports)
		return -ENOMEM;
	for (size_t i = 0; i < count; i++) {
		struct upstream_port *port = &host->ports[i];
		struct pci_dev *dev = &port->dev;
		u32 class_revision;

		memcpy(port->config, ports[i].config, sizeof(port->config));
		port->bus.sysdata = port;
		port->bus.ops = &port_ops;
		port->bus.number = (unsigned char)ports[i].bus;
		dev->bus = &port->bus;
		dev->devfn = ports[i].devfn;
		dev->current_state = PCI_D0;
		pci_read_config_word(dev, PCI_VENDOR_ID, &dev->vendor);
		pci_read_config_word(dev, PCI_DEVICE_ID, &dev->device);
		pci_read_config_byte(dev, PCI_HEADER_TYPE, &dev->hdr_type);
		pci_read_config_dword(dev, PCI_CLASS_REVISION, &class_revision);
		dev->revision = (u8)class_revision;
		dev->class = class_revision >> 8;
		if (stela_linux_pci_find_capability(dev, PCI_CAP_ID_EXP, &dev->pcie_cap) ||
		    stela_linux_pci_express_probe(dev))
			dev->pcie_cap = 0;
		snprintf(port->name, sizeof(port->name), "%04x:%02x:%02x.%d", 0, ports[i].bus,
			 PCI_SLOT(ports[i].devfn), PCI_FUNC(ports[i].devfn));
		dev->dev.init_name = port->name;
	}
	host->port_count = (unsigned int)count;
	host->ports_next = port_hosts;
	port_hosts = host;
	return 0;
}

static void release_ports(struct host_function *host)
{
	if (!host->ports)
		return;
	for (struct host_function **link = &port_hosts; *link; link = &(*link)->ports_next) {
		if (*link == host) {
			*link = host->ports_next;
			break;
		}
	}
	kfree(host->ports);
	host->ports = NULL;
	host->port_count = 0;
}

struct pci_dev *stela_linux_pci_upstream(struct pci_dev *dev)
{
	struct pci_dev *bridge;

	if (!dev)
		return NULL;
	bridge = pci_upstream_bridge(dev);
	if (bridge)
		return bridge;
	for (struct host_function *host = port_hosts; host; host = host->ports_next) {
		if (dev == &host->dev)
			return &host->ports[0].dev;
		for (unsigned int i = 0; i < host->port_count; i++)
			if (dev == &host->ports[i].dev)
				return i + 1 < host->port_count ? &host->ports[i + 1].dev : NULL;
	}
	return NULL;
}

/* The functions with a platform ROM copy. Only the serialized owner attaches
 * and releases. */
static struct host_function *rom_hosts;

/* x86 Linux keeps the video BIOS EFI hands over in memory the firmware
 * reserved, and amdgpu maps pci_dev::rom with ioremap. The copy here is
 * pages of the layer's own, which the owner's device mappings do not hold,
 * so ioremap answers it from this list. */
void __iomem *stela_linux_pci_rom_map(phys_addr_t start, size_t size)
{
	for (struct host_function *host = rom_hosts; host; host = host->rom_next) {
		phys_addr_t rom = host->dev.rom;

		if (start >= rom && size <= host->dev.romlen && start - rom <= host->dev.romlen - size)
			return (void __iomem __force *)((u8 *)page_address(host->rom) + (start - rom));
	}
	return NULL;
}

bool stela_linux_pci_rom_mapped(const volatile void __iomem *address)
{
	unsigned long at = (unsigned long)address;

	for (struct host_function *host = rom_hosts; host; host = host->rom_next) {
		unsigned long copy = (unsigned long)page_address(host->rom);

		if (at >= copy && at - copy < host->dev.romlen)
			return true;
	}
	return false;
}

static void release_rom(struct host_function *host)
{
	if (!host->rom)
		return;
	for (struct host_function **link = &rom_hosts; *link; link = &(*link)->rom_next) {
		if (*link == host) {
			*link = host->rom_next;
			break;
		}
	}
	__free_pages(host->rom, (unsigned int)get_order(host->dev.romlen));
	host->rom = NULL;
}

static u64 quarantined_functions;
static struct host_function *quarantined_hosts;

u64 stela_linux_pci_quarantined_functions(void)
{
	return __atomic_load_n(&quarantined_functions, __ATOMIC_RELAXED);
}

static const struct stela_linux_pci_function *of_bus(const struct pci_bus *bus)
{
	return bus->sysdata;
}

static const struct stela_linux_pci_function *of_device(struct device *dev)
{
	return of_bus(to_pci_dev(dev)->bus);
}

static int read_config(struct pci_bus *bus, unsigned int devfn, int where, int size, u32 *value)
{
	const struct stela_linux_pci_function *function = of_bus(bus);

	if (devfn != function->devfn)
		return PCIBIOS_DEVICE_NOT_FOUND;
	return function->read_config(function->context, where, size, value);
}

static int write_config(struct pci_bus *bus, unsigned int devfn, int where, int size, u32 value)
{
	const struct stela_linux_pci_function *function = of_bus(bus);

	if (devfn != function->devfn)
		return PCIBIOS_DEVICE_NOT_FOUND;
	return function->write_config(function->context, where, size, value);
}

static struct pci_ops config_ops = {.read = read_config, .write = write_config};

static dma_addr_t map_phys(struct device *dev, phys_addr_t physical, size_t size,
			   enum dma_data_direction direction, unsigned long attributes)
{
	const struct stela_linux_pci_function *function = of_device(dev);

	return function->dma_map(function->context, physical, size, direction,
				 attributes & DMA_ATTR_MMIO);
}

static void unmap_phys(struct device *dev, dma_addr_t address, size_t size,
		       enum dma_data_direction direction, unsigned long attributes)
{
	const struct stela_linux_pci_function *function = of_device(dev);

	(void)attributes;
	function->dma_unmap(function->context, address, size, direction);
}

static void unmap_scatter(struct device *dev, struct scatterlist *list, int entries,
			  enum dma_data_direction direction, unsigned long attributes)
{
	struct scatterlist *entry;
	int i;

	for_each_sg(list, entry, entries, i)
		unmap_phys(dev, sg_dma_address(entry), sg_dma_len(entry), direction, attributes);
}

static int map_scatter(struct device *dev, struct scatterlist *list, int entries,
		       enum dma_data_direction direction, unsigned long attributes)
{
	struct scatterlist *entry;
	int i;

	for_each_sg(list, entry, entries, i) {
		dma_addr_t address = map_phys(dev, sg_phys(entry), entry->length, direction,
					      attributes);

		if (address == DMA_MAPPING_ERROR) {
			unmap_scatter(dev, list, i, direction, attributes);
			return -EIO;
		}
		sg_dma_address(entry) = address;
		sg_dma_len(entry) = entry->length;
	}
	return entries;
}

static void *alloc_coherent(struct device *dev, size_t size, dma_addr_t *address, gfp_t gfp,
			    unsigned long attributes)
{
	unsigned int order = (unsigned int)get_order(size);
	struct page *page = alloc_pages(gfp | __GFP_ZERO, order);

	if (!page)
		return NULL;
	*address = map_phys(dev, page_to_phys(page), size, DMA_BIDIRECTIONAL, attributes);
	if (*address == DMA_MAPPING_ERROR) {
		__free_pages(page, order);
		return NULL;
	}
	return page_address(page);
}

static void free_coherent(struct device *dev, size_t size, void *cpu, dma_addr_t address,
			  unsigned long attributes)
{
	unmap_phys(dev, address, size, DMA_BIDIRECTIONAL, attributes);
	__free_pages(virt_to_page(cpu), (unsigned int)get_order(size));
}

/* Linux dma_common_alloc_pages, which x86's direct and IOMMU mappings both
 * reach from dma_alloc_pages: zeroed pages, mapped for the device. nouveau's
 * falcon firmware (the GSP booter on a GTX 16 or RTX 20) is loaded from such
 * pages and takes their address without a check. */
static struct page *alloc_mapped_pages(struct device *dev, size_t size, dma_addr_t *address,
				       enum dma_data_direction direction, gfp_t gfp)
{
	unsigned int order = (unsigned int)get_order(size);
	struct page *page = alloc_pages(gfp | __GFP_ZERO, order);

	if (!page)
		return NULL;
	*address = map_phys(dev, page_to_phys(page), size, direction, 0);
	if (*address == DMA_MAPPING_ERROR) {
		__free_pages(page, order);
		return NULL;
	}
	return page;
}

static void free_mapped_pages(struct device *dev, size_t size, struct page *page,
			      dma_addr_t address, enum dma_data_direction direction)
{
	unmap_phys(dev, address, size, direction, 0);
	__free_pages(page, (unsigned int)get_order(size));
}

/* The device reaches whatever its owner hands it: its addresses, not physical
 * memory, are what a mask must cover. */
static int mask_supported(struct device *dev, u64 mask)
{
	return mask >= of_device(dev)->dma_limit;
}

static const struct dma_map_ops mapping_ops = {
	.map_phys = map_phys,
	.unmap_phys = unmap_phys,
	.map_sg = map_scatter,
	.unmap_sg = unmap_scatter,
	.alloc = alloc_coherent,
	.free = free_coherent,
	.alloc_pages_op = alloc_mapped_pages,
	.free_pages = free_mapped_pages,
	.dma_supported = mask_supported,
};

static struct host_function *host_of(struct pci_dev *dev);
static void disable_msix(struct pci_dev *dev);

int pci_enable_msi(struct pci_dev *dev)
{
	const struct stela_linux_pci_function *function = of_bus(dev->bus);

	if (dev->msix_enabled)
		return -EINVAL;
	int error = function->enable_msi(function->context);

	dev->msi_enabled = !error;
	return error;
}

void pci_disable_msi(struct pci_dev *dev)
{
	const struct stela_linux_pci_function *function = of_bus(dev->bus);

	if (dev->msi_enabled)
		function->disable_msi(function->context);
	dev->msi_enabled = 0;
}

/* drivers/pci/msi/msi.c writes a function's message back after a reset
 * cleared it. The owner holds the message, so the owner routes it again. */
void pci_restore_msi_state(struct pci_dev *dev)
{
	const struct stela_linux_pci_function *function = of_bus(dev->bus);
	unsigned int first = 0;

	if (dev->msix_enabled &&
	    (function->enable_msix(function->context, host_of(dev)->msix_count, &first) !=
		     (int)host_of(dev)->msix_count ||
	     first != host_of(dev)->msix_first)) {
		pci_err(dev, "the owner could not write the MSI-X table again after a reset\n");
		disable_msix(dev);
	}
	if (dev->msi_enabled && function->enable_msi(function->context)) {
		pci_err(dev, "the owner could not route the message again after a reset\n");
		dev->msi_enabled = 0;
	}
}

bool stela_linux_pci_routes_msix(const struct pci_dev *dev)
{
	return dev->bus && dev->bus->ops == &config_ops && of_bus(dev->bus)->enable_msix;
}

static struct host_function *host_of(struct pci_dev *dev)
{
	return container_of(dev, struct host_function, dev);
}

int stela_linux_pci_msix_lines(struct pci_dev *dev)
{
	unsigned int first = 0;

	if (!stela_linux_pci_routes_msix(dev))
		return 0;
	const struct stela_linux_pci_function *function = of_bus(dev->bus);
	int lines = function->enable_msix(function->context, 0, &first);

	return lines > 0 ? lines : 0;
}

/* MSI-X vectors, min_vecs to max_vecs of them, as drivers/pci/msi/msi.c
 * sets them up: as many as the table holds and the owner routes, each
 * masked until its line is enabled. */
static int enable_msix(struct pci_dev *dev, unsigned int min_vecs, unsigned int max_vecs)
{
	const struct stela_linux_pci_function *function = of_bus(dev->bus);
	int entries = pci_msix_vec_count(dev);
	unsigned int first = 0;

	if (entries < 0)
		return entries;
	if ((unsigned int)entries < min_vecs)
		return -ENOSPC;
	if (dev->msi_enabled || dev->msix_enabled)
		return -EINVAL;
	int vectors = function->enable_msix(function->context, min(max_vecs, (unsigned int)entries),
					    &first);
	if (vectors <= 0)
		return vectors ? vectors : -ENOSPC;
	if ((unsigned int)vectors < min_vecs) {
		function->disable_msix(function->context);
		return -ENOSPC;
	}
	host_of(dev)->msix_first = first;
	host_of(dev)->msix_count = (unsigned int)vectors;
	dev->msix_enabled = 1;
	return vectors;
}

static void disable_msix(struct pci_dev *dev)
{
	const struct stela_linux_pci_function *function = of_bus(dev->bus);

	if (dev->msix_enabled)
		function->disable_msix(function->context);
	dev->msix_enabled = 0;
	host_of(dev)->msix_count = 0;
}

/* Interrupt vectors, following drivers/pci/msi/api.c: MSI-X, MSI and INTx
 * in that order, each only where the caller allows it. MSI-X gives as many
 * vectors as its owner routes; MSI and INTx give one. */
int pci_alloc_irq_vectors(struct pci_dev *dev, unsigned int min_vecs, unsigned int max_vecs,
			  unsigned int flags)
{
	int error = -ENOSPC;
	bool tried_msix = (flags & PCI_IRQ_MSIX) && stela_linux_pci_routes_msix(dev);

	if (!min_vecs || max_vecs < min_vecs)
		return -ERANGE;
	if (tried_msix) {
		int vectors = enable_msix(dev, min_vecs, max_vecs);

		if (vectors > 0)
			return vectors;
		error = vectors;
	}
	if (min_vecs > 1) {
		/* MSI and INTx give one vector; MSI-X, where routed, gave its answer. */
		if (!tried_msix)
			stela_linux_unsupported("more than one PCI interrupt vector");
		return error;
	}
	if (flags & PCI_IRQ_MSI) {
		error = pci_enable_msi(dev);
		if (!error)
			return 1;
	}
	if ((flags & PCI_IRQ_INTX) && dev->irq)
		return 1;
	if (!(flags & (PCI_IRQ_MSI | PCI_IRQ_INTX)) && !stela_linux_pci_routes_msix(dev))
		stela_linux_unsupported("PCI MSI-X vectors");
	return error;
}

void pci_free_irq_vectors(struct pci_dev *dev)
{
	disable_msix(dev);
	pci_disable_msi(dev);
}

/* The Linux interrupt of vector nr: an MSI-X vector's line, or the
 * function's one line or message. */
int pci_irq_vector(struct pci_dev *dev, unsigned int nr)
{
	if (dev->msix_enabled)
		return nr < host_of(dev)->msix_count ? (int)(host_of(dev)->msix_first + nr) : -EINVAL;
	return nr ? -EINVAL : (int)dev->irq;
}

/* What a driver whose probe failed left running on its function, after its
 * managed resources were released: the interrupt requests on its lines, its
 * MSI or MSI-X vectors and its bus mastering. Linux's driver core leaves
 * these to the driver's own error path; here a request left behind would run
 * a handler of a driver that gave the function up, and a function that may
 * still master the bus could write memory nobody owns, so each is undone,
 * with one line that names it. The function then goes to the next driver,
 * or stays unbound, as its claim left it. */
void stela_linux_pci_quiesce_failed(struct pci_dev *dev, const char *driver)
{
	stela_linux_pci_quiesce(dev, driver, "failed probe");
}

void stela_linux_pci_quiesce(struct pci_dev *dev, const char *driver, const char *after)
{
	struct host_function *host = host_of(dev);
	unsigned int freed = 0;
	u16 command;

	if (dev->bus->ops != &config_ops)
		return;
	if (dev->msix_enabled)
		for (unsigned int i = 0; i < host->msix_count; i++)
			freed += stela_linux_irq_free_line(host->msix_first + i);
	if (dev->irq)
		freed += stela_linux_irq_free_line(dev->irq);
	if (freed)
		pci_warn(dev, "%s's %s left %u interrupt requests; they are freed\n", driver,
			 after, freed);
	if (dev->msi_enabled || dev->msix_enabled) {
		pci_warn(dev, "%s's %s left %s on; it is turned off\n", driver, after,
			 dev->msix_enabled ? "MSI-X" : "MSI");
		pci_free_irq_vectors(dev);
	}
	if (!pci_read_config_word(dev, PCI_COMMAND, &command) && command != 0xffff &&
	    (command & PCI_COMMAND_MASTER)) {
		pci_warn(dev, "%s's %s left its bus mastering on; it is turned off\n", driver,
			 after);
		pci_write_config_word(dev, PCI_COMMAND, command & (u16)~PCI_COMMAND_MASTER);
	}
	dev->is_busmaster = 0;
}

static void release(struct device *dev)
{
	struct host_function *host = container_of(to_pci_dev(dev), struct host_function, dev);
	if (stela_linux_owner() != host->owner || !stela_linux_quiescent()) {
		/* The serialized owner admits only one writer. Preserve the original
		 * C allocation and every callback/root in its context; final Rust
		 * destruction and native deallocation need process context. */
		host->quarantined_next = quarantined_hosts;
		quarantined_hosts = host;
		u64 count = __atomic_load_n(&quarantined_functions, __ATOMIC_RELAXED);
		if (count != ~(u64)0)
			__atomic_store_n(&quarantined_functions, count + 1, __ATOMIC_RELAXED);
		return;
	}
	release_rom(host);
	release_ports(host);
	stela_linux_pci_free_saved_state(&host->dev);
	if (host->function.release_context)
		host->function.release_context(host->function.context);
	kfree(host);
}

struct pci_dev *stela_linux_pci_attach(const struct stela_linux_pci_function *function, int *status)
{
	if (stela_linux_kernel_hosted()) {
		if (status)
			*status = -EPERM;
		return NULL;
	}
	int error = -ENOMEM;
	struct host_function *host = kzalloc_obj(*host);
	if (status)
		*status = error;
	if (!host)
		return NULL;
	host->function = *function;
	host->owner = stela_linux_owner();
	host->bus.sysdata = &host->function;
	host->bus.ops = &config_ops;
	host->bus.number = (unsigned char)function->bus;
	struct pci_dev *dev = &host->dev;
	u32 class_revision;

	dev->bus = &host->bus;
	dev->devfn = function->devfn;
	dev->irq = function->irq;
	dev->dma_mask = DMA_BIT_MASK(32);
	dev->current_state = PCI_D0;
	dev->rom_base_reg = PCI_ROM_ADDRESS;
    error = pci_read_config_word(dev, PCI_VENDOR_ID, &dev->vendor);
    if (!error)
        error = pci_read_config_word(dev, PCI_DEVICE_ID, &dev->device);
    if (!error)
        error = pci_read_config_word(dev, PCI_SUBSYSTEM_VENDOR_ID, &dev->subsystem_vendor);
    if (!error)
        error = pci_read_config_word(dev, PCI_SUBSYSTEM_ID, &dev->subsystem_device);
    if (!error)
        error = pci_read_config_byte(dev, PCI_HEADER_TYPE, &dev->hdr_type);
    if (!error)
        error = pci_read_config_dword(dev, PCI_CLASS_REVISION, &class_revision);
    if (!error)
        error = stela_linux_pci_find_capability(dev, PCI_CAP_ID_EXP, &dev->pcie_cap);
    if (!error)
        error = stela_linux_pci_express_probe(dev);
    if (error) {
        /* No original device_initialize/publication or context transfer has
         * occurred. Rust retains exact State/Host/authority on this refusal. */
        if (status)
            *status = pcibios_err_to_errno(error);
        kfree(host);
        return NULL;
    }
    dev->revision = (u8)class_revision;
    dev->class = class_revision >> 8;
	for (int i = 0; i < PCI_STD_NUM_BARS; i++) {
		const struct stela_linux_pci_region *region = &function->regions[i];

		if (!region->size)
			continue;
		dev->resource[i].start = region->start;
		dev->resource[i].end = region->start + region->size - 1;
		dev->resource[i].flags = region->flags;
	}
	if (function->rom_bar.size) {
		dev->resource[PCI_ROM_RESOURCE].start = function->rom_bar.start;
		dev->resource[PCI_ROM_RESOURCE].end =
			function->rom_bar.start + function->rom_bar.size - 1;
		dev->resource[PCI_ROM_RESOURCE].flags = function->rom_bar.flags;
	}
	/* The caller's image lasts only for the attach. */
	host->function.rom = NULL;
	if (function->rom_length) {
		host->rom = alloc_pages(GFP_KERNEL, (unsigned int)get_order(function->rom_length));
		if (!host->rom) {
			if (status)
				*status = -ENOMEM;
			kfree(host);
			return NULL;
		}
		memcpy(page_address(host->rom), function->rom, function->rom_length);
		dev->rom = page_to_phys(host->rom);
		dev->romlen = function->rom_length;
		host->rom_next = rom_hosts;
		rom_hosts = host;
	}
	/* The caller's ports, like its image, last only for the attach. */
	host->function.upstream = NULL;
	host->function.upstream_count = 0;
	if (attach_ports(host, function->upstream, function->upstream_count)) {
		if (status)
			*status = -ENOMEM;
		release_rom(host);
		kfree(host);
		return NULL;
	}
	dev->dev.dma_mask = &dev->dma_mask;
	dev->dev.coherent_dma_mask = DMA_BIT_MASK(32);
	/* pci_device_add's DMA parameters, which drivers read and change
	 * (amdgpu sets its largest segment). */
	dev->dev.dma_parms = &dev->dma_parms;
	dma_set_max_seg_size(&dev->dev, 65536);
	dma_set_seg_boundary(&dev->dev, 0xffffffff);
	dev->dev.release = release;
	set_dma_ops(&dev->dev, &mapping_ops);
	device_initialize(&dev->dev);
	error = dev_set_name(&dev->dev, "%04x:%02x:%02x.%d", 0, function->bus,
                         PCI_SLOT(function->devfn), PCI_FUNC(function->devfn));
	if (!error && function->enroll)
		error = function->enroll(function->context, &dev->dev);
	if (!error)
		error = stela_linux_pci_publish_device(dev);
	if (error) {
		/* Refusal never takes the caller's host-context ownership. Any exact
         * source record already enrolled retires through the real final put. */
		host->function.release_context = NULL;
		put_device(&dev->dev);
		if (status)
			*status = error;
		return NULL;
	}
	if (status)
		*status = 0;
	return dev;
}

/* The configuration a published function's owner read for it, if any. */
struct snapshot {
	size_t length;
	u8 bytes[];
};

static int read_identity_config(struct pci_bus *bus, unsigned int devfn, int where, int size,
				u32 *value)
{
	const struct stela_linux_pci_function *function = bus->sysdata;
	const struct snapshot *snapshot = function->context;

	(void)devfn;
	if (!snapshot || where < 0 || size <= 0 || (size_t)where + (size_t)size > snapshot->length) {
		*value = ~(u32)0;
		return PCIBIOS_DEVICE_NOT_FOUND;
	}
	*value = 0;
	for (int i = size - 1; i >= 0; i--)
		*value = *value << 8 | snapshot->bytes[where + i];
	return PCIBIOS_SUCCESSFUL;
}

static void free_snapshot(void *snapshot)
{
	kfree(snapshot);
}

static int write_identity_config(struct pci_bus *bus, unsigned int devfn, int where, int size,
				 u32 value)
{
	(void)bus, (void)devfn, (void)where, (void)size, (void)value;
	stela_linux_unsupported("a PCI configuration write to a function published for searches only");
	return PCIBIOS_DEVICE_NOT_FOUND;
}

static struct pci_ops identity_ops = {.read = read_identity_config,
				      .write = write_identity_config};

struct pci_dev *stela_linux_pci_publish_identity(const struct stela_linux_pci_identity *identity,
						 int *status)
{
	return stela_linux_pci_publish_snapshot(identity, NULL, 0, status);
}

struct pci_dev *stela_linux_pci_publish_snapshot(const struct stela_linux_pci_identity *identity,
						 const u8 *config, size_t length, int *status)
{
	int error = stela_linux_kernel_hosted() ? -EPERM : -ENOMEM;
	struct host_function *host = NULL;
	struct snapshot *snapshot = NULL;

	if (status)
		*status = length > 256 || (length && !config) ? -EINVAL : error;
	if (length > 256 || (length && !config))
		return NULL;
	if (error != -EPERM) {
		host = kzalloc_obj(*host);
		snapshot = length ? kmalloc(struct_size(snapshot, bytes, length), GFP_KERNEL) : NULL;
	}
	if (!host || (length && !snapshot)) {
		kfree(host);
		kfree(snapshot);
		return NULL;
	}
	/* No callback; the context is the snapshot, if any, which the release
	 * frees with the host. */
	if (snapshot) {
		snapshot->length = length;
		memcpy(snapshot->bytes, config, length);
		host->function.context = snapshot;
		host->function.release_context = free_snapshot;
	}
	host->function.bus = identity->bus;
	host->function.devfn = identity->devfn;
	host->owner = stela_linux_owner();
	host->bus.sysdata = &host->function;
	host->bus.ops = &identity_ops;
	host->bus.number = (unsigned char)identity->bus;
	struct pci_dev *dev = &host->dev;

	dev->bus = &host->bus;
	dev->devfn = identity->devfn;
	dev->vendor = identity->vendor;
	dev->device = identity->device;
	dev->subsystem_vendor = identity->subsystem_vendor;
	dev->subsystem_device = identity->subsystem_device;
	dev->revision = (u8)identity->class_revision;
	dev->class = identity->class_revision >> 8;
	dev->current_state = PCI_D0;
	dev->dev.release = release;
	device_initialize(&dev->dev);
	error = dev_set_name(&dev->dev, "%04x:%02x:%02x.%d", 0, identity->bus,
			     PCI_SLOT(identity->devfn), PCI_FUNC(identity->devfn));
	if (!error)
		error = stela_linux_pci_publish_for_searches(dev);
	if (status)
		*status = error;
	if (error) {
		put_device(&dev->dev);
		return NULL;
	}
	return dev;
}

/* The functions whose driver ended the picture the firmware left on their
 * outputs, with the step that did. Few GPUs can show that picture. Only the
 * serialized owner writes; a stop on any processor may read, so each entry's
 * step is stored last and, once set, never changes. */
static struct {
	unsigned int bus, devfn;
	const char *why;
} screens_ended[8];

void stela_linux_pci_screen_ended(struct pci_dev *dev, const char *why)
{
	unsigned int bus = dev->bus->number, devfn = dev->devfn;

	for (size_t i = 0; i < ARRAY_SIZE(screens_ended); i++) {
		if (!__atomic_load_n(&screens_ended[i].why, __ATOMIC_ACQUIRE)) {
			screens_ended[i].bus = bus;
			screens_ended[i].devfn = devfn;
			__atomic_store_n(&screens_ended[i].why, why, __ATOMIC_RELEASE);
			return;
		}
		if (screens_ended[i].bus == bus && screens_ended[i].devfn == devfn)
			return;
	}
	stela_linux_unsupported("recording the end of the firmware's picture on a ninth PCI function");
}

const char *stela_linux_pci_screen_ended_at(unsigned int bus, unsigned int devfn)
{
	for (size_t i = 0; i < ARRAY_SIZE(screens_ended); i++) {
		const char *why = __atomic_load_n(&screens_ended[i].why, __ATOMIC_ACQUIRE);

		if (!why)
			return NULL;
		if (screens_ended[i].bus == bus && screens_ended[i].devfn == devfn)
			return why;
	}
	return NULL;
}

#ifdef STELA_LINUX_PCI_HOST_TEST
#include <linux/bottom_half.h>
#include <linux/interrupt.h>
#include <stela/page_test.h>
#include <stela/test.h>

#define BASE 0xc0000000ULL
#define BUS_OFFSET 0x100000000ULL

static u8 config[256];
static unsigned int mapped, fail_map_after = ~0U;
static int msi_error, config_error;
static bool on_message;
static unsigned int context_releases;

static void test_release_context(void *context)
{
	assert(context == config && stela_linux_quiescent());
	context_releases++;
}

static int test_read(void *context, int where, int size, u32 *value)
{
	assert(context == config);
	if (config_error)
		return config_error;
	*value = 0;
	memcpy(value, &config[where], (size_t)size);
	return 0;
}

static int test_write(void *context, int where, int size, u32 value)
{
	memcpy(&config[where], &value, (size_t)size);
	return context == config ? 0 : PCIBIOS_SET_FAILED;
}

static bool last_pageless;

static dma_addr_t test_map(void *context, phys_addr_t physical, size_t size,
			   enum dma_data_direction direction, bool pageless)
{
	(void)context;
	(void)size;
	(void)direction;
	last_pageless = pageless;
	if (!fail_map_after)
		return DMA_MAPPING_ERROR;
	fail_map_after--;
	mapped++;
	return physical + BUS_OFFSET;
}

static void test_unmap(void *context, dma_addr_t address, size_t size,
		       enum dma_data_direction direction)
{
	(void)context;
	(void)address;
	(void)size;
	(void)direction;
	mapped--;
}

static int test_enable_msi(void *context)
{
	(void)context;
	on_message = !msi_error;
	return msi_error;
}

static void test_disable_msi(void *context)
{
	(void)context;
	on_message = false;
}

static unsigned int refusals;

static void test_unsupported(void *context, const char *operation)
{
	(void)context;
	assert(strstr(operation, "PCI"));
	refusals++;
}

static unsigned int probes;

static int test_probe(struct pci_dev *dev, const struct pci_device_id *id)
{
	(void)dev, (void)id;
	probes++;
	return 0;
}

/* A function published for searches is found by class, table and address,
 * says who it is, and is offered to no driver, registered before or after. */
static void test_identity(void)
{
	static const struct pci_device_id bridges[] = {
		{.vendor = PCI_VENDOR_ID_INTEL, .device = 0x5182,
		 .subvendor = (u32)PCI_ANY_ID, .subdevice = (u32)PCI_ANY_ID}, {}};
	static struct pci_driver early = {.name = "early", .id_table = bridges,
					  .probe = test_probe};
	static struct pci_driver late = {.name = "late", .id_table = bridges,
					 .probe = test_probe};
	const struct stela_linux_pci_identity identity = {
		.bus = 0, .devfn = PCI_DEVFN(0x1f, 0),
		.vendor = PCI_VENDOR_ID_INTEL, .device = 0x5182,
		.subsystem_vendor = 0x17aa, .subsystem_device = 0x22e7,
		.class_revision = PCI_CLASS_BRIDGE_ISA << 16 | 0x01,
	};
	int status = 1;
	u64 quarantined = stela_linux_pci_quarantined_functions();

	assert(!pci_get_class(PCI_CLASS_BRIDGE_ISA << 8, NULL));
	assert(!pci_register_driver(&early));
	struct pci_dev *bridge = stela_linux_pci_publish_identity(&identity, &status);

	assert(bridge && !status);
	assert(!pci_register_driver(&late));
	assert(!probes && !stela_linux_pci_bound(bridge));
	assert(pci_dev_present(bridges));
	struct pci_dev *found = pci_get_class(PCI_CLASS_BRIDGE_ISA << 8, NULL);

	assert(found == bridge);
	assert(found->vendor == PCI_VENDOR_ID_INTEL && found->device == 0x5182);
	assert(found->subsystem_vendor == 0x17aa && found->subsystem_device == 0x22e7);
	assert(found->class == PCI_CLASS_BRIDGE_ISA << 8 && found->revision == 0x01);
	assert(!pci_resource_len(found, 0) && !found->irq);
	assert(!strcmp(dev_name(&found->dev), "0000:00:1f.0"));
	assert(!pci_get_class(PCI_CLASS_BRIDGE_ISA << 8, found));
	found = pci_get_domain_bus_and_slot(0, 0, PCI_DEVFN(0x1f, 0));
	assert(found == bridge);
	pci_dev_put(found);

	/* Its configuration is its owner's: reads fail as for an absent
	 * function, and a write is refused and reported. */
	u32 value = 0;
	unsigned int before = refusals;

	assert(pci_read_config_dword(bridge, PCI_VENDOR_ID, &value) == PCIBIOS_DEVICE_NOT_FOUND);
	assert(value == ~(u32)0);
	assert(pci_write_config_word(bridge, PCI_COMMAND, PCI_COMMAND_MASTER) ==
	       PCIBIOS_DEVICE_NOT_FOUND);
	assert(refusals == before + 1);
	/* The cases after this one count their own refusals from here. */
	refusals = before;

	stela_linux_pci_remove_device(bridge);
	assert(!pci_get_class(PCI_CLASS_BRIDGE_ISA << 8, NULL) && !pci_dev_present(bridges));

	/* A host bridge with the configuration its owner read: reads inside
	 * the copy answer it, past it they fail, and writes are refused. */
	const struct stela_linux_pci_identity host_bridge = {
		.bus = 0, .devfn = PCI_DEVFN(0, 0), .vendor = PCI_VENDOR_ID_INTEL, .device = 0x3e30,
		.class_revision = PCI_CLASS_BRIDGE_HOST << 16 | 0x0d,
	};
	u8 snapshot[0x58] = {0x86, 0x80, 0x30, 0x3e};
	snapshot[0x48] = 0x01, snapshot[0x49] = 0x00, snapshot[0x4a] = 0xd1, snapshot[0x4b] = 0xfe;
	snapshot[0x54] = 0x19;
	assert(!stela_linux_pci_publish_snapshot(&host_bridge, snapshot, 257, &status));
	assert(status == -EINVAL);
	struct pci_dev *host = stela_linux_pci_publish_snapshot(&host_bridge, snapshot,
								 sizeof(snapshot), &status);
	assert(host && !status);
	found = pci_get_domain_bus_and_slot(0, 0, PCI_DEVFN(0, 0));
	assert(found == host);
	pci_dev_put(found);
	assert(!pci_read_config_dword(host, 0x48, &value) && value == 0xfed10001);
	u16 word = 0;
	assert(!pci_read_config_word(host, PCI_DEVICE_ID, &word) && word == 0x3e30);
	u8 byte = 0;
	assert(!pci_read_config_byte(host, 0x54, &byte) && byte == 0x19);
	assert(pci_read_config_dword(host, 0x58, &value) == PCIBIOS_DEVICE_NOT_FOUND);
	assert(value == ~(u32)0);
	assert(pci_write_config_dword(host, 0x48, 0) == PCIBIOS_DEVICE_NOT_FOUND);
	assert(refusals == before + 1);
	refusals = before;
	stela_linux_pci_remove_device(host);
	assert(stela_linux_pci_quarantined_functions() == quarantined);
	pci_unregister_driver(&early);
	pci_unregister_driver(&late);
}

/* A PCI Express capability at 0x60 of the given type, link capabilities
 * and trained link: speeds as LNKCAP/LNKSTA encode them (1 = 2.5 GT/s). */
static void express(u32 *dwords, u16 type, u8 max_speed, u8 max_width, u8 speed, u8 width)
{
	u8 *bytes = (u8 *)dwords;

	bytes[PCI_STATUS] = PCI_STATUS_CAP_LIST;
	bytes[PCI_CAPABILITY_LIST] = 0x60;
	bytes[0x60] = PCI_CAP_ID_EXP;
	bytes[0x61] = 0;
	*(u16 *)&bytes[0x60 + PCI_EXP_FLAGS] = (u16)(2 | type << 4);
	*(u32 *)&bytes[0x60 + PCI_EXP_LNKCAP] = (u32)max_speed | (u32)max_width << 4;
	*(u16 *)&bytes[0x60 + PCI_EXP_LNKSTA] = (u16)(speed | width << 4);
	*(u32 *)&bytes[0x60 + PCI_EXP_LNKCAP2] = (u32)GENMASK(max_speed, 1);
}

static void identify(u32 *dwords, u16 vendor, u16 device, u32 class)
{
	dwords[0] = (u32)device << 16 | vendor;
	dwords[PCI_CLASS_REVISION / 4] = class << 8 | 0xc1;
	((u8 *)dwords)[PCI_HEADER_TYPE] = PCI_HEADER_TYPE_BRIDGE;
}

/* A discrete Navi card in a slot: a root port that runs at 32 GT/s, the
 * card's own switch (its upstream and downstream ports, ATI's 1478 and
 * 1479) and the GPU, all at 16 GT/s x16, with the slot's link trained at
 * 8 GT/s x8. amdgpu skips the card's switch and finds the root port, with
 * its speeds and width, as on Linux; the slowest trained link is the
 * slot's, which the root port reports first from the top. */
static void test_upstream(struct stela_linux_pci_function function)
{
	static struct stela_linux_pci_port ports[3];
	struct stela_linux_pci_port *downstream = &ports[0], *upstream = &ports[1],
				    *root = &ports[2];
	u8 saved[256];

	memcpy(saved, config, sizeof(saved));
	express((u32 *)config, PCI_EXP_TYPE_ENDPOINT, 4, 16, 4, 16);
	memset(ports, 0, sizeof(ports));
	identify(downstream->config, PCI_VENDOR_ID_ATI, 0x1479, PCI_CLASS_BRIDGE_PCI << 8);
	express(downstream->config, PCI_EXP_TYPE_DOWNSTREAM, 4, 16, 4, 16);
	downstream->bus = 2;
	identify(upstream->config, PCI_VENDOR_ID_ATI, 0x1478, PCI_CLASS_BRIDGE_PCI << 8);
	express(upstream->config, PCI_EXP_TYPE_UPSTREAM, 4, 16, 3, 8);
	upstream->bus = 1;
	identify(root->config, PCI_VENDOR_ID_AMD, 0x14ab, PCI_CLASS_BRIDGE_PCI << 8);
	express(root->config, PCI_EXP_TYPE_ROOT_PORT, 5, 16, 3, 8);
	root->devfn = PCI_DEVFN(1, 1);
	function.upstream = ports;
	function.upstream_count = 3;
	/* The cases after this one count releases of the context from none. */
	function.release_context = NULL;
	struct pci_dev *gpu = stela_linux_pci_attach(&function, NULL);

	assert(gpu);
	/* The attach keeps its own copy. */
	memset(ports, 0xff, sizeof(ports));
	assert(!pci_upstream_bridge(gpu));
	struct pci_dev *partner = stela_linux_pci_upstream(gpu), *link = gpu;

	assert(partner && partner->vendor == PCI_VENDOR_ID_ATI && partner->device == 0x1479);
	assert(pci_pcie_type(partner) == PCI_EXP_TYPE_DOWNSTREAM);
	assert(!strcmp(dev_name(&partner->dev), "0000:02:00.0"));
	while (partner && partner->vendor == PCI_VENDOR_ID_ATI) {
		link = partner;
		partner = stela_linux_pci_upstream(link);
	}
	assert(link->device == 0x1478 && pci_pcie_type(link) == PCI_EXP_TYPE_UPSTREAM);
	assert(partner && partner->vendor == PCI_VENDOR_ID_AMD && partner->device == 0x14ab);
	assert(!strcmp(dev_name(&partner->dev), "0000:00:01.1"));
	assert(pci_pcie_type(partner) == PCI_EXP_TYPE_ROOT_PORT);
	assert(pcie_get_speed_cap(partner) == PCIE_SPEED_32_0GT);
	assert(pcie_get_width_cap(partner) == PCIE_LNK_X16);
	assert(pcie_get_speed_cap(link) == PCIE_SPEED_16_0GT);
	assert(pcie_get_speed_cap(gpu) == PCIE_SPEED_16_0GT);
	assert(!stela_linux_pci_upstream(partner));

	struct pci_dev *limiting = NULL;
	enum pci_bus_speed speed;
	enum pcie_link_width width;

	assert(pcie_bandwidth_available(gpu, &limiting, &speed, &width) == 8 * (8000 * 128 / 130));
	assert(limiting == partner && speed == PCIE_SPEED_8_0GT && width == PCIE_LNK_X8);

	/* The ports are read-only: a write is refused and reported. */
	u32 value;
	unsigned int before = refusals;

	assert(pci_write_config_word(partner, PCI_COMMAND, 0) == PCIBIOS_SET_FAILED);
	assert(refusals == before + 1);
	refusals = before;
	assert(!pci_read_config_dword(partner, PCI_VENDOR_ID, &value) && value == 0x14ab1022);
	assert(pci_read_config_dword(partner, 0x100, &value) == PCIBIOS_DEVICE_NOT_FOUND);

	/* A function attached without ports is on a root bus. */
	function.upstream = NULL;
	function.upstream_count = 0;
	struct pci_dev *alone = stela_linux_pci_attach(&function, NULL);

	assert(alone && !stela_linux_pci_upstream(alone));
	assert(pcie_bandwidth_available(alone, &limiting, &speed, &width) == 16 * (16000 * 128 / 130));
	assert(limiting == alone && speed == PCIE_SPEED_16_0GT && width == PCIE_LNK_X16);
	stela_linux_pci_remove_device(alone);
	stela_linux_pci_remove_device(gpu);
	/* The ports went with the function. */
	assert(!port_hosts);
	memcpy(config, saved, sizeof(saved));
}

/* A function with an MSI-X table of eight entries, whose owner routes as
 * many vectors as msix_routes allows. */
static u8 msix_config[256];
static int msix_routes;
static unsigned int msix_asked, msix_enabled_vectors, msix_writes;

static int msix_read(void *context, int where, int size, u32 *value)
{
	assert(context == msix_config);
	*value = 0;
	memcpy(value, &msix_config[where], (size_t)size);
	return 0;
}

static int msix_write(void *context, int where, int size, u32 value)
{
	assert(context == msix_config);
	memcpy(&msix_config[where], &value, (size_t)size);
	return 0;
}

static int msix_lines = 64;

static int msix_enable(void *context, unsigned int vectors, unsigned int *first_irq)
{
	assert(context == msix_config);
	if (!vectors)
		return msix_lines;
	msix_asked = vectors;
	msix_writes++;
	if (msix_routes < 0)
		return msix_routes;
	msix_enabled_vectors = min(vectors, (unsigned int)msix_routes);
	*first_irq = 40;
	return (int)msix_enabled_vectors;
}

static void msix_disable(void *context)
{
	assert(context == msix_config);
	msix_enabled_vectors = 0;
}

/* MSI-X gives as many vectors as the owner routes, each its own line, and
 * falls back to MSI where it routes none or too few. */
static void test_msix(void)
{
	memcpy(&msix_config[PCI_VENDOR_ID], "\x86\x80\xa0\x56", 4);
	memcpy(&msix_config[PCI_CLASS_REVISION], "\x08\x00\x00\x03", 4);
	msix_config[PCI_STATUS] = PCI_STATUS_CAP_LIST;
	msix_config[PCI_CAPABILITY_LIST] = 0x60;
	msix_config[0x60] = PCI_CAP_ID_MSIX;
	*(u16 *)&msix_config[0x60 + PCI_MSIX_FLAGS] = 7;
	struct stela_linux_pci_function function = {
		.context = msix_config, .read_config = msix_read, .write_config = msix_write,
		.dma_map = test_map, .dma_unmap = test_unmap,
		.enable_msi = test_enable_msi, .disable_msi = test_disable_msi,
		.enable_msix = msix_enable, .disable_msix = msix_disable,
		.dma_limit = DMA_BIT_MASK(40), .bus = 4, .devfn = PCI_DEVFN(0, 0), .irq = 11,
	};
	struct pci_dev *dev = stela_linux_pci_attach(&function, NULL);
	unsigned int before = refusals;

	assert(dev && stela_linux_pci_routes_msix(dev) && pci_msix_vec_count(dev) == 8);
	/* No more entries than the owner has lines for; none left is MSI. */
	msix_lines = 3;
	assert(pci_msix_vec_count(dev) == 3 && refusals == before);
	msix_lines = 0;
	assert(pci_msix_vec_count(dev) == -EINVAL && refusals == before + 1);
	refusals = before;
	msix_lines = 64;
	assert(!msix_asked);
	msix_routes = 8;
	assert(pci_alloc_irq_vectors(dev, 1, 4, PCI_IRQ_MSIX) == 4 && msix_asked == 4);
	assert(dev->msix_enabled && msix_enabled_vectors == 4 && !on_message);
	assert(pci_irq_vector(dev, 0) == 40 && pci_irq_vector(dev, 3) == 43);
	assert(pci_irq_vector(dev, 4) == -EINVAL);
	assert(pci_enable_msi(dev) == -EINVAL && !on_message);
	/* After a reset, the same vectors are written again. */
	msix_writes = 0;
	pci_restore_msi_state(dev);
	assert(msix_writes == 1 && msix_asked == 4 && dev->msix_enabled);
	pci_free_irq_vectors(dev);
	assert(!dev->msix_enabled && !msix_enabled_vectors);

	/* No more than the table holds; fewer where the routes run out. */
	assert(pci_alloc_irq_vectors(dev, 1, 64, PCI_IRQ_MSIX) == 8 && msix_asked == 8);
	pci_free_irq_vectors(dev);
	msix_routes = 2;
	assert(pci_alloc_irq_vectors(dev, 1, 4, PCI_IRQ_ALL_TYPES) == 2);
	pci_free_irq_vectors(dev);

	/* Too few routes for the minimum: none kept, and MSI gives one alone. */
	msix_routes = 1;
	assert(pci_alloc_irq_vectors(dev, 2, 4, PCI_IRQ_MSIX) == -ENOSPC);
	assert(!dev->msix_enabled && !msix_enabled_vectors);
	assert(pci_alloc_irq_vectors(dev, 9, 9, PCI_IRQ_MSIX) == -ENOSPC);
	msix_routes = -ENOSPC;
	assert(pci_alloc_irq_vectors(dev, 1, 4, PCI_IRQ_MSIX | PCI_IRQ_MSI) == 1);
	assert(on_message && !dev->msix_enabled && pci_irq_vector(dev, 0) == 11);
	pci_free_irq_vectors(dev);
	assert(!on_message && refusals == before);
	stela_linux_pci_remove_device(dev);
}

/* A device two registered drivers' tables hold, as i915's and xe's both hold
 * Tiger Lake: the first registered driver is offered it first, and when that
 * probe fails, what the probe set up is released and the next driver binds
 * it, as Linux's driver core tries each driver in turn. */
static u8 fallback_config[256];
static unsigned int first_probes, second_probes, first_releases;

static int fallback_read(void *context, int where, int size, u32 *value)
{
	*value = 0;
	memcpy(value, (u8 *)context + where, (size_t)size);
	return 0;
}

static int fallback_write(void *context, int where, int size, u32 value)
{
	memcpy((u8 *)context + where, &value, (size_t)size);
	return 0;
}

static void first_release(void *data)
{
	(void)data;
	first_releases++;
}

static int first_probe(struct pci_dev *dev, const struct pci_device_id *id)
{
	(void)id;
	first_probes++;
	assert(!devm_add_action(&dev->dev, first_release, NULL));
	return -ENODEV;
}

static int second_probe(struct pci_dev *dev, const struct pci_device_id *id)
{
	(void)dev, (void)id;
	/* The first driver's setup is gone before the next probe runs. */
	assert(first_releases == first_probes);
	second_probes++;
	return 0;
}

static void test_fallback(void)
{
	static const struct pci_device_id both[] = {
		{.vendor = PCI_VENDOR_ID_INTEL, .device = 0x9a49,
		 .subvendor = (u32)PCI_ANY_ID, .subdevice = (u32)PCI_ANY_ID}, {}};
	static struct pci_driver first = {.name = "first", .id_table = both,
					  .probe = first_probe};
	static struct pci_driver second = {.name = "second", .id_table = both,
					   .probe = second_probe};
	memcpy(&fallback_config[PCI_VENDOR_ID], "\x86\x80\x49\x9a", 4);
	memcpy(&fallback_config[PCI_CLASS_REVISION], "\x01\x00\x00\x03", 4);
	struct stela_linux_pci_function function = {
		.context = fallback_config, .read_config = fallback_read,
		.write_config = fallback_write, .dma_map = test_map, .dma_unmap = test_unmap,
		.enable_msi = test_enable_msi, .disable_msi = test_disable_msi,
		.dma_limit = DMA_BIT_MASK(39), .bus = 0, .devfn = PCI_DEVFN(2, 0), .irq = 16,
	};
	/* The drivers register before the function is attached, as in the
	 * image: its publication offers it to each in turn. */
	assert(!pci_register_driver(&first) && !pci_register_driver(&second));
	struct pci_dev *dev = stela_linux_pci_attach(&function, NULL);

	assert(dev);
	assert(first_probes == 1 && first_releases == 1 && second_probes == 1);
	assert(stela_linux_pci_bound(dev) && dev->driver == &second);
	assert(!strcmp(stela_linux_pci_driver_name(dev), "second"));
	stela_linux_pci_remove_device(dev);
	pci_unregister_driver(&second);
	pci_unregister_driver(&first);
	assert(first_probes == 1 && second_probes == 1);
	/* A function no driver binds has no driver's name. */
	dev = stela_linux_pci_attach(&function, NULL);
	assert(dev && !stela_linux_pci_bound(dev) && !stela_linux_pci_driver_name(dev));
	stela_linux_pci_remove_device(dev);
}

#ifdef STELA_LINUX_PCI_HOST_FAILED_PROBE_TEST
/* Requesting an interrupt takes the line's mutex, which needs a current
 * task; this fixture has no native task lease, so its ELF link wraps current
 * to init_task. */
struct task_struct *__wrap_stela_linux_current(void);
struct task_struct *__wrap_stela_linux_current(void)
{
	return &init_task;
}

/* A GPU whose driver fails part way through its probe, beside a second GPU
 * another driver then binds: the first driver enabled the function, set it
 * to master the bus, moved its interrupt to a message and requested it,
 * mapped its register BAR (managed) and allocated DMA memory, then failed as
 * a firmware load does, freeing only its DMA memory. What it left must be
 * gone before the second GPU's driver runs, and nothing of it may remain. */
static u8 failing_config[256], second_config[256];
static unsigned int line_enables, line_disables, line_retires, device_maps, device_unmaps;
static unsigned int failing_probes, failing_handlers, second_binds;
static u8 device_memory[2 * PAGE_SIZE] __aligned(PAGE_SIZE);
/* The DMA mappings live before the first GPU's attach. */
static unsigned int mapped_before;

static int test_irq_enable(void *context, unsigned int irq, bool *unexposed)
{
	(void)context;
	assert(irq == 16 || irq == 17);
	*unexposed = false;
	line_enables++;
	return 0;
}

static int test_irq_disable(void *context, unsigned int irq)
{
	(void)context, (void)irq;
	line_disables++;
	return 0;
}

static int test_irq_retire(void *context, unsigned int irq)
{
	(void)context;
	assert(stela_linux_irq_actions_joined(irq));
	line_retires++;
	return 0;
}

static void *test_map_device(void *context, u64 physical, size_t size, bool write_combining)
{
	(void)context, (void)write_combining;
	assert(physical == BASE && size <= sizeof(device_memory));
	device_maps++;
	return device_memory;
}

static void test_unmap_device(void *context, void *address)
{
	(void)context;
	assert(address == device_memory);
	device_unmaps++;
}

static irqreturn_t failing_handler(int irq, void *dev_id)
{
	(void)irq, (void)dev_id;
	failing_handlers++;
	return IRQ_HANDLED;
}

static int failing_probe(struct pci_dev *dev, const struct pci_device_id *id)
{
	(void)id;
	failing_probes++;
	assert(!pci_enable_device(dev));
	pci_set_master(dev);
	assert(pci_alloc_irq_vectors(dev, 1, 1, PCI_IRQ_MSI) == 1 && dev->msi_enabled);
	assert(!request_irq((unsigned int)pci_irq_vector(dev, 0), failing_handler, 0, "failing", &failing_probes));
	assert(devm_ioremap_wc(&dev->dev, pci_resource_start(dev, 0), PAGE_SIZE));
	dma_addr_t address;
	void *memory = dma_alloc_coherent(&dev->dev, PAGE_SIZE, &address, GFP_KERNEL);

	assert(memory && mapped == mapped_before + 1);
	/* The firmware the driver asks for next is missing: it frees its
	 * memory and returns, leaving the rest as it was. */
	dma_free_coherent(&dev->dev, PAGE_SIZE, memory, address);
	return -ENOENT;
}

static int second_probe_beside(struct pci_dev *dev, const struct pci_device_id *id)
{
	(void)id;
	/* Nothing of the first GPU's driver is left when this one runs. */
	assert(failing_probes == 1 && mapped == mapped_before && device_maps == device_unmaps);
	assert(line_retires == line_enables && !stela_linux_irq_actions_joined(16));
	assert(!(*(u16 *)&failing_config[PCI_COMMAND] & PCI_COMMAND_MASTER));
	assert(!on_message);
	second_binds++;
	assert(!pci_enable_device(dev));
	assert(!request_irq(dev->irq, failing_handler, 0, "second", &second_binds));
	free_irq(dev->irq, &second_binds);
	pci_disable_device(dev);
	return 0;
}

static void test_failed_probe_leaves_nothing(void)
{
	static const struct pci_device_id first_table[] = {
		{.vendor = PCI_VENDOR_ID_NVIDIA, .device = 0x2c02,
		 .subvendor = (u32)PCI_ANY_ID, .subdevice = (u32)PCI_ANY_ID}, {}};
	static const struct pci_device_id second_table[] = {
		{.vendor = PCI_VENDOR_ID_INTEL, .device = 0xa780,
		 .subvendor = (u32)PCI_ANY_ID, .subdevice = (u32)PCI_ANY_ID}, {}};
	static struct pci_driver failing = {.name = "failing", .id_table = first_table,
					    .probe = failing_probe};
	static struct pci_driver second = {.name = "second", .id_table = second_table,
					   .probe = second_probe_beside};
	memcpy(&failing_config[PCI_VENDOR_ID], "\xde\x10\x02\x2c", 4);
	memcpy(&failing_config[PCI_CLASS_REVISION], "\xa1\x00\x00\x03", 4);
	memcpy(&second_config[PCI_VENDOR_ID], "\x86\x80\x80\xa7", 4);
	memcpy(&second_config[PCI_CLASS_REVISION], "\x04\x00\x00\x03", 4);
	struct stela_linux_pci_function function = {
		.context = failing_config, .read_config = fallback_read,
		.write_config = fallback_write, .dma_map = test_map, .dma_unmap = test_unmap,
		.enable_msi = test_enable_msi, .disable_msi = test_disable_msi,
		.dma_limit = DMA_BIT_MASK(40), .bus = 1, .devfn = PCI_DEVFN(0, 0), .irq = 16,
	};
	function.regions[0] = (struct stela_linux_pci_region){BASE, PAGE_SIZE, IORESOURCE_MEM};
	mapped_before = mapped;
	assert(!pci_register_driver(&failing) && !pci_register_driver(&second));
	struct pci_dev *first = stela_linux_pci_attach(&function, NULL);

	assert(first && !stela_linux_pci_bound(first) && failing_probes == 1);
	/* The interrupt the driver left requested was freed, never delivered. */
	assert(line_enables == 1 && line_disables == 1 && line_retires == 1 && !failing_handlers);
	assert(!first->msi_enabled && !on_message && !first->is_busmaster && mapped == mapped_before);
	assert(device_maps == 1 && device_unmaps == 1);
	assert(*(u16 *)&failing_config[PCI_COMMAND] == PCI_COMMAND_MEMORY);
	/* Its line is free for a request of its own again. */
	assert(!request_irq(16, failing_handler, 0, "again", &failing_handlers));
	free_irq(16, &failing_handlers);
	function.context = second_config;
	function.devfn = PCI_DEVFN(2, 0);
	function.bus = 0;
	function.irq = 17;
	struct pci_dev *other = stela_linux_pci_attach(&function, NULL);

	assert(other && stela_linux_pci_bound(other) && second_binds == 1);
	assert(!strcmp(stela_linux_pci_driver_name(other), "second"));
	stela_linux_pci_remove_device(other);
	stela_linux_pci_remove_device(first);
	pci_unregister_driver(&second);
	pci_unregister_driver(&failing);
}

/* A GPU whose driver bound it and is unloaded while Linux runs on, its
 * display unused: its remove stops its hardware but leaves its interrupt
 * requested, as a remove that leaves it to its managed release does. The
 * release unbinds it and stops what the remove left. */
static unsigned int unloaded_removes;

static int unloaded_probe(struct pci_dev *dev, const struct pci_device_id *id)
{
	(void)id;
	assert(!pci_enable_device(dev));
	pci_set_master(dev);
	assert(pci_alloc_irq_vectors(dev, 1, 1, PCI_IRQ_MSI) == 1 && dev->msi_enabled);
	assert(!request_irq((unsigned int)pci_irq_vector(dev, 0), failing_handler, 0, "unloaded",
			    &unloaded_removes));
	assert(devm_ioremap_wc(&dev->dev, pci_resource_start(dev, 0), PAGE_SIZE));
	return 0;
}

static void unloaded_remove(struct pci_dev *dev)
{
	(void)dev;
	unloaded_removes++;
}

static void test_release_leaves_nothing(void)
{
	static const struct pci_device_id table[] = {
		{.vendor = PCI_VENDOR_ID_NVIDIA, .device = 0x2c02,
		 .subvendor = (u32)PCI_ANY_ID, .subdevice = (u32)PCI_ANY_ID}, {}};
	static struct pci_driver unloaded = {.name = "unloaded", .id_table = table,
					     .probe = unloaded_probe, .remove = unloaded_remove};
	unsigned int enables = line_enables, retires = line_retires, maps = device_maps;

	memset(failing_config, 0, sizeof(failing_config));
	memcpy(&failing_config[PCI_VENDOR_ID], "\xde\x10\x02\x2c", 4);
	memcpy(&failing_config[PCI_CLASS_REVISION], "\xa1\x00\x00\x03", 4);
	struct stela_linux_pci_function function = {
		.context = failing_config, .read_config = fallback_read,
		.write_config = fallback_write, .dma_map = test_map, .dma_unmap = test_unmap,
		.enable_msi = test_enable_msi, .disable_msi = test_disable_msi,
		.dma_limit = DMA_BIT_MASK(40), .bus = 1, .devfn = PCI_DEVFN(0, 0), .irq = 16,
	};
	function.regions[0] = (struct stela_linux_pci_region){BASE, PAGE_SIZE, IORESOURCE_MEM};
	assert(!pci_register_driver(&unloaded));
	struct pci_dev *dev = stela_linux_pci_attach(&function, NULL);

	assert(dev && stela_linux_pci_bound(dev) && dev->msi_enabled && dev->is_busmaster);
	assert(line_enables == enables + 1 && device_maps == maps + 1);
	assert(stela_linux_pci_release(dev));
	assert(unloaded_removes == 1 && !stela_linux_pci_bound(dev));
	assert(!stela_linux_pci_driver_name(dev));
	assert(line_retires == retires + 1 && !stela_linux_irq_actions_joined(16));
	assert(!dev->msi_enabled && !on_message && !dev->is_busmaster);
	assert(!(*(u16 *)&failing_config[PCI_COMMAND] & PCI_COMMAND_MASTER));
	assert(device_unmaps == device_maps);
	/* A function no driver holds has nothing to release. */
	assert(!stela_linux_pci_release(dev) && unloaded_removes == 1);
	/* Its line is free again, and the function's removal later finds no
	 * driver to call. */
	assert(!request_irq(16, failing_handler, 0, "again", &failing_handlers));
	free_irq(16, &failing_handlers);
	stela_linux_pci_remove_device(dev);
	assert(unloaded_removes == 1);
	pci_unregister_driver(&unloaded);
}

static const struct stela_linux_services test_services = {
	.unsupported = test_unsupported,
	.map_device = test_map_device,
	.unmap_device = test_unmap_device,
	.irq_enable = test_irq_enable,
	.irq_disable = test_irq_disable,
	.irq_retire = test_irq_retire,
};
#else
static const struct stela_linux_services test_services = {.unsupported = test_unsupported};
#endif
static const struct stela_linux_owner test_owner = {.services = &test_services};

int main(void)
{
	stela_linux_enter(&test_owner);
	stela_test_register_pages();
	memcpy(&config[PCI_VENDOR_ID], "\xde\x10\x07\x2d", 4);
	memcpy(&config[PCI_CLASS_REVISION], "\xa1\x00\x00\x03", 4);
	memcpy(&config[PCI_SUBSYSTEM_VENDOR_ID], "\x43\x10\x21\x87", 4);
	struct stela_linux_pci_function function = {
		.context = config, .read_config = test_read, .write_config = test_write,
		.dma_map = test_map, .dma_unmap = test_unmap,
		.enable_msi = test_enable_msi, .disable_msi = test_disable_msi,
		.dma_limit = DMA_BIT_MASK(40), .bus = 3, .devfn = PCI_DEVFN(1, 0), .irq = 9,
		.release_context = test_release_context,
	};
	function.regions[0] = (struct stela_linux_pci_region){BASE, 0x1000000, IORESOURCE_MEM};
    int refused_status = 0;
    config_error = PCIBIOS_SET_FAILED;
    assert(!stela_linux_pci_attach(&function, &refused_status));
    assert(refused_status == -EIO && !context_releases);
    config_error = 0;
	struct pci_dev *dev = stela_linux_pci_attach(&function, NULL);

	assert(dev && !stela_linux_pci_bound(dev));
	test_identity();
	test_upstream(function);

	/* The first step that ended the firmware's picture is kept, by function. */
	assert(!stela_linux_pci_screen_ended_at(3, PCI_DEVFN(1, 0)));
	stela_linux_pci_screen_ended(dev, "MODE2 reset");
	stela_linux_pci_screen_ended(dev, "start of the display engine");
	assert(!strcmp(stela_linux_pci_screen_ended_at(3, PCI_DEVFN(1, 0)), "MODE2 reset"));
	assert(!stela_linux_pci_screen_ended_at(3, PCI_DEVFN(2, 0)));
	assert(!stela_linux_pci_screen_ended_at(4, PCI_DEVFN(1, 0)));
	test_msix();
	test_fallback();

	assert(dev && dev->vendor == 0x10de && dev->device == 0x2d07);
	assert(dev->subsystem_vendor == 0x1043 && dev->subsystem_device == 0x8721);
	assert(dev->class == 0x030000 && dev->revision == 0xa1 && dev->irq == 9);
	assert(pci_resource_start(dev, 0) == BASE && pci_resource_len(dev, 0) == 0x1000000);
	assert(!pci_resource_len(dev, 1));
	assert(!strcmp(dev_name(&dev->dev), "0000:03:01.0"));

	u32 value;
	struct pci_dev other = {.bus = dev->bus, .devfn = PCI_DEVFN(2, 0)};

	assert(pci_read_config_dword(&other, 0, &value) == PCIBIOS_DEVICE_NOT_FOUND);

	/* The DMA parameters Linux's PCI core gives every function. */
	assert(dev->dev.dma_parms == &dev->dma_parms);
	assert(dma_get_max_seg_size(&dev->dev) == 65536);
	assert(dma_get_seg_boundary(&dev->dev) == 0xffffffff);
	dma_set_max_seg_size(&dev->dev, UINT_MAX);
	assert(dma_get_max_seg_size(&dev->dev) == UINT_MAX);

	/* A mask must cover what the owner hands out. */
	assert(dma_set_mask(&dev->dev, DMA_BIT_MASK(39)) == -EIO);
	assert(!dma_set_mask(&dev->dev, DMA_BIT_MASK(47)));

	dma_addr_t address = dma_map_phys(&dev->dev, 0x2000, 0x100, DMA_TO_DEVICE, 0);

	assert(address == 0x2000 + BUS_OFFSET && mapped == 1 && !last_pageless);
	dma_unmap_phys(&dev->dev, address, 0x100, DMA_TO_DEVICE, 0);
	assert(!mapped);
	address = dma_map_phys(&dev->dev, 0x2000, 0x100, DMA_TO_DEVICE, DMA_ATTR_MMIO);
	assert(last_pageless);
	dma_unmap_phys(&dev->dev, address, 0x100, DMA_TO_DEVICE, DMA_ATTR_MMIO);

	/* A failure part way through releases what it mapped. */
	static struct scatterlist list[2];
	struct page *page = alloc_page(GFP_KERNEL);

	assert(page);
	sg_init_table(list, 2);
	sg_set_page(&list[0], page, 0x10, 0);
	sg_set_page(&list[1], page, 0x10, 0x10);
	struct sg_table table = {.sgl = list, .orig_nents = 2};

	fail_map_after = 1;
	assert(dma_map_sgtable(&dev->dev, &table, DMA_BIDIRECTIONAL, 0) == -EIO && !mapped);
	fail_map_after = ~0U;
	assert(!dma_map_sgtable(&dev->dev, &table, DMA_BIDIRECTIONAL, 0));
	assert(table.nents == 2 && mapped == 2);
	dma_unmap_sgtable(&dev->dev, &table, DMA_BIDIRECTIONAL, 0);
	assert(!mapped);

	dma_addr_t coherent;
	void *cpu = dma_alloc_coherent(&dev->dev, PAGE_SIZE, &coherent, GFP_KERNEL);

	assert(cpu && mapped == 1 && !*(u8 *)cpu && coherent > BUS_OFFSET);
	dma_free_coherent(&dev->dev, PAGE_SIZE, cpu, coherent);
	assert(!mapped);

	/* Pages for the device, as nouveau's falcon firmware asks for them. */
	dma_addr_t pages;
	void *image = dma_alloc_noncoherent(&dev->dev, PAGE_SIZE + 1, &pages, DMA_TO_DEVICE,
					    GFP_KERNEL);

	assert(image && mapped == 1 && !*(u8 *)image && pages > BUS_OFFSET);
	sg_init_one(&list[0], image, 2 * PAGE_SIZE);
	assert(sg_page(&list[0]) == virt_to_page(image));
	dma_free_noncoherent(&dev->dev, PAGE_SIZE + 1, image, pages, DMA_TO_DEVICE);
	assert(!mapped);

	msi_error = -ENOSPC;
	assert(pci_enable_msi(dev) == -ENOSPC && !dev->msi_enabled);
	msi_error = 0;
	assert(!pci_enable_msi(dev) && on_message && dev->msi_enabled && dev->irq == 9);
	pci_disable_msi(dev);
	assert(!on_message && !dev->msi_enabled);

	/* One vector: the message where MSI is allowed and granted, else the
	 * line where INTx is allowed. MSI-X alone and two vectors are refused. */
	assert(pci_alloc_irq_vectors(dev, 1, 1, PCI_IRQ_MSI) == 1 && on_message);
	assert(pci_irq_vector(dev, 0) == 9 && pci_irq_vector(dev, 1) == -EINVAL);
	pci_free_irq_vectors(dev);
	assert(!on_message && !dev->msi_enabled);
	msi_error = -ENOSPC;
	assert(pci_alloc_irq_vectors(dev, 1, 1, PCI_IRQ_MSI) == -ENOSPC);
	assert(pci_alloc_irq_vectors(dev, 1, 4, PCI_IRQ_MSI | PCI_IRQ_INTX) == 1 && !on_message);
	msi_error = 0;
	assert(!refusals);
	assert(pci_alloc_irq_vectors(dev, 1, 1, PCI_IRQ_MSIX) == -ENOSPC && !on_message);
	assert(pci_alloc_irq_vectors(dev, 2, 2, PCI_IRQ_ALL_TYPES) == -ENOSPC && !on_message);
	assert(refusals == 2);
	assert(pci_alloc_irq_vectors(dev, 0, 1, PCI_IRQ_MSI) == -ERANGE);
	assert(pci_alloc_irq_vectors(dev, 2, 1, PCI_IRQ_MSI) == -ERANGE);

	/* Removal owns only registration's reference. An escaped pci_dev keeps
	 * the copied callback table and its original context until its last put. */
	assert(pci_dev_get(dev) == dev);
	stela_linux_pci_remove_device(dev);
	assert(!context_releases);
	assert(!pci_read_config_dword(dev, PCI_VENDOR_ID, &value) && value == 0x2d0710de);
	assert(!pci_write_config_dword(dev, PCI_COMMAND, PCI_COMMAND_MASTER));
	assert(!pci_read_config_dword(dev, PCI_COMMAND, &value) && value == PCI_COMMAND_MASTER);
	pci_dev_put(dev);
	assert(context_releases == 1);
	/* A legacy caller can still retain ownership without a release callback. */
	function.release_context = NULL;
	dev = stela_linux_pci_attach(&function, NULL);
	assert(dev && !dev->rom && !dev->romlen);
	stela_linux_pci_remove_device(dev);
	assert(context_releases == 1);
	/* A platform ROM is copied at the attach: Linux finds it at pci_dev::rom
	 * through ioremap, whole or in part, and iounmap leaves the copy, which
	 * lasts until the function is released. */
	static u8 rom_image[5000];
	for (size_t i = 0; i < sizeof(rom_image); i++)
		rom_image[i] = (u8)(i * 7);
	function.rom = rom_image;
	function.rom_length = sizeof(rom_image);
	dev = stela_linux_pci_attach(&function, NULL);
	rom_image[0] = 0xff;
	assert(dev && dev->rom && dev->romlen == sizeof(rom_image));
	u8 __iomem *rom = ioremap(dev->rom, dev->romlen);
	assert(rom && readb(rom) == 0 && readb(rom + 4999) == (u8)(4999 * 7));
	assert(ioremap(dev->rom + 4096, 904) == rom + 4096);
	assert(!stela_linux_pci_rom_map(dev->rom + 4096, 905));
	assert(!stela_linux_pci_rom_map(dev->rom - 1, 2));
	iounmap(rom);
	assert(readb(rom + 1) == 7);
	phys_addr_t copy = dev->rom;
	stela_linux_pci_remove_device(dev);
	assert(!stela_linux_pci_rom_map(copy, 1) && !stela_linux_pci_rom_mapped(rom));
	function.rom = NULL;
	function.rom_length = 0;
	/* An expansion ROM BAR the owner placed is PCI_ROM_RESOURCE, which
	 * pci_enable_rom turns on at its address and pci_disable_rom off. */
	function.rom_bar = (struct stela_linux_pci_region){
		0xc0020000, 0x20000, IORESOURCE_MEM | IORESOURCE_READONLY};
	memcpy(&config[PCI_ROM_ADDRESS], "\x00\x00\x02\xc0", 4);
	dev = stela_linux_pci_attach(&function, NULL);
	assert(dev && pci_resource_start(dev, PCI_ROM_RESOURCE) == 0xc0020000);
	assert(pci_resource_len(dev, PCI_ROM_RESOURCE) == 0x20000);
	assert(!pci_enable_rom(dev));
	assert(!pci_read_config_dword(dev, PCI_ROM_ADDRESS, &value) &&
	       value == (0xc0020000 | PCI_ROM_ADDRESS_ENABLE));
	pci_disable_rom(dev);
	assert(!pci_read_config_dword(dev, PCI_ROM_ADDRESS, &value) && value == 0xc0020000);
	stela_linux_pci_remove_device(dev);
	function.rom_bar = (struct stela_linux_pci_region){0};
	memset(&config[PCI_ROM_ADDRESS], 0, 4);
	/* The test pages are never reused, so this one comes after the
	 * allocations above. */
#ifdef STELA_LINUX_PCI_HOST_FAILED_PROBE_TEST
	test_failed_probe_leaves_nothing();
	test_release_leaves_nothing();
#endif
	/* An atomic final put cannot destroy the host or callback context. */
	function.release_context = test_release_context;
	dev = stela_linux_pci_attach(&function, NULL);
	assert(dev);
	u64 before = stela_linux_pci_quarantined_functions();
	local_bh_disable();
	stela_linux_pci_remove_device(dev);
	local_bh_enable();
	assert(context_releases == 1 && stela_linux_pci_quarantined_functions() == before + 1);
	/* Quiescence under another platform does not authorize destruction of
	 * this function's callback roots through that platform's services. */
	dev = stela_linux_pci_attach(&function, NULL);
	assert(dev && pci_dev_get(dev) == dev);
	stela_linux_pci_remove_device(dev);
	const struct stela_linux_services other_services = {0};
	const struct stela_linux_owner other_owner = {.services = &other_services};
	stela_linux_exit();
	stela_linux_enter(&other_owner);
	pci_dev_put(dev);
	assert(context_releases == 1 && stela_linux_pci_quarantined_functions() == before + 2);
	stela_linux_exit();
	printf("Linux host functions passed\n");
	return 0;
}
#endif
