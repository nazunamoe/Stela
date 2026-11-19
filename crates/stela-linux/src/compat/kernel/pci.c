// SPDX-License-Identifier: GPL-2.0-only
/* Linux's PCI core. Stela's own PCI owners enumerate, power and map
 * functions; Linux drivers match against their tables and reach the function
 * through the pci_ops the owner gives its bus. */
#include <linux/bitfield.h>
#include <linux/delay.h>
#include <linux/device.h>
#include <linux/err.h>
#include <linux/io.h>
#include <linux/log2.h>
#include <linux/pci.h>
#include <linux/sizes.h>
#include <linux/slab.h>
#include <stela/device.h>
#include <stela/linux.h>
#include <stela/pci.h>
#include <stela/catalogue.h>
#include <unsupported.h>

/* An ID field matches its value or, as PCI_ANY_ID, any value. */
static bool id_matches(u32 id, u32 value)
{
	return id == (u32)PCI_ANY_ID || id == value;
}

/* The first entry before the table's all-zero terminator that matches. */
const struct pci_device_id *pci_match_id(const struct pci_device_id *ids, struct pci_dev *dev)
{
	for (; ids && (ids->vendor || ids->subvendor || ids->class_mask); ids++)
		if (id_matches(ids->vendor, dev->vendor) && id_matches(ids->device, dev->device) &&
		    id_matches(ids->subvendor, dev->subsystem_vendor) &&
		    id_matches(ids->subdevice, dev->subsystem_device) &&
		    !((ids->class ^ dev->class) & ids->class_mask))
			return ids;
	return NULL;
}

/* Configuration space, through the bus's pci_ops, following
 * drivers/pci/access.c. Linux code runs in one context at a time, so
 * Linux's pci_lock has nothing to exclude. */
#define PCI_OP_READ(size, type, length, bad)                                                     \
	int pci_bus_read_config_##size(struct pci_bus *bus, unsigned int devfn, int pos,         \
				       type *value)                                               \
	{                                                                                        \
		u32 data = 0;                                                                    \
		if ((bad) || pos < 0 || pos > PCI_CFG_SPACE_EXP_SIZE - length)                                                                         \
			return PCIBIOS_BAD_REGISTER_NUMBER;                                      \
		int error = bus->ops->read(bus, devfn, pos, length, &data);                      \
		if (error)                                                                       \
			PCI_SET_ERROR_RESPONSE(value);                                           \
		else                                                                             \
			*value = (type)data;                                                     \
		return error;                                                                    \
	}                                                                                        \
	int pci_read_config_##size(const struct pci_dev *dev, int where, type *value)            \
	{                                                                                        \
		if (pci_dev_is_disconnected(dev)) {                                              \
			PCI_SET_ERROR_RESPONSE(value);                                           \
			return PCIBIOS_DEVICE_NOT_FOUND;                                         \
		}                                                                                \
		return pci_bus_read_config_##size(dev->bus, dev->devfn, where, value);           \
	}

#define PCI_OP_WRITE(size, type, length, bad)                                                    \
	int pci_bus_write_config_##size(struct pci_bus *bus, unsigned int devfn, int pos,        \
					type value)                                               \
	{                                                                                        \
		if ((bad) || pos < 0 || pos > PCI_CFG_SPACE_EXP_SIZE - length)                                                                         \
			return PCIBIOS_BAD_REGISTER_NUMBER;                                      \
		return bus->ops->write(bus, devfn, pos, length, value);                          \
	}                                                                                        \
	int pci_write_config_##size(const struct pci_dev *dev, int where, type value)            \
	{                                                                                        \
		if (pci_dev_is_disconnected(dev))                                                \
			return PCIBIOS_DEVICE_NOT_FOUND;                                         \
		return pci_bus_write_config_##size(dev->bus, dev->devfn, where, value);          \
	}

PCI_OP_READ(byte, u8, 1, false)
PCI_OP_READ(word, u16, 2, pos & 1)
PCI_OP_READ(dword, u32, 4, pos & 3)
PCI_OP_WRITE(byte, u8, 1, false)
PCI_OP_WRITE(word, u16, 2, pos & 1)
PCI_OP_WRITE(dword, u32, 4, pos & 3)

/* The standard capability list's entry for cap, or 0, as drivers/pci/pci.c
 * finds it; the bound stops a looped list. */
int stela_linux_pci_find_capability(struct pci_dev *dev, int cap, u8 *found)
{
    u16 status, entry;
    u8 pos;
    int error;
    *found = 0;
    error = pci_read_config_word(dev, PCI_STATUS, &status);
    if (error)
        return error;
    if (!(status & PCI_STATUS_CAP_LIST))
        return PCIBIOS_SUCCESSFUL;
    switch (dev->hdr_type) {
    case PCI_HEADER_TYPE_NORMAL:
    case PCI_HEADER_TYPE_BRIDGE:
        pos = PCI_CAPABILITY_LIST;
        break;
    case PCI_HEADER_TYPE_CARDBUS:
        pos = PCI_CB_CAPABILITY_LIST;
        break;
    default:
        return PCIBIOS_SUCCESSFUL;
    }
    error = pci_read_config_byte(dev, pos, &pos);
    if (error)
        return error;
    for (unsigned int bound = 48; bound && pos >= 0x40; bound--) {
        pos &= ~3;
        error = pci_read_config_word(dev, pos, &entry);
        if (error)
            return error;
        if ((entry & 0xff) == 0xff)
            break;
        if ((entry & 0xff) == cap) {
            *found = pos;
            break;
        }
        pos = (u8)(entry >> 8);
    }
    return PCIBIOS_SUCCESSFUL;
}

u8 pci_find_capability(struct pci_dev *dev, int cap)
{
    u8 found;
    if (stela_linux_pci_find_capability(dev, cap, &found))
        stela_linux_fatal("PCI capability access failed without a result");
    return found;
}

/* The PCI Express capability's registers, following drivers/pci/access.c: a
 * register the capability's version or the port's type does not have reads
 * as zero and takes no write. */
static bool pcie_has_link(const struct pci_dev *dev)
{
	int type = pci_pcie_type(dev);

	return type == PCI_EXP_TYPE_ENDPOINT || type == PCI_EXP_TYPE_LEG_END ||
	       type == PCI_EXP_TYPE_ROOT_PORT || type == PCI_EXP_TYPE_UPSTREAM ||
	       type == PCI_EXP_TYPE_DOWNSTREAM || type == PCI_EXP_TYPE_PCI_BRIDGE ||
	       type == PCI_EXP_TYPE_PCIE_BRIDGE;
}

static bool downstream_port(const struct pci_dev *dev)
{
	int type = pci_pcie_type(dev);

	return type == PCI_EXP_TYPE_ROOT_PORT || type == PCI_EXP_TYPE_DOWNSTREAM ||
	       type == PCI_EXP_TYPE_PCIE_BRIDGE;
}

static bool pcie_register_implemented(struct pci_dev *dev, int pos)
{
	int type, version;

	if (!pci_is_pcie(dev))
		return false;
	type = pci_pcie_type(dev);
	version = pcie_caps_reg(dev) & PCI_EXP_FLAGS_VERS;
	switch (pos) {
	case PCI_EXP_FLAGS:
	case PCI_EXP_DEVCAP:
	case PCI_EXP_DEVCTL:
	case PCI_EXP_DEVSTA:
		return true;
	case PCI_EXP_LNKCAP:
	case PCI_EXP_LNKCTL:
	case PCI_EXP_LNKSTA:
		return pcie_has_link(dev);
	case PCI_EXP_SLTCAP:
	case PCI_EXP_SLTCTL:
	case PCI_EXP_SLTSTA:
		return downstream_port(dev) && (pcie_caps_reg(dev) & PCI_EXP_FLAGS_SLOT);
	case PCI_EXP_RTCTL:
	case PCI_EXP_RTCAP:
	case PCI_EXP_RTSTA:
		return type == PCI_EXP_TYPE_ROOT_PORT || type == PCI_EXP_TYPE_RC_EC;
	case PCI_EXP_DEVCAP2:
	case PCI_EXP_DEVCTL2:
		return version > 1;
	case PCI_EXP_LNKCAP2:
	case PCI_EXP_LNKCTL2:
	case PCI_EXP_LNKSTA2:
		return version > 1 && pcie_has_link(dev);
	default:
		return false;
	}
}

int pcie_capability_read_word(struct pci_dev *dev, int pos, u16 *val)
{
	int error;

	*val = 0;
	if (pos & 1)
		return PCIBIOS_BAD_REGISTER_NUMBER;
	if (pcie_register_implemented(dev, pos)) {
		error = pci_read_config_word(dev, pci_pcie_cap(dev) + pos, val);
		/* Linux's callers do not check: a failed read is zero. */
		if (error)
			*val = 0;
		return error;
	}
	/* A downstream port without a slot has its device always present. */
	if (pci_is_pcie(dev) && downstream_port(dev) && pos == PCI_EXP_SLTSTA)
		*val = PCI_EXP_SLTSTA_PDS;
	return 0;
}

int pcie_capability_read_dword(struct pci_dev *dev, int pos, u32 *val)
{
	int error;

	*val = 0;
	if (pos & 3)
		return PCIBIOS_BAD_REGISTER_NUMBER;
	if (pcie_register_implemented(dev, pos)) {
		error = pci_read_config_dword(dev, pci_pcie_cap(dev) + pos, val);
		if (error)
			*val = 0;
		return error;
	}
	if (pci_is_pcie(dev) && downstream_port(dev) && pos == PCI_EXP_SLTSTA)
		*val = PCI_EXP_SLTSTA_PDS;
	return 0;
}

int pcie_capability_write_word(struct pci_dev *dev, int pos, u16 val)
{
	if (pos & 1)
		return PCIBIOS_BAD_REGISTER_NUMBER;
	if (!pcie_register_implemented(dev, pos))
		return 0;
	return pci_write_config_word(dev, pci_pcie_cap(dev) + pos, val);
}

int pcie_capability_clear_and_set_word_unlocked(struct pci_dev *dev, int pos, u16 clear, u16 set)
{
	u16 val;
	int error = pcie_capability_read_word(dev, pos, &val);

	if (error)
		return error;
	val = (u16)((val & ~clear) | set);
	return pcie_capability_write_word(dev, pos, val);
}

/* Linux's lock keeps a link's control registers whole between its contexts;
 * Linux code runs in one context at a time here. */
int pcie_capability_clear_and_set_word_locked(struct pci_dev *dev, int pos, u16 clear, u16 set)
{
	return pcie_capability_clear_and_set_word_unlocked(dev, pos, clear, set);
}

/* What probing a PCI Express function caches, following set_pcie_port_type
 * and pcie_get_supported_speeds in drivers/pci/probe.c and pci.c. */
