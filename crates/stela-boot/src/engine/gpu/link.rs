//! The PCI Express link above a GPU: its L1 PM Substates set at the claim
//! as Linux's ASPM core leaves a link the firmware set up, and, for the
//! boot log, both ends' ASPM, Clock Power Management, LTR and L1 PM
//! Substates as the firmware left them and after the claim's reset, and the
//! port's view when the GPU stops answering or answers again, which tells a
//! link that went down from a port that lost power with it, with the
//! recovery the port still allows. The port is read through a read-only
//! mapping of its configuration kept for the boot; only the substates are
//! written, through the fabric that owns the port.
use crate::{native_mmio, output};
use alloc::{format, string::String};
use stela_drivers::bus::pci::{
    Address, ConfigSpace, Function, link_report, owned::ObservedConfig, owned::OwnedConfig,
};

/// The PCI Express port directly above a GPU, kept read-only, and the
/// fabric that owns it.
pub(in crate::engine) struct Port {
    address: Address,
    #[cfg_attr(not(feature = "nvk"), allow(dead_code, reason = "asked at a loss"))]
    gpu: Address,
    config: ObservedConfig,
    source: &'static native_mmio::Source,
}

impl Port {
    /// `port <address> <its report>`, or why it could not be read.
    fn report(&self) -> String {
        match link_report::read(&self.config) {
            Ok(end) => format!("port {} {end}", self.address),
            Err(error) => format!("port {} unreadable: {error:?}", self.address),
        }
    }
}

/// The port above the GPU at `address`, mapped read-only for the boot;
/// `None` for one on a root bus, and, said in a line, for one whose port
/// cannot be mapped.
pub(in crate::engine) fn port_above(
    mmio: &'static native_mmio::Source,
    address: Address,
) -> Option<Port> {
    let bridge = mmio.parent_bridge(address)?;
    let refused = |why: &dyn core::fmt::Debug| {
        output::arguments(format_args!(
            "GPU {address}: its port {bridge} is not read for the link lines: {why:?}\n"
        ));
    };
    // SAFETY: the mount's fabric owns the bridge's configuration and keeps
    // other writers out; this mapping only reads it, for the boot.
    let (window, mapping) = match unsafe { mmio.observe_config_page(bridge) } {
        Ok(observed) => observed,
        Err(error) => {
            refused(&error);
            return None;
        }
    };
    // SAFETY: the source issued this exact UC read-only page of `bridge`.
    match unsafe { ObservedConfig::attach(bridge, window, mapping) } {
        Ok(config) => Some(Port {
            address: bridge,
            gpu: address,
            config,
            source: mmio,
        }),
        Err((error, mut mapping)) => {
            // SAFETY: the refused attachment published no view or reader.
            let _ = unsafe { mapping.retire() };
            refused(&error);
            None
        }
    }
}

/// `GPU <address> link <when>: GPU <its report>; port <address> <its
/// report>`, from the GPU's configuration `gpu` and the port above it.
pub(in crate::engine) fn log_link<C: ConfigSpace + ?Sized>(
    address: Address,
    when: &str,
    gpu: &C,
    port: Option<&Port>,
) {
    let own = match link_report::read(gpu) {
        Ok(end) => format!("{end}"),
        Err(error) => format!("unreadable: {error:?}"),
    };
    let above = port.map_or_else(|| String::from("no port above it is read"), Port::report);
    output::arguments(format_args!(
        "GPU {address} link {when}: GPU {own}; {above}\n"
    ));
}

/// `GPU <address> L1 PM Substates: <what was found and done>`: the link
/// above the GPU `function` at `address`, in D0, set as Linux leaves it
/// ([`stela_drivers::bus::pci::substates`]) before the claim's reset, which
/// then saves and restores the GPU's side as set here.
///
/// # Safety
/// `function` is the cold owner of `address` the port's fabric promoted,
/// in D0, with Bus Master sealed; no other configuration writer runs.
pub(in crate::engine) unsafe fn set_substates(
    address: Address,
    function: &mut Function<OwnedConfig>,
    port: Option<&Port>,
) {
    let done = match port {
        None => String::from("not set: no port above it is read"),
        // SAFETY: forwarded; the fabric owns the port.
        Some(port) => match unsafe { port.source.configure_link_substates(address, function) } {
            Ok(configured) => format!("{configured}"),
            Err(why) => format!("not set: {why}"),
        },
    };
    output::arguments(format_args!("GPU {address} L1 PM Substates: {done}\n"));
}

/// `GPU <name> <event>; port <address> <its report>`: what the port above
/// a GPU whose driver said `event` of it reads now, which tells whether the
/// port and its link are still up, and, when it stopped answering, which
/// recovery the port still allows.
#[cfg(feature = "nvk")]
pub(in crate::engine) fn log_port_at(name: &str, event: &str, port: Option<&Port>) {
    let above = port.map_or_else(|| String::from("no port above it is read"), Port::report);
    output::arguments(format_args!("GPU {name} {event}; {above}\n"));
    if event == stela_boot_core::gpu_loss::STOPPED_ANSWERING {
        let recovery = match port {
            None => stela_boot_core::gpu_loss::Recovery::Unknown,
            Some(port) => port.recovery(),
        };
        output::arguments(format_args!("GPU {name} recovery: {recovery}\n"));
    }
}

#[cfg(feature = "nvk")]
impl Port {
    /// Which recovery the port allows now: none when it does not answer
    /// either, else a secondary bus reset through it if the fabric would
    /// make one, else why not.
    fn recovery(&self) -> stela_boot_core::gpu_loss::Recovery {
        use stela_boot_core::gpu_loss::Recovery;
        let end = match link_report::read(&self.config) {
            Ok(end) => end,
            Err(_) => return Recovery::Unknown,
        };
        if !end.answers() {
            return Recovery::PortGone;
        }
        let link_up = end
            .express
            .is_some_and(|express| express.link_status & (1 << 13) != 0);
        match self.source.parent_bus_reset_refusal(self.gpu) {
            None => Recovery::BusReset {
                port: format!("{}", self.address),
                link_up,
            },
            Some(why) => Recovery::NoBusReset { why, link_up },
        }
    }
}