int stela_linux_pci_express_probe(struct pci_dev *dev)
{
	u32 lnkcap, lnkcap2;
	u16 flags;
	u8 speeds;
	int error;

	if (!dev->pcie_cap)
		return 0;
	error = pci_read_config_word(dev, dev->pcie_cap + PCI_EXP_FLAGS, &flags);
	if (!error)
		error = pci_read_config_dword(dev, dev->pcie_cap + PCI_EXP_DEVCAP, &dev->devcap);
	if (error)
		return error;
	dev->pcie_flags_reg = flags;
	dev->pcie_mpss = FIELD_GET(PCI_EXP_DEVCAP_PAYLOAD, dev->devcap) & 7;
	pcie_capability_read_dword(dev, PCI_EXP_LNKCAP2, &lnkcap2);
	pcie_capability_read_dword(dev, PCI_EXP_LNKCAP, &lnkcap);
	/* Speeds above the link's maximum do not count. */
	speeds = (u8)(lnkcap2 & PCI_EXP_LNKCAP2_SLS & GENMASK(lnkcap & PCI_EXP_LNKCAP_SLS, 0));
	/* Before PCIe r3.0 the vector is absent: the maximum implies the rest. */
	if (!speeds && (lnkcap & PCI_EXP_LNKCAP_SLS) == PCI_EXP_LNKCAP_SLS_5_0GB)
		speeds = PCI_EXP_LNKCAP2_SLS_5_0GB | PCI_EXP_LNKCAP2_SLS_2_5GB;
	else if (!speeds && (lnkcap & PCI_EXP_LNKCAP_SLS) == PCI_EXP_LNKCAP_SLS_2_5GB)
		speeds = PCI_EXP_LNKCAP2_SLS_2_5GB;
	dev->supported_speeds = speeds;
	return 0;
}

/* The highest speed of the function's link, as drivers/pci/pci.h's
 * PCIE_LNKCAP2_SLS2SPEED reads the cached vector. */
enum pci_bus_speed pcie_get_speed_cap(struct pci_dev *dev)
{
	static const struct {
		u8 bit;
		enum pci_bus_speed speed;
	} speeds[] = {
		{PCI_EXP_LNKCAP2_SLS_64_0GB, PCIE_SPEED_64_0GT},
		{PCI_EXP_LNKCAP2_SLS_32_0GB, PCIE_SPEED_32_0GT},
		{PCI_EXP_LNKCAP2_SLS_16_0GB, PCIE_SPEED_16_0GT},
		{PCI_EXP_LNKCAP2_SLS_8_0GB, PCIE_SPEED_8_0GT},
		{PCI_EXP_LNKCAP2_SLS_5_0GB, PCIE_SPEED_5_0GT},
		{PCI_EXP_LNKCAP2_SLS_2_5GB, PCIE_SPEED_2_5GT},
	};

	for (size_t i = 0; i < ARRAY_SIZE(speeds); i++)
		if (dev->supported_speeds & speeds[i].bit)
			return speeds[i].speed;
	return PCI_SPEED_UNKNOWN;
}

enum pcie_link_width pcie_get_width_cap(struct pci_dev *dev)
{
	u32 lnkcap;

	pcie_capability_read_dword(dev, PCI_EXP_LNKCAP, &lnkcap);
	if (lnkcap)
		return (enum pcie_link_width)FIELD_GET(PCI_EXP_LNKCAP_MLW, lnkcap);
	return PCIE_LNK_WIDTH_UNKNOWN;
}

/* The speed a link status's Current Link Speed field names, as Linux's
 * pcie_link_speed[] table gives it. */
static enum pci_bus_speed link_speed(u16 status)
{
	switch (status & PCI_EXP_LNKSTA_CLS) {
	case PCI_EXP_LNKSTA_CLS_2_5GB:
		return PCIE_SPEED_2_5GT;
	case PCI_EXP_LNKSTA_CLS_5_0GB:
		return PCIE_SPEED_5_0GT;
	case PCI_EXP_LNKSTA_CLS_8_0GB:
		return PCIE_SPEED_8_0GT;
	case PCI_EXP_LNKSTA_CLS_16_0GB:
		return PCIE_SPEED_16_0GT;
	case PCI_EXP_LNKSTA_CLS_32_0GB:
		return PCIE_SPEED_32_0GT;
	case PCI_EXP_LNKSTA_CLS_64_0GB:
		return PCIE_SPEED_64_0GT;
	default:
		return PCI_SPEED_UNKNOWN;
	}
}

/* A lane's data rate in Mb/s after its encoding, drivers/pci/pci.h's
 * PCIE_SPEED2MBS_ENC. */
static u32 lane_mbps(enum pci_bus_speed speed)
{
	switch (speed) {
	case PCIE_SPEED_64_0GT:
		return 64000 * 1 / 1;
	case PCIE_SPEED_32_0GT:
		return 32000 * 128 / 130;
	case PCIE_SPEED_16_0GT:
		return 16000 * 128 / 130;
	case PCIE_SPEED_8_0GT:
		return 8000 * 128 / 130;
	case PCIE_SPEED_5_0GT:
		return 5000 * 8 / 10;
	case PCIE_SPEED_2_5GT:
		return 2500 * 8 / 10;
	default:
		return 0;
	}
}

/* The bandwidth the function's path to the root complex gives it, following
 * drivers/pci/pci.c: the slowest trained link from the function up, with
 * its device, speed and width. The path is Linux's bridges, else the ports
 * the function's owner gave it (stela_linux_pci_upstream). */
u32 pcie_bandwidth_available(struct pci_dev *dev, struct pci_dev **limiting_dev,
			     enum pci_bus_speed *speed, enum pcie_link_width *width)
{
	u32 bandwidth = 0;

	if (speed)
		*speed = PCI_SPEED_UNKNOWN;
	if (width)
		*width = PCIE_LNK_WIDTH_UNKNOWN;
	for (; dev; dev = stela_linux_pci_upstream(dev)) {
		u16 status = 0;
		enum pci_bus_speed next_speed;
		enum pcie_link_width next_width;
		u32 next;

		pcie_capability_read_word(dev, PCI_EXP_LNKSTA, &status);
		next_speed = link_speed(status);
		next_width = (enum pcie_link_width)FIELD_GET(PCI_EXP_LNKSTA_NLW, status);
		next = (u32)next_width * lane_mbps(next_speed);
		if (!bandwidth || next <= bandwidth) {
			bandwidth = next;
			if (limiting_dev)
				*limiting_dev = dev;
			if (speed)
				*speed = next_speed;
			if (width)
				*width = next_width;
		}
	}
	return bandwidth;
}

/* A processor's identity from CPUID, as Linux's early identification
 * (arch/x86/kernel/cpu/common.c, lib/cpu.c) decodes it: the maker from leaf
 * 0's vendor string, and from leaf 1's signature the family, extended where
 * the base family is 0xf, and the model, extended from family 6 up. PCIe
 * host code reads it to know the root complex's maker, as Linux's does. */
static struct cpuinfo_x86 decode_cpu(const char vendor[12], u32 signature)
{
	struct cpuinfo_x86 cpu = {.x86_vendor = X86_VENDOR_UNKNOWN};
	u32 family = (signature >> 8) & 0xf;
	u32 model = (signature >> 4) & 0xf;

	if (!memcmp(vendor, "GenuineIntel", 12))
		cpu.x86_vendor = X86_VENDOR_INTEL;
	else if (!memcmp(vendor, "AuthenticAMD", 12))
		cpu.x86_vendor = X86_VENDOR_AMD;
	else if (!memcmp(vendor, "HygonGenuine", 12))
		cpu.x86_vendor = X86_VENDOR_HYGON;
	if (family == 0xf)
		family += (signature >> 20) & 0xff;
	if (family >= 6)
		model |= ((signature >> 16) & 0xf) << 4;
	cpu.x86 = (u8)family;
	cpu.x86_model = (u8)model;
	return cpu;
}

struct cpuinfo_x86 *stela_linux_cpu_data(unsigned int cpu)
{
	/* Linux code runs in one context at a time, so the first reader
	 * fills it alone. */
	static struct cpuinfo_x86 boot;
	static bool known;
	u32 leaf = 0, ebx, ecx, edx, signature;
	char vendor[12];

	(void)cpu;
	if (known)
		return &boot;
	asm volatile("cpuid" : "+a"(leaf), "=b"(ebx), "=c"(ecx), "=d"(edx) : "c"(0));
	memcpy(vendor, &ebx, 4);
	memcpy(vendor + 4, &edx, 4);
	memcpy(vendor + 8, &ecx, 4);
	signature = 1;
	asm volatile("cpuid" : "+a"(signature), "=b"(ebx), "=c"(ecx), "=d"(edx) : "c"(0));
	boot = decode_cpu(vendor, signature);
	known = true;
	return &boot;
}

int pcie_get_mps(struct pci_dev *dev)
{
	u16 ctl;

	pcie_capability_read_word(dev, PCI_EXP_DEVCTL, &ctl);
	return 128 << FIELD_GET(PCI_EXP_DEVCTL_PAYLOAD, ctl);
}

/* Atomic operation requests, following drivers/pci/pci.c: every port up to
 * the root must route them. A function's bus here has no bridge above it in
 * Linux's view, so, as Linux answers for a function without a root port, the
 * requester's own enable says whether the platform arranged the path. */
int pci_enable_atomic_ops_to_root(struct pci_dev *dev, u32 cap_mask)
{
	u32 ctl2;

	(void)cap_mask;
	if (dev->is_virtfn || !pci_is_pcie(dev))
		return -EINVAL;
	switch (pci_pcie_type(dev)) {
	case PCI_EXP_TYPE_ENDPOINT:
	case PCI_EXP_TYPE_LEG_END:
		break;
	default:
		return -EINVAL;
	}
	if (WARN_ON(pci_upstream_bridge(dev)))
		return -EINVAL;
	pcie_capability_read_dword(dev, PCI_EXP_DEVCTL2, &ctl2);
	return ctl2 & PCI_EXP_DEVCTL2_ATOMIC_REQ ? 0 : -EINVAL;
}

/* Wait for the function's pending transactions to end, with Linux's four
 * looks over 700 ms. Nonzero when none is left. */
int pci_wait_for_pending_transaction(struct pci_dev *dev)
{
	if (!pci_is_pcie(dev))
		return 1;
	for (unsigned int i = 0; i < 4; i++) {
		u16 status;

		if (i)
			msleep((1U << (i - 1)) * 100);
		if (pci_read_config_word(dev, pci_pcie_cap(dev) + PCI_EXP_DEVSTA, &status))
			return 0;
		if (!(status & PCI_EXP_DEVSTA_TRPND))
			return 1;
	}
	return 0;
}

/* The extended capability list's entry for cap, or 0, as drivers/pci/pci.c
 * finds it. A function whose owner reaches only the first 256 bytes fails
 * the first read and has none. */
u16 pci_find_ext_capability(struct pci_dev *dev, int cap)
{
	u16 pos = PCI_CFG_SPACE_SIZE;
	u32 header;

	if (pci_read_config_dword(dev, pos, &header) || !header || header == ~0U)
		return 0;
	for (int bound = (PCI_CFG_SPACE_EXP_SIZE - PCI_CFG_SPACE_SIZE) / 8; bound > 0; bound--) {
		if (PCI_EXT_CAP_ID(header) == (u32)cap)
			return pos;
		pos = PCI_EXT_CAP_NEXT(header);
		if (pos < PCI_CFG_SPACE_SIZE || pci_read_config_dword(dev, pos, &header))
			break;
	}
	return 0;
}

/* Resizable BARs, following drivers/pci/rebar.c. What a function offers is
 * read from it. A resize moves bridge windows, which belong to the firmware
 * here: a bus lists none, so a driver that looks for room above 4 GiB finds
 * none and keeps the size the firmware gave, and that is said once. */
#define PCI_REBAR_MIN_SIZE ((resource_size_t)SZ_1M)

int pci_rebar_bytes_to_size(u64 bytes)
{
	int minimum = ilog2(PCI_REBAR_MIN_SIZE);

	bytes = roundup_pow_of_two(bytes);
	return max(ilog2(bytes), minimum) - minimum;
}

resource_size_t pci_rebar_size_to_bytes(int size)
{
	return 1ULL << (size + ilog2(PCI_REBAR_MIN_SIZE));
}

u64 pci_rebar_get_possible_sizes(struct pci_dev *pdev, int bar)
{
	unsigned int pos = pci_find_ext_capability(pdev, PCI_EXT_CAP_ID_REBAR);
	unsigned int count;
	u32 control, sizes;

	if (!pos || bar < 0 || bar >= PCI_STD_NUM_BARS ||
	    pci_read_config_dword(pdev, (int)pos + PCI_REBAR_CTRL, &control))
		return 0;
	count = (control & PCI_REBAR_CTRL_NBAR_MASK) >> PCI_REBAR_CTRL_NBAR_SHIFT;
	for (unsigned int i = 0; i < count; i++, pos += 8) {
		if (pci_read_config_dword(pdev, (int)pos + PCI_REBAR_CTRL, &control))
			return 0;
		if ((control & PCI_REBAR_CTRL_BAR_IDX) != (u32)bar)
			continue;
		if (pci_read_config_dword(pdev, (int)pos + PCI_REBAR_CAP, &sizes))
			return 0;
		sizes = (sizes & PCI_REBAR_CAP_SIZES) >> 4;
		/* Linux's correction of one card's invalid capability. */
		if (pdev->vendor == PCI_VENDOR_ID_ATI && pdev->device == 0x731f && bar == 0 &&
		    sizes == 0x700)
			return 0x3f00;
		return sizes;
	}
	return 0;
}

bool pci_rebar_size_supported(struct pci_dev *pdev, int bar, int size)
{
	u64 sizes = pci_rebar_get_possible_sizes(pdev, bar);

	if (size < 0 || size > ilog2(SZ_128T) - ilog2(PCI_REBAR_MIN_SIZE))
		return false;
	return BIT_ULL(size) & sizes;
}

int pci_rebar_get_max_size(struct pci_dev *pdev, int bar)
{
	u64 sizes = pci_rebar_get_possible_sizes(pdev, bar);

	return sizes ? (int)__fls(sizes) : -ENOENT;
}

struct resource *pci_bus_resource_n(const struct pci_bus *bus, int n)
{
	(void)bus;
	(void)n;
	stela_linux_unsupported("a PCI bus's windows: BARs keep the sizes the firmware gave");
	return NULL;
}

/* Linux's pci_resize_resource asks first whether the host bridge must keep
 * the firmware's resource assignment (preserve_config) and answers -ENOTSUPP
 * when it must. Every bridge here must: no bus has a window to grow into, so
 * a BAR keeps the size the firmware gave. */
int pci_resize_resource(struct pci_dev *dev, int resno, int size, int exclude_bars)
{
	(void)dev;
	(void)resno;
	(void)size;
	(void)exclude_bars;
	stela_linux_unsupported("resizing a PCI BAR: BARs keep the sizes the firmware gave");
	return -ENOTSUPP;
}

/* drivers/pci/bus.c's: a resource placed in a free part of the bus's
 * windows. No bus has a window here (pci_bus_resource_n), so a driver that
 * places a resource the firmware left unplaced goes without it: i915 then
 * leaves MCHBAR off, as when Linux finds no room. */
int pci_bus_alloc_resource(struct pci_bus *bus, struct resource *res, resource_size_t size,
			   resource_size_t align, resource_size_t min, unsigned long type_mask,
			   resource_alignf alignf, void *alignf_data)
{
	(void)bus, (void)res, (void)size, (void)align, (void)min, (void)type_mask;
	(void)alignf, (void)alignf_data;
	stela_linux_unsupported("placing a resource the firmware left unplaced in a PCI bus window: "
				"the driver goes without it");
	return -EBUSY;
}

/* The end of the legacy BIOS area (arch/x86/include/asm/e820/types.h). */
#define BIOS_END 0x00100000

/* arch/x86/pci/i386.c's alignment of a resource placed in a bus window,
 * which pci_bus_alloc_resource asks for; it places none. */
resource_size_t pcibios_align_resource(void *data, const struct resource *res,
				       const struct resource *empty_res, resource_size_t size,
				       resource_size_t align)
{
	resource_size_t start = res->start;

	(void)data, (void)empty_res, (void)size, (void)align;
	if ((res->flags & IORESOURCE_IO) && (start & 0x300))
		start = (start + 0x3ff) & ~(resource_size_t)0x3ff;
	else if ((res->flags & IORESOURCE_MEM) && start < BIOS_END)
		start = BIOS_END;
	return start;
}

/* What a BAR's resize released, assigned again. pci_resize_resource refuses
 * every resize, so each BAR keeps the firmware's assignment and nothing is
 * unassigned: there is nothing to do. */
void pci_assign_unassigned_bus_resources(struct pci_bus *bus)
{
	(void)bus;
}

const struct bus_type pci_bus_type = {.name = "pci"};

/* The devices their owners registered and the drivers bound to them, as
 * drivers/pci/pci-driver.c matches and probes. */
struct registered_driver {
	struct list_head node;
	struct pci_driver *driver;
};

static LIST_HEAD(devices);
static LIST_HEAD(drivers);

/* drivers/pci/pci.h: the bit of priv_flags pci_bus_add_device sets on a device
 * drivers may match, which pci_bus_match asks for first. */
#define PCI_DEV_ALLOW_BINDING 7

static void bind(struct pci_driver *driver, struct pci_dev *dev)
{
	const struct pci_device_id *id = pci_match_id(driver->id_table, dev);

	if (dev->driver || !id || !test_bit(PCI_DEV_ALLOW_BINDING, &dev->priv_flags))
		return;
	dev->driver = driver;
	dev->dev.driver = &driver->driver;
	int error = driver->probe(dev, id);

	if (!error)
		return;
	if (error != -ENODEV)
		dev_err(&dev->dev, "%s: probe failed: %pe\n", driver->name, ERR_PTR(error));
	stela_linux_devres_release(&dev->dev);
	/* What the driver's own error path left running is stopped before the
	 * next driver is offered the function. */
	stela_linux_pci_quiesce_failed(dev, driver->name);
	dev->dev.driver = NULL;
	dev->driver = NULL;
}

static void unbind(struct pci_dev *dev)
{
	struct pci_driver *driver = dev->driver;

	if (!driver)
		return;
	if (driver->remove)
		driver->remove(dev);
	stela_linux_devres_release(&dev->dev);
	dev->dev.driver = NULL;
	dev->driver = NULL;
}

/* drivers/base/dd.c's device_release_driver for one function while Linux
 * runs on: its driver's remove, then its managed resources; what the remove
 * left (interrupt requests, MSI, bus mastering) is stopped as after a failed
 * probe. The function stays published and unbound. Whether a driver held
 * it. */
bool stela_linux_pci_release(struct pci_dev *dev)
{
	const char *driver = dev->driver ? dev->driver->name : NULL;

	if (!driver)
		return false;
	unbind(dev);
	stela_linux_pci_quiesce(dev, driver, "unload");
	return true;
}

bool stela_linux_pci_devices_present(void)
{
	return !list_empty(&devices);
}

int stela_linux_pci_publish_device(struct pci_dev *dev)
{
	struct registered_driver *registered;
	if (!stela_linux_device_publish_allowed(&dev->dev))
		return -EPERM;

	dev->dev.bus = &pci_bus_type;
	set_bit(PCI_DEV_ALLOW_BINDING, &dev->priv_flags);
	list_add_tail(&dev->bus_list, &devices);
	list_for_each_entry(registered, &drivers, node)
		bind(registered->driver, dev);
	return 0;
}

/* A function no driver may bind, for the searches of other functions' drivers
 * (pci_get_class, pci_dev_present, pci_get_domain_bus_and_slot): it never
 * allows binding, so it needs and is given no enrolled authority. */
int stela_linux_pci_publish_for_searches(struct pci_dev *dev)
{
	if (stela_linux_kernel_hosted())
		return -EPERM;
	dev->dev.bus = &pci_bus_type;
	list_add_tail(&dev->bus_list, &devices);
	return 0;
}

bool stela_linux_pci_bound(const struct pci_dev *device)
{
	return device->dev.driver;
}

/* The name of the driver bound to the device, which is the first registered
 * driver whose table holds it and whose probe succeeded, or NULL. */
const char *stela_linux_pci_driver_name(const struct pci_dev *device)
{
	return device->driver ? device->driver->name : NULL;
}

/* What the kernel's shutdown does of a device: its driver stops the hardware. */
void stela_linux_pci_shutdown(struct pci_dev *dev)
{
	if (dev->driver && dev->driver->shutdown)
		dev->driver->shutdown(dev);
}

void stela_linux_pci_remove_device(struct pci_dev *dev)
{
	stela_linux_device_close(&dev->dev);
	unbind(dev);
	list_del(&dev->bus_list);
	put_device(&dev->dev);
}

int __pci_register_driver(struct pci_driver *driver, struct module *owner, const char *name)
{
	struct registered_driver *registered = kmalloc_obj(*registered);
	struct pci_dev *dev;

	(void)owner;
	if (!registered)
		return -ENOMEM;
	driver->driver.name = driver->name;
	driver->driver.mod_name = name;
	registered->driver = driver;
	list_add_tail(&registered->node, &drivers);
	list_for_each_entry(dev, &devices, bus_list)
		bind(driver, dev);
	return 0;
}

void pci_unregister_driver(struct pci_driver *driver)
{
	struct registered_driver *registered;
	struct pci_dev *dev;

	list_for_each_entry(registered, &drivers, node)
		if (registered->driver == driver) {
			list_del(&registered->node);
			kfree(registered);
			break;
		}
	list_for_each_entry(dev, &devices, bus_list)
		if (dev->driver == driver)
			unbind(dev);
}

/* Owners keep their bridges' hotplug slots out of a driver's power cycles. */
void pci_ignore_hotplug(struct pci_dev *dev)
{
	dev->ignore_hotplug = 1;
}

/* A capability's registers pci_save_state keeps, following drivers/pci/pci.h:
 * the PCI Express capability's control registers, whose buffer the first
 * save allocates (Linux's pci_allocate_cap_save_buffers allocates it when it
 * adds the function) and the function's release frees. */
struct pci_cap_saved_data {
	u16 cap_nr;
	bool cap_extended;
	unsigned int size;
	u32 data[];
};

struct pci_cap_saved_state {
	struct hlist_node next;
	struct pci_cap_saved_data cap;
};

/* The control registers a reset clears, in pci_save_pcie_state's order:
 * device, link, slot and root control, and the device, link and slot
 * controls of version 2. Absent registers save as zero and are not
 * written back (pcie_capability_write_word). */
static const int pcie_saved_registers[] = {
	PCI_EXP_DEVCTL, PCI_EXP_LNKCTL, PCI_EXP_SLTCTL, PCI_EXP_RTCTL,
	PCI_EXP_DEVCTL2, PCI_EXP_LNKCTL2, PCI_EXP_SLTCTL2,
};
/* Rounded up to whole words, so a stored copy's next capability stays
 * aligned. */
#define PCIE_SAVED_BYTES ALIGN(ARRAY_SIZE(pcie_saved_registers) * sizeof(u16), sizeof(u32))

static struct pci_cap_saved_state *saved_cap(struct pci_dev *dev, u16 cap, bool extended)
{
	struct pci_cap_saved_state *state;

	hlist_for_each_entry(state, &dev->saved_cap_space, next)
		if (state->cap.cap_extended == extended && state->cap.cap_nr == cap)
			return state;
	return NULL;
}

static int save_pcie_state(struct pci_dev *dev)
{
	struct pci_cap_saved_state *state;
	u16 *words;

	if (!pci_is_pcie(dev))
		return 0;
	state = saved_cap(dev, PCI_CAP_ID_EXP, false);
	if (!state) {
		state = kzalloc(sizeof(*state) + PCIE_SAVED_BYTES, GFP_KERNEL);
		if (!state) {
			pci_err(dev, "buffer not found in %s\n", __func__);
			return -ENOMEM;
		}
		state->cap.cap_nr = PCI_CAP_ID_EXP;
		state->cap.size = PCIE_SAVED_BYTES;
		hlist_add_head(&state->next, &dev->saved_cap_space);
	}
	words = (u16 *)state->cap.data;
	for (size_t i = 0; i < ARRAY_SIZE(pcie_saved_registers); i++)
		pcie_capability_read_word(dev, pcie_saved_registers[i], &words[i]);
	return 0;
}

static void restore_pcie_state(struct pci_dev *dev)
{
	struct pci_cap_saved_state *state = saved_cap(dev, PCI_CAP_ID_EXP, false);
	const u16 *words;

	if (!state)
		return;
	words = (const u16 *)state->cap.data;
	for (size_t i = 0; i < ARRAY_SIZE(pcie_saved_registers); i++)
		pcie_capability_write_word(dev, pcie_saved_registers[i], words[i]);
}

/* A function's message, which a reset of the whole function clears.
 * Linux's pci_restore_msi_state writes it back from the message it keeps
 * (__pci_restore_msi_state); here the owner programmed it, so the words the
 * owner wrote are kept: the control, the address, the data and the mask. */
enum { MSI_CONTROL, MSI_ADDRESS_LO, MSI_ADDRESS_HI, MSI_DATA, MSI_MASK, MSI_SAVED_WORDS };

static struct pci_cap_saved_state *msi_buffer(struct pci_dev *dev, u16 cap, size_t bytes)
{
	struct pci_cap_saved_state *state = saved_cap(dev, cap, false);

	if (state)
		return state;
	state = kzalloc(sizeof(*state) + bytes, GFP_KERNEL);
	if (!state) {
		pci_err(dev, "buffer not found in %s\n", __func__);
		return NULL;
	}
	state->cap.cap_nr = cap;
	state->cap.size = (unsigned int)bytes;
	hlist_add_head(&state->next, &dev->saved_cap_space);
	return state;
}

static int save_msi_state(struct pci_dev *dev)
{
	u8 pos = pci_find_capability(dev, PCI_CAP_ID_MSI);
	struct pci_cap_saved_state *state;
	u32 *words;
	u16 control;

	if (!pos)
		return 0;
	state = msi_buffer(dev, PCI_CAP_ID_MSI, MSI_SAVED_WORDS * sizeof(u32));
	if (!state)
		return -ENOMEM;
	words = state->cap.data;
	memset(words, 0, MSI_SAVED_WORDS * sizeof(u32));
	if (pci_read_config_word(dev, pos + PCI_MSI_FLAGS, &control))
		return -EIO;
	words[MSI_CONTROL] = control;
	pci_read_config_dword(dev, pos + PCI_MSI_ADDRESS_LO, &words[MSI_ADDRESS_LO]);
	if (control & PCI_MSI_FLAGS_64BIT) {
		u16 data;

		pci_read_config_dword(dev, pos + PCI_MSI_ADDRESS_HI, &words[MSI_ADDRESS_HI]);
		pci_read_config_word(dev, pos + PCI_MSI_DATA_64, &data);
		words[MSI_DATA] = data;
		if (control & PCI_MSI_FLAGS_MASKBIT)
			pci_read_config_dword(dev, pos + PCI_MSI_MASK_64, &words[MSI_MASK]);
	} else {
		u16 data;

		pci_read_config_word(dev, pos + PCI_MSI_DATA_32, &data);
		words[MSI_DATA] = data;
		if (control & PCI_MSI_FLAGS_MASKBIT)
			pci_read_config_dword(dev, pos + PCI_MSI_MASK_32, &words[MSI_MASK]);
	}
	return 0;
}

/* In __pci_restore_msi_state's order: the message off, its address, data
 * and mask, then the control word the owner left, with its enable. Only a
 * function whose message is on, as Linux restores only then. */
static void restore_msi_state(struct pci_dev *dev)
{
	struct pci_cap_saved_state *state = saved_cap(dev, PCI_CAP_ID_MSI, false);
	u8 pos = pci_find_capability(dev, PCI_CAP_ID_MSI);
	const u32 *words;
	u16 control;
	bool wide;

	if (!dev->msi_enabled || !state || !pos)
		return;
	words = state->cap.data;
	control = (u16)words[MSI_CONTROL];
	wide = control & PCI_MSI_FLAGS_64BIT;
	pci_write_config_word(dev, pos + PCI_MSI_FLAGS, control & (u16)~PCI_MSI_FLAGS_ENABLE);
	pci_write_config_dword(dev, pos + PCI_MSI_ADDRESS_LO, words[MSI_ADDRESS_LO]);
	if (wide)
		pci_write_config_dword(dev, pos + PCI_MSI_ADDRESS_HI, words[MSI_ADDRESS_HI]);
	pci_write_config_word(dev, pos + (wide ? PCI_MSI_DATA_64 : PCI_MSI_DATA_32),
			      (u16)words[MSI_DATA]);
	if (control & PCI_MSI_FLAGS_MASKBIT)
		pci_write_config_dword(dev, pos + (wide ? PCI_MSI_MASK_64 : PCI_MSI_MASK_32),
				       words[MSI_MASK]);
	pci_write_config_word(dev, pos + PCI_MSI_FLAGS, control);
}

/* Each resizable BAR set back to the size its resource has, following
 * pci_restore_rebar_state: a reset of the whole function returns them to
 * their defaults, while the resources (and the driver's mappings) keep the
 * size the firmware chose. A BAR without a memory resource is left as it
 * is: its size here would be no assignment's. */
static void restore_rebar_state(struct pci_dev *dev)
{
	unsigned int pos = pci_find_ext_capability(dev, PCI_EXT_CAP_ID_REBAR);
	unsigned int count;
	u32 ctrl;

	if (!pos || pci_read_config_dword(dev, (int)pos + PCI_REBAR_CTRL, &ctrl) ||
	    PCI_POSSIBLE_ERROR(ctrl))
		return;
	count = (ctrl & PCI_REBAR_CTRL_NBAR_MASK) >> PCI_REBAR_CTRL_NBAR_SHIFT;
	for (unsigned int i = 0; i < count; i++, pos += 8) {
		const struct resource *res;
		unsigned int bar;

		if (pci_read_config_dword(dev, (int)pos + PCI_REBAR_CTRL, &ctrl) ||
		    PCI_POSSIBLE_ERROR(ctrl))
			return;
		bar = ctrl & PCI_REBAR_CTRL_BAR_IDX;
		res = &dev->resource[bar];
		if (bar >= PCI_STD_NUM_BARS || !(res->flags & IORESOURCE_MEM) || !resource_size(res))
			continue;
		ctrl &= ~(u32)PCI_REBAR_CTRL_BAR_SIZE;
		ctrl |= (u32)pci_rebar_bytes_to_size(resource_size(res)) << PCI_REBAR_CTRL_BAR_SHIFT;
		pci_write_config_dword(dev, (int)pos + PCI_REBAR_CTRL, ctrl);
	}
}

/* What a function loses in D3 or a reset of its own, following
 * drivers/pci/pci.c: its standard header, its PCI Express controls and its
 * message. */
int pci_save_state(struct pci_dev *dev)
{
    enum { SAVED_DWORDS = 16 };
    u32 saved[SAVED_DWORDS];
    int error;
    for (int i = 0; i < SAVED_DWORDS; i++) {
        error = pci_read_config_dword(dev, i * 4, &saved[i]);
        if (error)
            return pcibios_err_to_errno(error);
    }
    memcpy(dev->saved_config_space, saved, sizeof(saved));
    dev->state_saved = true;
    error = save_pcie_state(dev);
    return error ? error : save_msi_state(dev);
}

/* In pci_restore_state's order: the PCI Express controls and the resizable
 * BARs' sizes before the header that turns decoding back on, the message
 * after it. */
void pci_restore_state(struct pci_dev *dev)
{
    if (!dev->state_saved)
        return;
    restore_pcie_state(dev);
    restore_rebar_state(dev);
    for (int i = 15; i >= 0; i--)
        if (pci_write_config_dword(dev, i * 4, dev->saved_config_space[i]))
            stela_linux_fatal("PCI state restore failed without completion");
    restore_msi_state(dev);
    dev->state_saved = false;
}

void stela_linux_pci_free_saved_state(struct pci_dev *dev)
{
	struct pci_cap_saved_state *state;
	struct hlist_node *next;

	hlist_for_each_entry_safe(state, next, &dev->saved_cap_space, next) {
		hlist_del(&state->next);
		kfree(state);
	}
}

/* A copy of the saved state a driver keeps across resets, following
 * drivers/pci/pci.c: the header, then each saved capability, then an empty
 * one. */
struct pci_saved_state {
	u32 config_space[16];
	struct pci_cap_saved_data cap[];
};

struct pci_saved_state *pci_store_saved_state(struct pci_dev *dev)
{
	struct pci_cap_saved_state *saved;
	struct pci_saved_state *state;
	struct pci_cap_saved_data *cap;
	size_t size = sizeof(*state) + sizeof(struct pci_cap_saved_data);

	if (!dev->state_saved)
		return NULL;
	hlist_for_each_entry(saved, &dev->saved_cap_space, next)
		size += sizeof(struct pci_cap_saved_data) + saved->cap.size;
	state = kzalloc(size, GFP_KERNEL);
	if (!state)
		return NULL;
	memcpy(state->config_space, dev->saved_config_space, sizeof(state->config_space));
	cap = state->cap;
	hlist_for_each_entry(saved, &dev->saved_cap_space, next) {
		size_t length = sizeof(struct pci_cap_saved_data) + saved->cap.size;

		memcpy(cap, &saved->cap, length);
		cap = (struct pci_cap_saved_data *)((u8 *)cap + length);
	}
	return state;
}

int pci_load_saved_state(struct pci_dev *dev, struct pci_saved_state *state)
{
	struct pci_cap_saved_data *cap;

	dev->state_saved = false;
	if (!state)
		return 0;
	memcpy(dev->saved_config_space, state->config_space, sizeof(state->config_space));
	/* A capability this function never saved cannot be loaded. */
	for (cap = state->cap; cap->size;
	     cap = (struct pci_cap_saved_data *)((u8 *)cap + sizeof(*cap) + cap->size)) {
		struct pci_cap_saved_state *saved = saved_cap(dev, cap->cap_nr, cap->cap_extended);

		if (!saved || saved->cap.size != cap->size)
			return -EINVAL;
		memcpy(saved->cap.data, cap->data, saved->cap.size);
	}
	dev->state_saved = true;
	return 0;
}

/* Wait until the function answers configuration reads again after a reset,
 * following pci_dev_wait: a function not ready reads all ones, and Linux
 * gives it PCIE_RESET_READY_POLL_MS, a minute. */
static int reset_wait(struct pci_dev *dev)
{
	unsigned int delay = 1, waited = 0;
	u32 id;

	for (;;) {
		if (!pci_read_config_dword(dev, PCI_COMMAND, &id) && !PCI_POSSIBLE_ERROR(id))
			return 0;
		if (waited > 60000) {
			pci_warn(dev, "not ready %ums after reset; giving up\n", waited);
			return -ENOTTY;
		}
		msleep(delay);
		waited += delay;
		delay *= 2;
	}
}

/* Function-level reset, following pcie_flr. */
static int reset_flr(struct pci_dev *dev)
{
	if (!pci_is_pcie(dev) || !(dev->devcap & PCI_EXP_DEVCAP_FLR) ||
	    (dev->dev_flags & PCI_DEV_FLAGS_NO_FLR_RESET))
		return -ENOTTY;
	if (!pci_wait_for_pending_transaction(dev))
		pci_err(dev, "timed out waiting for pending transaction; performing function level reset anyway\n");
	pcie_capability_set_word(dev, PCI_EXP_DEVCTL, PCI_EXP_DEVCTL_BCR_FLR);
	msleep(100);
	return reset_wait(dev);
}

/* Reset through D3hot, following pci_pm_reset: only a function whose power
 * management capability says the transition resets it. */
static int reset_power(struct pci_dev *dev)
{
	u8 pm = pci_find_capability(dev, PCI_CAP_ID_PM);
	u16 control;

	if (!pm || (dev->dev_flags & PCI_DEV_FLAGS_NO_PM_RESET) ||
	    pci_read_config_word(dev, pm + PCI_PM_CTRL, &control) ||
	    (control & PCI_PM_CTRL_NO_SOFT_RESET) || dev->current_state != PCI_D0)
		return -ENOTTY;
	control = (u16)((control & ~PCI_PM_CTRL_STATE_MASK) | (__force u16)PCI_D3hot);
	if (pci_write_config_word(dev, pm + PCI_PM_CTRL, control))
		return -EIO;
	msleep(10);
	control &= (u16)~PCI_PM_CTRL_STATE_MASK;
	if (pci_write_config_word(dev, pm + PCI_PM_CTRL, control))
		return -EIO;
	msleep(10);
	return reset_wait(dev);
}

/* Reset one function and restore its header, following pci_reset_function
 * with the methods a function has on its own: function-level reset, then the
 * power-state reset. A reset of the bus above belongs to the function's
 * owner. Without a method Linux answers ENOTTY. */
int pci_reset_function(struct pci_dev *dev)
{
	int result;

	if (pci_save_state(dev))
		return -EIO;
	/* Quiet the function as pci_dev_save_and_disable does: no decoding, no
	 * bus mastering, no INTx. */
	if (pci_write_config_word(dev, PCI_COMMAND, PCI_COMMAND_INTX_DISABLE))
		return -EIO;
	result = reset_flr(dev);
	if (result == -ENOTTY)
		result = reset_power(dev);
	pci_restore_state(dev);
	return result;
}

/* Wake from D3, following pci_enable_wake: the function's own PME is armed
 * or disarmed in its power management capability. No platform wake source
 * stands behind a function that cannot signal PME, which is Linux's error. */
int pci_wake_from_d3(struct pci_dev *dev, bool enable)
{
	u8 pm = pci_find_capability(dev, PCI_CAP_ID_PM);
	u16 control;

	if (!pm || !!enable == !!dev->wakeup_prepared)
		return 0;
	if (!pci_pme_capable(dev, PCI_D3cold) && !pci_pme_capable(dev, PCI_D3hot)) {
		dev->wakeup_prepared = false;
		return enable ? -ENODEV : 0;
	}
	if (pci_read_config_word(dev, pm + PCI_PM_CTRL, &control))
		return -EIO;
	/* Writing the status bit clears a stale PME. */
	control |= PCI_PM_CTRL_PME_STATUS | PCI_PM_CTRL_PME_ENABLE;
	if (!enable)
		control &= (u16)~PCI_PM_CTRL_PME_ENABLE;
	if (pci_write_config_word(dev, pm + PCI_PM_CTRL, control))
		return -EIO;
	dev->wakeup_prepared = enable;
	return 0;
}

/* The power management capability's D-state, with the settling time D3hot
 * transitions need. A function without the capability stays in D0. */
int pci_set_power_state(struct pci_dev *dev, pci_power_t state)
{
    u8 pm;
    u16 control;
    int error;
    if (state < PCI_D0 || state > PCI_D3hot)
        return -EINVAL;
    error = stela_linux_pci_find_capability(dev, PCI_CAP_ID_PM, &pm);
    if (error)
        return pcibios_err_to_errno(error);
    if (!pm)
        return state == PCI_D0 ? 0 : -EIO;
    error = pci_read_config_word(dev, pm + PCI_PM_CTRL, &control);
    if (error)
        return pcibios_err_to_errno(error);
    u16 current_state = control & PCI_PM_CTRL_STATE_MASK;
    u16 target = (__force u16)state;
    if (current_state == target)
        return 0;
    control = (control & (u16)~PCI_PM_CTRL_STATE_MASK) | target;
    error = pci_write_config_word(dev, pm + PCI_PM_CTRL, control);
    if (error)
        return pcibios_err_to_errno(error);
    if (current_state == (__force u16)PCI_D3hot || target == (__force u16)PCI_D3hot)
        msleep(10);
    dev->current_state = state;
    return 0;
}

struct pci_dev *pci_dev_get(struct pci_dev *dev)
{
	if (dev)
		get_device(&dev->dev);
	return dev;
}

void pci_dev_put(struct pci_dev *dev)
{
	if (dev)
		put_device(&dev->dev);
}

struct pci_dev *pci_get_domain_bus_and_slot(int domain, unsigned int bus, unsigned int devfn)
{
	struct pci_dev *dev;

	list_for_each_entry(dev, &devices, bus_list)
		if (pci_domain_nr(dev->bus) == domain && dev->bus->number == bus &&
		    dev->devfn == devfn)
			return pci_dev_get(dev);
	return NULL;
}

/* Other functions, following drivers/pci/search.c, among those their owners
 * registered: the next of a class after from, whose reference the call
 * takes over, and whether any matches a table. */
struct pci_dev *pci_get_class(unsigned int class, struct pci_dev *from)
{
	struct pci_dev *dev = list_prepare_entry(from, &devices, bus_list);

	list_for_each_entry_continue(dev, &devices, bus_list)
		if (dev->class == class) {
			pci_dev_get(dev);
			pci_dev_put(from);
			return dev;
		}
	pci_dev_put(from);
	return NULL;
}

int pci_dev_present(const struct pci_device_id *ids)
{
	struct pci_dev *dev;

	list_for_each_entry(dev, &devices, bus_list)
		if (pci_match_id(ids, dev))
			return 1;
	return 0;
}

/* Whether the function can signal PME# from state, as its power management
 * capability says (PCI PM 1.2, 3.2.3), which drivers/pci/pci.c caches. */
/* drivers/pci/pci.c: whether the function may enter D3cold. Nothing here
 * puts a function or its bridge into D3cold, so only the flag changes. */
void pci_d3cold_enable(struct pci_dev *dev)
{
	dev->no_d3cold = false;
}

void pci_d3cold_disable(struct pci_dev *dev)
{
	dev->no_d3cold = true;
}

bool pci_pme_capable(struct pci_dev *dev, pci_power_t state)
{
	u8 capability = pci_find_capability(dev, PCI_CAP_ID_PM);
	u16 capabilities;

	if (!capability || state < PCI_D0 || state > PCI_D3cold ||
	    pci_read_config_word(dev, capability + PCI_PM_PMC, &capabilities))
		return false;
	return (capabilities & PCI_PM_CAP_PME_MASK) >> PCI_PM_CAP_PME_SHIFT & 1U << state;
}

/* The entries of the function's MSI-X table, following
 * drivers/pci/msi/api.c, which answers -EINVAL for a function without an
 * MSI-X capability: no more than its owner has lines for, since a driver
 * may ask for every entry it is told of (xe does). Where the owner routes
 * no MSI-X table or has no line left, Linux sees the function as one
 * without the capability, and a driver that takes MSI-X when it is offered
 * takes its one MSI vector instead, said once. */
int pci_msix_vec_count(struct pci_dev *dev)
{
	u8 capability = pci_find_capability(dev, PCI_CAP_ID_MSIX);
	u16 control;

	if (!capability || pci_read_config_word(dev, capability + PCI_MSIX_FLAGS, &control))
		return -EINVAL;
	int lines = stela_linux_pci_msix_lines(dev);

	if (lines <= 0) {
		stela_linux_unsupported("PCI MSI-X tables: a function's driver gets its one MSI vector");
		return -EINVAL;
	}
	return min((control & PCI_MSIX_FLAGS_QSIZE) + 1, lines);
}

/* BAR mappings, following drivers/pci/iomap.c. A port range has no mapping
 * here: Stela's arch reaches ports by instruction, never as memory. */
static void __iomem *map_bar(struct pci_dev *dev, int bar, unsigned long offset,
			     unsigned long maxlen, bool write_combining)
{
	if (bar < 0 || bar >= PCI_STD_NUM_BARS)
		return NULL;

	resource_size_t start = pci_resource_start(dev, bar);
	resource_size_t len = pci_resource_len(dev, bar);
	unsigned long flags = pci_resource_flags(dev, bar);

	if (len <= offset || !start)
		return NULL;
	len -= offset;
	start += offset;
	if (maxlen && len > maxlen)
		len = maxlen;
	if (flags & IORESOURCE_IO) {
		if (!write_combining)
			stela_linux_unsupported("a mapping of a PCI I/O port BAR");
		return NULL;
	}
	if (!(flags & IORESOURCE_MEM))
		return NULL;
	return write_combining ? ioremap_wc(start, len) : ioremap(start, len);
}

void __iomem *pci_iomap_range(struct pci_dev *dev, int bar, unsigned long offset,
			      unsigned long maxlen)
{
	return map_bar(dev, bar, offset, maxlen, false);
}

void __iomem *pci_iomap_wc_range(struct pci_dev *dev, int bar, unsigned long offset,
				 unsigned long maxlen)
{
	return map_bar(dev, bar, offset, maxlen, true);
}

void __iomem *pci_iomap(struct pci_dev *dev, int bar, unsigned long maxlen)
{
	return map_bar(dev, bar, 0, maxlen, false);
}

void __iomem *pci_iomap_wc(struct pci_dev *dev, int bar, unsigned long maxlen)
{
	return map_bar(dev, bar, 0, maxlen, true);
}

void pci_iounmap(struct pci_dev *dev, void __iomem *address)
{
	(void)dev;
	iounmap(address);
}

/* Enabling, following drivers/pci/pci.c. The owner has powered the function
 * and assigned its resources; this turns on their decoding. */
int pci_enable_device(struct pci_dev *dev)
{
    int error;
    u16 old, command;
    if (atomic_inc_return(&dev->enable_cnt) > 1)
        return 0;
    error = pci_read_config_word(dev, PCI_COMMAND, &old);
    if (error)
        goto refused;
    command = old;
    for (int bar = 0; bar < PCI_ROM_RESOURCE; bar++) {
        unsigned long flags = pci_resource_flags(dev, bar);
        if (flags & IORESOURCE_IO)
            command |= PCI_COMMAND_IO;
        else if (flags & IORESOURCE_MEM)
            command |= PCI_COMMAND_MEMORY;
    }
    if (command != old) {
        error = pci_write_config_word(dev, PCI_COMMAND, command);
        if (error)
            goto refused;
    }
    return 0;
refused:
    /* A possibly applied write traps at the actual Rust boundary. This only
     * restores metadata after a before-write PCIBIOS refusal; no hardware RMW. */
    atomic_dec(&dev->enable_cnt);
    return pcibios_err_to_errno(error);
}

void pci_disable_device(struct pci_dev *dev)
{
    WARN_ON_ONCE(atomic_read(&dev->enable_cnt) <= 0);
    if (atomic_dec_return(&dev->enable_cnt))
        return;
    if (dev->is_busmaster) {
        u16 command;
        if (pci_read_config_word(dev, PCI_COMMAND, &command) ||
            pci_write_config_word(dev, PCI_COMMAND, command & (u16)~PCI_COMMAND_MASTER))
            stela_linux_fatal("PCI disable failed without completion");
        dev->is_busmaster = 0;
    }
}

static void managed_disable(void *data)
{
	pci_disable_device(data);
}

/* Enabling that the driver's detach undoes, following drivers/pci/devres.c. */
int pcim_enable_device(struct pci_dev *dev)
{
	int error = pci_enable_device(dev);

	if (error)
		return error;
	error = devm_add_action_or_reset(&dev->dev, managed_disable, dev);
	if (!error)
		dev->is_managed = true;
	return error;
}

void pci_clear_master(struct pci_dev *dev)
{
    u16 command;
    if (pci_read_config_word(dev, PCI_COMMAND, &command))
        stela_linux_fatal("PCI bus-master access failed without completion");
    if ((command & PCI_COMMAND_MASTER) &&
        pci_write_config_word(dev, PCI_COMMAND, command & (u16)~PCI_COMMAND_MASTER))
        stela_linux_fatal("PCI bus-master access failed without completion");
    dev->is_busmaster = 0;
}

/* Linux's __pci_set_master reads Command only to compare the old and new
 * values, writes it and goes on whatever the function kept. A function may
 * drop the write (Bus Master Enable hardwired to zero, or a write the
 * function ignores), and its DMA then never reaches memory: this reads the
 * register back after the write and says so, then goes on as Linux does. */
void pci_set_master(struct pci_dev *dev)
{
    u16 command;
    if (pci_read_config_word(dev, PCI_COMMAND, &command))
        stela_linux_fatal("PCI bus-master access failed without completion");
    if (!(command & PCI_COMMAND_MASTER)) {
        if (pci_write_config_word(dev, PCI_COMMAND, command | PCI_COMMAND_MASTER) ||
            pci_read_config_word(dev, PCI_COMMAND, &command))
            stela_linux_fatal("PCI bus-master access failed without completion");
        if (!(command & PCI_COMMAND_MASTER))
            pci_warn(dev, "bus mastering stayed off (command %#06x): its DMA cannot reach memory\n",
                     command);
    }
    dev->is_busmaster = 1;
}

/* Option ROMs, following drivers/pci/rom.c. The owner assigned the ROM
 * resource, whose CPU and bus addresses are one on x86. */
int pci_enable_rom(struct pci_dev *pdev)
{
    struct resource *res = &pdev->resource[PCI_ROM_RESOURCE];
    u32 address;
    int error;
    if (!res->flags)
        return -1;
    if (res->flags & IORESOURCE_ROM_SHADOW)
        return 0;
    error = pci_read_config_dword(pdev, pdev->rom_base_reg, &address);
    if (error)
        return pcibios_err_to_errno(error);
    address &= ~PCI_ROM_ADDRESS_MASK;
    address |= (u32)res->start | PCI_ROM_ADDRESS_ENABLE;
    error = pci_write_config_dword(pdev, pdev->rom_base_reg, address);
    return pcibios_err_to_errno(error);
}

void pci_disable_rom(struct pci_dev *pdev)
{
    struct resource *res = &pdev->resource[PCI_ROM_RESOURCE];
    u32 address;
    if (res->flags & IORESOURCE_ROM_SHADOW)
        return;
    if (pci_read_config_dword(pdev, pdev->rom_base_reg, &address))
        stela_linux_fatal("PCI ROM disable failed without completion");
    address &= (u32)~PCI_ROM_ADDRESS_ENABLE;
    if (pci_write_config_dword(pdev, pdev->rom_base_reg, address))
        stela_linux_fatal("PCI ROM disable failed without completion");
}

/* The bytes the ROM's images span, never more than its window: real ROMs
 * have windows far larger than their contents, and some misreport. */
static size_t rom_size(struct pci_dev *pdev, void __iomem *rom, size_t size)
{
	void __iomem *image = rom;
	unsigned int length;
	int last_image;

	do {
		if (readw(image) != 0xaa55) {
			pci_info(pdev, "Invalid PCI ROM header signature: got %#06x\n",
				 readw(image));
			break;
		}
		void __iomem *data = image + readw(image + 24);
		if (readl(data) != 0x52494350) {
			pci_info(pdev, "Invalid PCI ROM data signature: got %#010x\n",
				 readl(data));
			break;
		}
		last_image = readb(data + 21) & 0x80;
		length = readw(data + 16);
		image += length * 512;
		if (image >= rom + size)
			break;
		if (!last_image && readw(image) != 0xaa55)
			break;
	} while (length && !last_image);
	return min((size_t)(image - rom), size);
}

void __iomem *pci_map_rom(struct pci_dev *pdev, size_t *size)
{
	resource_size_t start = pci_resource_start(pdev, PCI_ROM_RESOURCE);

	*size = (size_t)pci_resource_len(pdev, PCI_ROM_RESOURCE);
	if (!*size || pci_enable_rom(pdev))
		return NULL;
	void __iomem *rom = ioremap(start, *size);
	if (rom) {
		*size = rom_size(pdev, rom, *size);
		if (*size)
			return rom;
		iounmap(rom);
	}
	pci_disable_rom(pdev);
	return NULL;
}

void pci_unmap_rom(struct pci_dev *pdev, void __iomem *rom)
{
	iounmap(rom);
	pci_disable_rom(pdev);
}

#ifdef STELA_LINUX_PCI_TEST
#include <stela/linux.h>
#include <stela/test.h>

static u8 config[4096], before[4096];
static int config_error;
static bool ignore_bus_master_write;
static unsigned int warnings;
static u8 rom_window[0x1000] __aligned(PAGE_SIZE);

static int read_config(struct pci_bus *bus, unsigned int devfn, int where, int size, u32 *value)
{
	(void)bus;
	if (devfn != PCI_DEVFN(1, 0))
		return PCIBIOS_DEVICE_NOT_FOUND;
	if (config_error)
		return config_error;
	*value = 0;
	memcpy(value, &config[where], (size_t)size);
	return 0;
}

static int write_config(struct pci_bus *bus, unsigned int devfn, int where, int size, u32 value)
{
	(void)bus;
	(void)devfn;
	if (ignore_bus_master_write && where == PCI_COMMAND && size == 2 &&
	    (value & PCI_COMMAND_MASTER))
		return 0;
	memcpy(&config[where], &value, (size_t)size);
	return 0;
}

static struct pci_ops ops = {.read = read_config, .write = write_config};

static void *map_device(void *context, u64 physical, size_t size,
			bool write_combining)
{
	(void)context;
	(void)write_combining;
	assert(physical == 0xc0000000 && size == sizeof(rom_window));
	return rom_window;
}

static void unmap_device(void *context, void *address)
{
	(void)context;
	assert(address == rom_window);
}

static void log_line(void *context, int level, const char *message)
{
	(void)context;
	if (level == LOGLEVEL_WARNING && strstr(message, "bus mastering stayed off (command 0x0002)"))
		warnings++;
}

static unsigned int port_mappings, bus_windows, resizes, msix_tables;

static void unsupported(void *context, const char *operation)
{
	(void)context;
	if (!strcmp(operation, "a mapping of a PCI I/O port BAR"))
		port_mappings++;
	else if (!strcmp(operation, "a PCI bus's windows: BARs keep the sizes the firmware gave"))
		bus_windows++;
	else if (!strcmp(operation, "resizing a PCI BAR: BARs keep the sizes the firmware gave"))
		resizes++;
	else if (!strcmp(operation, "PCI MSI-X tables: a function's driver gets its one MSI vector"))
		msix_tables++;
	else
		assert(false);
}

static const struct stela_linux_services services = {
	.map_device = map_device,
	.unmap_device = unmap_device,
	.log = log_line,
	.unsupported = unsupported,
};
static const struct stela_linux_owner owner = {.services = &services};

static void one_image(u8 *image, unsigned int blocks, bool last)
{
	image[0] = 0x55;
	image[1] = 0xaa;
	image[24] = 0x40;
	memcpy(image + 0x40, "PCIR", 4);
	image[0x40 + 16] = (u8)blocks;
	image[0x40 + 21] = last ? 0x80 : 0;
}

static struct pci_dev dev, absent;

int main(void)
{
	stela_linux_enter(&owner);
	struct pci_bus bus = {.ops = &ops, .number = 3};
	dev.bus = &bus;
	dev.devfn = PCI_DEVFN(1, 0);
	dev.hdr_type = PCI_HEADER_TYPE_NORMAL;
	dev.rom_base_reg = PCI_ROM_ADDRESS;

	u32 value;
	config[PCI_VENDOR_ID] = 0xde;
	config[PCI_VENDOR_ID + 1] = 0x10;
	assert(!pci_read_config_dword(&dev, 0, &value) && value == 0x10de);
	assert(pci_read_config_dword(&dev, 2, &value) == PCIBIOS_BAD_REGISTER_NUMBER);
	absent.bus = &bus;
	absent.devfn = PCI_DEVFN(2, 0);
	assert(pci_read_config_dword(&absent, 0, &value) == PCIBIOS_DEVICE_NOT_FOUND &&
	       value == ~0U);

	/* Capabilities at 0x50 then 0x60; a list that loops stops. */
	config[PCI_STATUS] = PCI_STATUS_CAP_LIST;
	config[PCI_CAPABILITY_LIST] = 0x50;
	config[0x50] = PCI_CAP_ID_PM;
	config[0x51] = 0x60;
	config[0x60] = PCI_CAP_ID_EXP;
    u8 checked_capability = 0xaa;
    config_error = PCIBIOS_SET_FAILED;
    assert(pci_read_config_dword(&dev, 0, &value) == PCIBIOS_SET_FAILED && value == ~0U);
    assert(stela_linux_pci_find_capability(&dev, PCI_CAP_ID_EXP, &checked_capability) == PCIBIOS_SET_FAILED && !checked_capability);
    dev.saved_config_space[0] = 0x12345678;
    assert(pci_save_state(&dev) == -EIO && !dev.state_saved && dev.saved_config_space[0] == 0x12345678);
    assert(pci_enable_device(&dev) == -EIO && !atomic_read(&dev.enable_cnt));
    assert(pci_set_power_state(&dev, PCI_D0) == -EIO);
    config_error = 0;
	assert(pci_find_capability(&dev, PCI_CAP_ID_EXP) == 0x60);
	assert(!pci_find_capability(&dev, PCI_CAP_ID_AGP));
	config[0x61] = 0x50;
	assert(!pci_find_capability(&dev, PCI_CAP_ID_AGP));

	dev.resource[0].flags = IORESOURCE_MEM;
	assert(!pci_enable_device(&dev) && !pci_enable_device(&dev));
	/* A function that drops the write: a warning, and on as Linux goes. */
	ignore_bus_master_write = true;
	pci_set_master(&dev);
	assert(config[PCI_COMMAND] == PCI_COMMAND_MEMORY && dev.is_busmaster && warnings == 1);
	ignore_bus_master_write = false;
	pci_set_master(&dev);
	assert(warnings == 1);
	assert(config[PCI_COMMAND] == (PCI_COMMAND_MEMORY | PCI_COMMAND_MASTER));
	pci_disable_device(&dev);
	assert(config[PCI_COMMAND] & PCI_COMMAND_MASTER);
	pci_disable_device(&dev);
	assert(config[PCI_COMMAND] == PCI_COMMAND_MEMORY && !dev.is_busmaster);

	device_initialize(&dev.dev);
	assert(!stela_linux_pci_publish_device(&dev));
	assert(pci_get_domain_bus_and_slot(0, 3, PCI_DEVFN(1, 0)) == &dev);
	pci_dev_put(&dev);
	assert(!pci_get_domain_bus_and_slot(0, 4, PCI_DEVFN(1, 0)));

	/* A function is found by class and by table, and a search past the
	 * last one drops the reference it was given. */
	const u32 any = (u32)PCI_ANY_ID;
	const struct pci_device_id present[] = {{0x10de, any, any, any, 0, 0, 0, 0}, {}};
	const struct pci_device_id missing[] = {{0x8086, any, any, any, 0, 0, 0, 0}, {}};
	unsigned int references = kref_read(&dev.dev.kobj.kref);

	dev.vendor = 0x10de;
	dev.class = PCI_CLASS_BRIDGE_ISA << 8;
	assert(pci_dev_present(present) && !pci_dev_present(missing));
	assert(pci_get_class(PCI_CLASS_BRIDGE_ISA << 8, NULL) == &dev);
	assert(kref_read(&dev.dev.kobj.kref) == references + 1);
	assert(!pci_get_class(PCI_CLASS_BRIDGE_ISA << 8, &dev));
	assert(kref_read(&dev.dev.kobj.kref) == references);
	assert(!pci_get_class(PCI_CLASS_DISPLAY_VGA << 8, NULL));
	list_del(&dev.bus_list);

	/* A detach undoes a managed enable. */
	assert(!pcim_enable_device(&dev) && dev.is_managed);
	assert(config[PCI_COMMAND] == PCI_COMMAND_MEMORY && atomic_read(&dev.enable_cnt) == 1);
	stela_linux_devres_release(&dev.dev);
	assert(!atomic_read(&dev.enable_cnt));

	/* PME# from D3hot and D3cold only; an MSI-X table of eight entries,
	 * which is not offered, since the owner routes one message. */
	*(u16 *)&config[0x50 + PCI_PM_PMC] = 0x18 << PCI_PM_CAP_PME_SHIFT;
	assert(pci_pme_capable(&dev, PCI_D3hot) && pci_pme_capable(&dev, PCI_D3cold));
	assert(!pci_pme_capable(&dev, PCI_D0) && !pci_pme_capable(&dev, PCI_D2));
	assert(pci_msix_vec_count(&dev) == -EINVAL && !msix_tables);
	config[0x60] = PCI_CAP_ID_MSIX;
	config[0x61] = 0;
	*(u16 *)&config[0x60 + PCI_MSIX_FLAGS] = 7;
	assert(pci_msix_vec_count(&dev) == -EINVAL && msix_tables == 1);

	/* No extended list, then a resizable BAR capability behind another:
	 * BAR 2 offers 256 MiB to 8 GiB. No bus lists a window to grow into. */
	assert(!pci_find_ext_capability(&dev, PCI_EXT_CAP_ID_REBAR));
	assert(!pci_rebar_get_possible_sizes(&dev, 2) && pci_rebar_get_max_size(&dev, 2) == -ENOENT);
	*(u32 *)&config[0x100] = PCI_EXT_CAP_ID_ERR | 1 << 16 | 0x200 << 20;
	*(u32 *)&config[0x200] = PCI_EXT_CAP_ID_REBAR | 1 << 16;
	*(u32 *)&config[0x200 + PCI_REBAR_CAP] = 0x3f << 12;
	*(u32 *)&config[0x200 + PCI_REBAR_CTRL] = 2 | 2 << PCI_REBAR_CTRL_NBAR_SHIFT;
	*(u32 *)&config[0x208 + PCI_REBAR_CAP] = 0x10;
	*(u32 *)&config[0x208 + PCI_REBAR_CTRL] = 0;
	assert(pci_find_ext_capability(&dev, PCI_EXT_CAP_ID_REBAR) == 0x200);
	assert(!pci_find_ext_capability(&dev, PCI_EXT_CAP_ID_SRIOV));
	assert(pci_rebar_get_possible_sizes(&dev, 2) == 0x3f00);
	assert(pci_rebar_get_possible_sizes(&dev, 0) == 1 && !pci_rebar_get_possible_sizes(&dev, 1));
	assert(pci_rebar_get_max_size(&dev, 2) == 13 && pci_rebar_size_to_bytes(13) == 8ULL << 30);
	assert(pci_rebar_size_supported(&dev, 2, 8) && !pci_rebar_size_supported(&dev, 2, 7));
	assert(!pci_rebar_size_supported(&dev, 2, -1) && !pci_rebar_size_supported(&dev, 2, 64));
	assert(pci_rebar_bytes_to_size(1) == 0 && pci_rebar_bytes_to_size(SZ_1M) == 0);
	assert(pci_rebar_bytes_to_size(SZ_256M) == 8 && pci_rebar_bytes_to_size(SZ_256M + 1) == 9);
	assert(!pci_bus_resource_n(&bus, 0) && bus_windows == 1);
	/* Growing a BAR is refused as by a bridge that keeps the firmware's
	 * assignment, and nothing of the function is written. */
	memcpy(before, config, sizeof(config));
	assert(pci_resize_resource(&dev, 2, 13, 0) == -ENOTSUPP && resizes == 1);
	assert(!memcmp(before, config, sizeof(config)));
	*(u32 *)&config[0x100] = PCI_EXT_CAP_ID_ERR | 1 << 16 | 0x100 << 20;
	assert(!pci_find_ext_capability(&dev, PCI_EXT_CAP_ID_REBAR));
	memset(&config[0x100], 0, sizeof(config) - 0x100);

	/* A PCI Express endpoint of capability version 2: an x16 link of up to
	 * 8 GT/s (a vector that lists 16 GT/s above the link's maximum does not
	 * count), 256-byte payloads, function-level reset. */
	config[0x61] = 0x70;
	config[0x70] = PCI_CAP_ID_EXP;
	*(u16 *)&config[0x70 + PCI_EXP_FLAGS] = 2;
	*(u32 *)&config[0x70 + PCI_EXP_DEVCAP] = 1 | PCI_EXP_DEVCAP_FLR;
	*(u16 *)&config[0x70 + PCI_EXP_DEVCTL] = 1 << 5;
	*(u32 *)&config[0x70 + PCI_EXP_LNKCAP] = PCI_EXP_LNKCAP_SLS_8_0GB | 16 << 4;
	*(u32 *)&config[0x70 + PCI_EXP_LNKCAP2] = 0x1e;
	dev.pcie_cap = pci_find_capability(&dev, PCI_CAP_ID_EXP);
	assert(dev.pcie_cap == 0x70 && !stela_linux_pci_express_probe(&dev));
	assert(pci_pcie_type(&dev) == PCI_EXP_TYPE_ENDPOINT && dev.pcie_mpss == 1);
	assert(pcie_get_speed_cap(&dev) == PCIE_SPEED_8_0GT);
	assert(pcie_get_width_cap(&dev) == PCIE_LNK_X16 && pcie_get_mps(&dev) == 256);
	/* An endpoint has no slot registers: they read as zero. */
	u16 word = 0xffff;
	*(u16 *)&config[0x70 + PCI_EXP_SLTCTL] = 0x1234;
	assert(!pcie_capability_read_word(&dev, PCI_EXP_SLTCTL, &word) && !word);
	assert(pcie_capability_read_word(&dev, PCI_EXP_DEVCTL + 1, &word) ==
	       PCIBIOS_BAD_REGISTER_NUMBER);
	/* Atomic requests are allowed where the platform enabled the requester. */
	assert(pci_enable_atomic_ops_to_root(&dev, PCI_EXP_DEVCAP2_ATOMIC_COMP64) == -EINVAL);
	*(u16 *)&config[0x70 + PCI_EXP_DEVCTL2] = PCI_EXP_DEVCTL2_ATOMIC_REQ;
	assert(!pci_enable_atomic_ops_to_root(&dev, PCI_EXP_DEVCAP2_ATOMIC_COMP64));
	assert(pci_wait_for_pending_transaction(&dev) == 1);

	/* A driver's copy of the saved header loads back. */
	assert(!pci_store_saved_state(&dev) && !pci_save_state(&dev));
	struct pci_saved_state *saved = pci_store_saved_state(&dev);
	assert(saved);
	dev.saved_config_space[0] = 0;
	assert(!pci_load_saved_state(&dev, saved) && dev.state_saved);
	assert(dev.saved_config_space[0] == 0x10de);
	kfree(saved);
	assert(!pci_load_saved_state(&dev, NULL) && !dev.state_saved);

	/* A reset of the whole function (amdgpu's mode 1 reset, then its
	 * pci_load_saved_state and pci_restore_state) clears the PCI Express
	 * controls and returns a resizable BAR to its default size: both come
	 * back, the BAR to its resource's 8 GiB, before the header. */
	*(u16 *)&config[0x70 + PCI_EXP_DEVCTL] = 1 << 5 | PCI_EXP_DEVCTL_RELAX_EN;
	*(u16 *)&config[0x70 + PCI_EXP_LNKCTL] = PCI_EXP_LNKCTL_CCC;
	*(u16 *)&config[0x70 + PCI_EXP_DEVCTL2] = PCI_EXP_DEVCTL2_ATOMIC_REQ;
	*(u32 *)&config[0x100] = PCI_EXT_CAP_ID_REBAR | 1 << 16;
	*(u32 *)&config[0x100 + PCI_REBAR_CAP] = 0x3f << 12;
	*(u32 *)&config[0x100 + PCI_REBAR_CTRL] = 2 | 2 << PCI_REBAR_CTRL_NBAR_SHIFT |
						  13 << PCI_REBAR_CTRL_BAR_SHIFT;
	*(u32 *)&config[0x108 + PCI_REBAR_CTRL] = 4 | 8 << PCI_REBAR_CTRL_BAR_SHIFT;
	dev.resource[2] = (struct resource){
		.start = 0x400000000, .end = 0x5ffffffff, .flags = IORESOURCE_MEM};
	memset(&dev.resource[4], 0, sizeof(dev.resource[4]));
	/* The owner's 64-bit, maskable message, on. */
	config[0x71] = 0x80;
	config[0x80] = PCI_CAP_ID_MSI;
	*(u16 *)&config[0x80 + PCI_MSI_FLAGS] =
		PCI_MSI_FLAGS_64BIT | PCI_MSI_FLAGS_MASKBIT | PCI_MSI_FLAGS_ENABLE;
	*(u32 *)&config[0x80 + PCI_MSI_ADDRESS_LO] = 0xfee01000;
	*(u32 *)&config[0x80 + PCI_MSI_ADDRESS_HI] = 0;
	*(u16 *)&config[0x80 + PCI_MSI_DATA_64] = 0x4041;
	*(u32 *)&config[0x80 + PCI_MSI_MASK_64] = 0;
	dev.msi_enabled = 1;
	assert(!pci_save_state(&dev));
	saved = pci_store_saved_state(&dev);
	assert(saved);
	memcpy(before, config, sizeof(config));
	memset(config + PCI_BASE_ADDRESS_0, 0, PCI_ROM_ADDRESS - PCI_BASE_ADDRESS_0);
	*(u16 *)&config[0x70 + PCI_EXP_DEVCTL] = 2 << 12;
	*(u16 *)&config[0x70 + PCI_EXP_LNKCTL] = 0;
	*(u16 *)&config[0x70 + PCI_EXP_DEVCTL2] = 0;
	*(u32 *)&config[0x100 + PCI_REBAR_CTRL] &= ~(u32)PCI_REBAR_CTRL_BAR_SIZE;
	*(u32 *)&config[0x100 + PCI_REBAR_CTRL] |= 8 << PCI_REBAR_CTRL_BAR_SHIFT;
	memset(&config[0x80 + PCI_MSI_FLAGS], 0, 0x18 - PCI_MSI_FLAGS);
	*(u16 *)&config[0x80 + PCI_MSI_FLAGS] = PCI_MSI_FLAGS_64BIT | PCI_MSI_FLAGS_MASKBIT;
	*(u32 *)&config[0x80 + PCI_MSI_MASK_64] = 1;
	assert(!pci_load_saved_state(&dev, saved) && dev.state_saved);
	pci_restore_state(&dev);
	assert(!dev.state_saved && !memcmp(before, config, sizeof(config)));
	/* BAR 4 has no resource, so its size is not written. */
	assert((*(u32 *)&config[0x108 + PCI_REBAR_CTRL] & PCI_REBAR_CTRL_BAR_SIZE) ==
	       8 << PCI_REBAR_CTRL_BAR_SHIFT);
	/* A copy holding a capability this function never saved does not
	 * load. */
	stela_linux_pci_free_saved_state(&dev);
	assert(hlist_empty(&dev.saved_cap_space));
	assert(pci_load_saved_state(&dev, saved) == -EINVAL && !dev.state_saved);
	kfree(saved);
	memset(&config[0x100], 0, sizeof(config) - 0x100);
	*(u16 *)&config[0x70 + PCI_EXP_DEVCTL] = 1 << 5;
	*(u16 *)&config[0x70 + PCI_EXP_LNKCTL] = 0;
	memset(&dev.resource[2], 0, sizeof(dev.resource[2]));
	/* A function whose message is off gets none written back. */
	dev.msi_enabled = 0;
	assert(!pci_save_state(&dev));
	*(u16 *)&config[0x80 + PCI_MSI_DATA_64] = 0;
	pci_restore_state(&dev);
	assert(!*(u16 *)&config[0x80 + PCI_MSI_DATA_64]);
	memset(&config[0x80], 0, 0x18);
	config[0x71] = 0;

	/* A function with neither a function-level reset nor a D3hot that resets
	 * it has no reset of its own, and keeps its header. (The resets
	 * themselves sleep, which needs a Linux task: stela/pci_reset_test.c.) */
	config[PCI_COMMAND] = PCI_COMMAND_MEMORY | PCI_COMMAND_MASTER;
	dev.current_state = PCI_D0;
	dev.devcap &= ~(u32)PCI_EXP_DEVCAP_FLR;
	*(u16 *)&config[0x50 + PCI_PM_CTRL] = PCI_PM_CTRL_NO_SOFT_RESET;
	assert(pci_reset_function(&dev) == -ENOTTY);
	assert(config[PCI_COMMAND] == (PCI_COMMAND_MEMORY | PCI_COMMAND_MASTER));
	pci_clear_master(&dev);
	assert(config[PCI_COMMAND] == PCI_COMMAND_MEMORY && !dev.is_busmaster);

	/* Wake arms and disarms the function's own PME. */
	assert(!pci_wake_from_d3(&dev, true));
	assert(*(u16 *)&config[0x50 + PCI_PM_CTRL] & PCI_PM_CTRL_PME_ENABLE);
	assert(!pci_wake_from_d3(&dev, false));
	assert(!(*(u16 *)&config[0x50 + PCI_PM_CTRL] & PCI_PM_CTRL_PME_ENABLE));
	memset(&config[0x70], 0, 0x40);
	config[0x61] = 0;

	/* A memory BAR maps from an offset, bounded by the caller's length and
	 * the BAR's end; a port BAR, an empty one and one past the last do not. */
	dev.resource[0].start = 0xc0000000 - 0x2000;
	dev.resource[0].end = 0xc0000000 + sizeof(rom_window) - 1;
	assert(pci_iomap_range(&dev, 0, 0x2000, 0) == (void __iomem *)rom_window);
	assert(pci_iomap_range(&dev, 0, 0x2000, 2 * sizeof(rom_window)) ==
	       (void __iomem *)rom_window);
	pci_iounmap(&dev, rom_window);
	assert(!pci_iomap_range(&dev, 0, 0x2000 + sizeof(rom_window), 0));
	dev.resource[0].start = 0xc0000000;
	assert(pci_iomap(&dev, 0, 0) == (void __iomem *)rom_window);
	assert(pci_iomap_wc(&dev, 0, sizeof(rom_window)) == (void __iomem *)rom_window);
	assert(!pci_iomap(&dev, 1, 0) && !pci_iomap(&dev, PCI_STD_NUM_BARS, 0));
	dev.resource[1] = dev.resource[0];
	dev.resource[1].flags = IORESOURCE_IO;
	assert(!pci_iomap(&dev, 1, 0) && port_mappings == 1);
	assert(!pci_iomap_wc(&dev, 1, 0) && port_mappings == 1);
	dev.resource[1] = (struct resource){};
	dev.resource[0].end = 0;
	dev.resource[0].start = 0;

	/* Two images of a block each in a window of eight: the size is theirs. */
	dev.resource[PCI_ROM_RESOURCE].start = 0xc0000000;
	dev.resource[PCI_ROM_RESOURCE].end = 0xc0000fff;
	dev.resource[PCI_ROM_RESOURCE].flags = IORESOURCE_MEM;
	one_image(rom_window, 1, false);
	one_image(rom_window + 512, 1, true);
	size_t size;
	void __iomem *rom = pci_map_rom(&dev, &size);
	assert(rom == (void __iomem *)rom_window && size == 1024);
	assert(*(u32 *)&config[PCI_ROM_ADDRESS] == (0xc0000000 | PCI_ROM_ADDRESS_ENABLE));
	pci_unmap_rom(&dev, rom);
	assert(*(u32 *)&config[PCI_ROM_ADDRESS] == 0xc0000000);

	/* No ROM signature: nothing maps and decoding goes back off. */
	rom_window[0] = 0;
	assert(!pci_map_rom(&dev, &size) && *(u32 *)&config[PCI_ROM_ADDRESS] == 0xc0000000);
	stela_linux_pci_free_saved_state(&dev);

	/* The processor identities amdgpu's PCIe speed switching asks about:
	 * Pinnacle Ridge (Zen+), a Raptor Lake, a Hygon and an unknown maker. */
	struct cpuinfo_x86 cpu = decode_cpu("AuthenticAMD", 0x00800f82);

	assert(cpu.x86_vendor == X86_VENDOR_AMD && cpu.x86 == 0x17 && cpu.x86_model == 0x08);
	cpu = decode_cpu("AuthenticAMD", 0x00a60f12);
	assert(cpu.x86 == 0x19 && cpu.x86_model == 0x61);
	cpu = decode_cpu("GenuineIntel", 0x000b0671);
	assert(cpu.x86_vendor == X86_VENDOR_INTEL && cpu.x86 == 6 && cpu.x86_model == 0xb7);
	cpu = decode_cpu("HygonGenuine", 0x00900f02);
	assert(cpu.x86_vendor == X86_VENDOR_HYGON && cpu.x86 == 0x18 && cpu.x86_model == 0);
	cpu = decode_cpu("CentaurHauls", 0x000006f2);
	assert(cpu.x86_vendor == X86_VENDOR_UNKNOWN && cpu.x86 == 6 && cpu.x86_model == 0xf);
	/* This processor's, read once. */
	assert(&cpu_data(0) == stela_linux_cpu_data(1) && cpu_data(0).x86 >= 6);
	stela_linux_exit();
	printf("Linux PCI configuration, enabling, searches, PCI Express, resets, resizable BARs, BAR mappings, option ROMs and processor identity passed\n");
	return 0;
}
#endif
