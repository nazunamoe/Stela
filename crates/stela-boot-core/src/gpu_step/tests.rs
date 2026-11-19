//! The whole GPU step of the boot screen, and the log's `GPU SELECT` lines,
//! of each kind of PC Stela meets, through the functions the boot uses: the
//! selection ([`select`]), the claims' refusals, the step's lines
//! ([`not_started`], [`started`]) and the screen's step line
//! ([`finished_step`]). Each PC is booted by two builds: a release, whose
//! image drives NVIDIA GPUs alone, and a build with every port and every
//! Vulkan driver in its image.
//!
//! What a PC is made of is what its drivers' tables say of each GPU before
//! the probe (`table`), and what the probe, the claim and the screen then
//! find. When the AMD and Intel ports are in this test binary (a workspace
//! run with every feature), each table answer is checked against them.
use super::*;
use crate::gpu_status::{Picture, shown_by_copy, unbound};
use crate::screen::finished_step;
use alloc::{string::ToString, vec};
use stela_drivers::bus::pci::Address;
use stela_drivers::gpu::selection::{Driven, fall_back, select};
use stela_drivers::gpu::stack::GpuStack;

/// Which image boots.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Build {
    /// A release: `nvk` alone, so the AMD and Intel ports are not in it.
    Release,
    /// `nvk`, `amdgpu` and `intel`, with RADV and ANV in the image.
    Full,
}

/// One display controller of a PC.
#[derive(Clone, Copy)]
struct Gpu {
    bus: u8,
    slot: u8,
    vendor: u16,
    device: u16,
    revision: u8,
    /// What its driver's table says of it before the probe in the full
    /// build: integrated, discrete or left to the probe; `Err` with the
    /// port's words where the port does not drive it.
    table: Result<Option<bool>, &'static str>,
    /// What its driver says once bound: built into the processor.
    integrated: bool,
    /// A screen hangs on it, so the firmware's picture may be on it.
    screen: bool,
    /// Its claim is refused, for this reason in these words.
    refused: Option<(Reason, &'static str)>,
    /// Its driver's probe fails.
    unbound: bool,
    /// Its driver binds it and its Metal device does not open, for this.
    no_metal: Option<&'static str>,
}

const fn gpu(bus: u8, slot: u8, vendor: u16, device: u16, revision: u8) -> Gpu {
    Gpu {
        bus,
        slot,
        vendor,
        device,
        revision,
        table: Ok(Some(false)),
        integrated: false,
        screen: false,
        refused: None,
        unbound: false,
        no_metal: None,
    }
}

impl Gpu {
    const fn screen(self) -> Self {
        Self {
            screen: true,
            ..self
        }
    }

    /// Integrated, by its table (`Some(true)`) or only by its probe (`None`).
    const fn integrated(self, table: Option<bool>) -> Self {
        Self {
            table: Ok(table),
            integrated: true,
            ..self
        }
    }

    /// Discrete, told only by its probe.
    const fn probed(self) -> Self {
        Self {
            table: Ok(None),
            ..self
        }
    }

    const fn refused(self, reason: Reason, why: &'static str) -> Self {
        Self {
            refused: Some((reason, why)),
            ..self
        }
    }

    const fn unbound(self) -> Self {
        Self {
            unbound: true,
            ..self
        }
    }

    const fn no_metal(self, why: &'static str) -> Self {
        Self {
            no_metal: Some(why),
            ..self
        }
    }

    const fn not_driven(self, why: &'static str) -> Self {
        Self {
            table: Err(why),
            ..self
        }
    }

    fn adapter(&self) -> Adapter {
        Adapter {
            address: Address {
                segment: 0,
                bus: self.bus,
                device: self.slot,
                function: 0,
            },
            vendor: Vendor::from_pci(self.vendor),
            device: self.device,
            class: [3, 0, 0],
            revision: self.revision,
        }
    }

    /// What its stack says of it in `build`, as [`selection::driven`] tells
    /// the boot: every stack of the full build draws.
    fn found(&self, build: Build) -> Found {
        let adapter = self.adapter();
        let stack = match (build, adapter.vendor) {
            (_, Vendor::Nvidia) => Ok(Driven {
                draws: true,
                integrated: Some(false),
            }),
            (Build::Full, Vendor::Amd | Vendor::Intel) => self.table.map(|integrated| Driven {
                draws: true,
                integrated,
            }),
            _ => Err(adapter.unavailable_reason()),
        };
        // The ports' own tables, where this binary holds them.
        if build == Build::Full
            && let Ok(port) = GpuStack::select(&adapter)
            && port.vendor != Vendor::Nvidia
        {
            assert_eq!(
                Ok(port.kernel.table_integrated(&adapter)),
                self.table,
                "{}",
                adapter.name()
            );
        }
        Found {
            stack,
            firmware_screen: self.screen,
        }
    }

    fn driver(&self) -> &'static str {
        match Vendor::from_pci(self.vendor) {
            Vendor::Nvidia => "nouveau",
            Vendor::Amd => "amdgpu",
            _ => "i915",
        }
    }
}

const NVIDIA: u16 = 0x10de;
const AMD: u16 = 0x1002;
const INTEL: u16 = 0x8086;
const ASPEED: u16 = 0x1a03;

/// A desktop's GeForce RTX 5080 and a laptop's GeForce RTX 4060.
const RTX_5080: Gpu = gpu(1, 0, NVIDIA, 0x2c02, 0xa1);
const RTX_4060_LAPTOP: Gpu = gpu(1, 0, NVIDIA, 0x28e0, 0xa1);
/// The card of a server, beside its BMC display.
const RTX_5080_SERVER: Gpu = gpu(0x41, 0, NVIDIA, 0x2c02, 0xa1);
/// A Radeon RX 7900 XTX and a laptop's Radeon RX 7600M XT, which amdgpu
/// takes by class: their probe tells their kind.
const RX_7900_XTX: Gpu = gpu(3, 0, AMD, 0x744c, 0xc8).probed();
const RX_7600M_XT: Gpu = gpu(3, 0, AMD, 0x7480, 0xc7).probed();
/// A Radeon RX 580, whose generation the port does not drive.
const RX_580: Gpu =
    gpu(3, 0, AMD, 0x67df, 0xe7).not_driven(stela_driver_core::amd_metal::NO_APPLE_FAMILY);
/// An Arc A770, which i915's table marks discrete.
const ARC_A770: Gpu = gpu(3, 0, INTEL, 0x56a0, 0x08);
/// A Ryzen 7040's Radeon 780M, which amdgpu takes by class.
const RADEON_780M: Gpu = gpu(0x65, 0, AMD, 0x15bf, 0xc1).integrated(None);
/// Tiger Lake's and Raptor Lake-P's Iris Xe Graphics and Raptor Lake-S's
/// UHD Graphics 770, which i915's table marks integrated.
const IRIS_XE: Gpu = gpu(0, 2, INTEL, 0x9a49, 0x01).integrated(Some(true));
const IRIS_XE_RPL: Gpu = gpu(0, 2, INTEL, 0xa7a0, 0x04).integrated(Some(true));
const UHD_770: Gpu = gpu(0, 2, INTEL, 0xa780, 0x04).integrated(Some(true));
/// A server's BMC display, which no stack drives.
const BMC: Gpu = gpu(2, 0, ASPEED, 0x2000, 0x52).not_driven("");

/// The NVIDIA claim's refusal when the USB holds no firmware for its chip.
const NO_FIRMWARE: &str = "no pinned firmware for the observed GPU chipset was loaded";

/// The first error Linux reports in a failed probe, as the tests have it.
fn probe_error(gpu: &Gpu) -> String {
    format!(
        "{} {}: probe failed with error -5",
        gpu.driver(),
        gpu.adapter().address
    )
}

/// A boot of a PC as the engine runs it: the claims, the probes, the
/// fallbacks after a GPU chosen to draw failed, the Metal devices.
struct Booting<'a> {
    pc: &'a [Gpu],
    adapters: Vec<Adapter>,
    found: Vec<Found>,
    names: Vec<String>,
    chosen: Selection,
    /// The census indices of the GPUs that failed after their claim.
    failed: Vec<usize>,
    /// The bound GPUs, by census index, in the order they bound.
    bound: Vec<usize>,
    /// The claimed GPUs no driver bound, by census index.
    unbound: Vec<usize>,
    /// The unbound drawing GPUs the screen has not named yet: the fallback
    /// that may take their place names them (`warn_one`).
    pending: Vec<usize>,
    screen: Vec<(Mark, String)>,
    log: Vec<String>,
}

impl Booting<'_> {
    fn draws(&self, index: usize) -> bool {
        self.found[index].stack.is_ok_and(|driven| driven.draws)
    }

    /// Each GPU's probe, in the order given; a driver that does not bind its
    /// GPU says so as it happens (`warn_unbound`), a drawing GPU's after the
    /// fallback that may take its place.
    fn probe(&mut self, indices: &[usize]) {
        for &index in indices {
            if self.pc[index].unbound {
                self.unbound.push(index);
                match self.draws(index) {
                    true => self.pending.push(index),
                    false => self.warn_one(index, false),
                }
            } else {
                self.bound.push(index);
            }
        }
    }

    /// The screen's line for the unbound GPU `index`, with why it did not
    /// start where another GPU takes its place (`replaced`).
    fn warn_one(&mut self, index: usize, replaced: bool) {
        let gpu = &self.pc[index];
        let picture = match gpu.screen {
            true => Picture::ResetForClaim,
            false => Picture::Elsewhere,
        };
        let address = self.adapters[index].address.to_string();
        let cause = replaced.then(|| probe_error(gpu));
        let line = unbound(
            &self.names[index],
            &address,
            gpu.driver(),
            picture,
            cause.as_deref(),
        );
        self.screen.push((Mark::Warn, line));
        self.pending.retain(|&pending| pending != index);
    }

    /// A fallback's claims after the GPUs of `stages` failed, as the
    /// engine's `Fallback` makes them: the GPUs claimed newly.
    fn fall_back(&mut self, stages: &[(usize, Stage)]) -> Vec<usize> {
        let before = self.chosen.clone();
        self.failed.extend(stages.iter().map(|&(index, _)| index));
        // The failed GPUs' lines first, with why where a GPU takes their
        // place: claimed now, or started already and drawing.
        let ideal = fall_back(&self.found, &before, &self.failed, |_| true);
        let replaced = !stela_drivers::gpu::selection::newly_claimed(&before, &ideal).is_empty()
            || ideal.claimed.iter().any(|&index| {
                ideal.choices[index] == Choice::First
                    && self.draws(index)
                    && !self.failed.contains(&index)
            });
        for &(index, _) in stages {
            if self.pending.contains(&index) {
                self.warn_one(index, replaced);
            }
        }
        let mut refused = Vec::new();
        let after = fall_back(&self.found, &before, &self.failed, |index| {
            match self.pc[index].refused {
                Some((reason, why)) => {
                    let draws = self.found[index].stack.is_ok_and(|driven| driven.draws);
                    refused.push(Refused::new(&self.adapters[index], draws, reason, why));
                    false
                }
                None => true,
            }
        });
        let drawing = after.claimed.iter().copied().find(|&index| {
            after.choices[index] == Choice::First
                && self.draws(index)
                && !self.failed.contains(&index)
        });
        let next = Next::of(&before, &after, drawing);
        for (index, stage) in stages {
            self.log.push(format!(
                "GPU FALLBACK {}",
                fallback_line(&self.adapters, *index, stage, &next)
            ));
        }
        self.screen
            .extend(refused.iter().map(|gpu| (Mark::Warn, gpu.line())));
        let claimed = stela_drivers::gpu::selection::newly_claimed(&before, &after);
        self.chosen = after;
        claimed
    }

    /// While a GPU chosen to draw is unbound and has not failed yet, the
    /// GPUs that draw in its place are claimed and probed.
    fn fall_back_after_probe(&mut self) {
        loop {
            let stages: Vec<(usize, Stage)> = self
                .unbound
                .iter()
                .copied()
                .filter(|&index| self.draws(index) && !self.failed.contains(&index))
                .map(|index| {
                    let gpu = &self.pc[index];
                    (
                        index,
                        Stage::Probe {
                            what: format!("{} did not bind it", gpu.driver()),
                            error: Some(probe_error(gpu)),
                        },
                    )
                })
                .collect();
            if stages.is_empty() {
                break;
            }
            let claimed = self.fall_back(&stages);
            if claimed.is_empty() {
                break;
            }
            self.probe(&claimed);
        }
        for index in self.pending.clone() {
            self.warn_one(index, false);
        }
    }

    /// The step's bound GPUs, in the order they bound.
    fn bound(&self) -> Vec<Bound<'_>> {
        self.bound
            .iter()
            .map(|&index| Bound {
                market: &self.names[index],
                draws: true,
                integrated: self.pc[index].integrated,
                shows_screen: self.pc[index].screen,
            })
            .collect()
    }

    /// The bound GPUs' Metal choice, `display` the bound index whose output
    /// took the display.
    fn metal(&self, display: Option<usize>) -> stela_drivers::gpu::stack::MetalDevices {
        let candidates: Vec<Candidate> = self
            .bound()
            .iter()
            .enumerate()
            .map(|(index, gpu)| Candidate {
                draws: gpu.draws,
                integrated: gpu.integrated,
                drives_display: display == Some(index),
            })
            .collect();
        metal_devices(&candidates)
    }

    /// The services that open on the bound GPUs from bound index `first`
    /// on, as `gate::open_from` opens them: the bound index of each that
    /// opened, and the drawing GPUs that did not, with why.
    fn open_from(
        &self,
        first: usize,
        display: Option<usize>,
    ) -> (Vec<usize>, Vec<(usize, String)>) {
        let chosen = self.metal(display);
        let mut failed = Vec::new();
        let mut open = |indices: &[usize]| -> Vec<usize> {
            indices
                .iter()
                .copied()
                .filter(|&index| index >= first)
                .filter(|&index| match self.pc[self.bound[index]].no_metal {
                    Some(why) => {
                        failed.push((index, String::from(why)));
                        false
                    }
                    None => true,
                })
                .collect()
        };
        let mut opened = open(&chosen.serve);
        if opened.is_empty() {
            opened = open(&chosen.display_only);
        }
        (opened, failed)
    }
}

/// The boot screen's lines from the GPU step on, as `status` draws them,
/// and the log's `GPU SELECT` and `GPU FALLBACK` lines, of `pc` booted by
/// `build`, as the engine boots it: the claims, the probes in two turns,
/// a fallback after each stage a GPU chosen to draw fails at, the display
/// and the Metal devices.
fn boot(pc: &[Gpu], build: Build) -> (Vec<String>, Vec<String>) {
    let adapters: Vec<Adapter> = pc.iter().map(Gpu::adapter).collect();
    let found: Vec<Found> = pc.iter().map(|gpu| gpu.found(build)).collect();
    let mut refused = Vec::new();
    let chosen = select(&found, |index| match pc[index].refused {
        Some((reason, why)) => {
            let draws = found[index].stack.is_ok_and(|driven| driven.draws);
            refused.push(Refused::new(&adapters[index], draws, reason, why));
            false
        }
        None => true,
    });
    let mut log: Vec<String> = selection_lines(&adapters, &found, &chosen, &refused)
        .into_iter()
        .map(|line| format!("GPU SELECT {line}"))
        .collect();
    log.extend(
        refusal_lines(&adapters, &found, &chosen, &refused)
            .into_iter()
            .map(|line| format!("GPU FALLBACK {line}")),
    );
    let ports: &[Vendor] = match build {
        Build::Release => &[Vendor::Nvidia],
        Build::Full => &[Vendor::Nvidia, Vendor::Amd, Vendor::Intel],
    };
    let names: Vec<String> = adapters.iter().map(Adapter::name).collect();
    let mut booting = Booting {
        pc,
        adapters,
        found,
        names,
        chosen,
        failed: Vec::new(),
        bound: Vec::new(),
        unbound: Vec::new(),
        pending: Vec::new(),
        screen: Vec::new(),
        log,
    };
    if booting.chosen.claimed.is_empty() {
        let step = not_started(&booting.adapters, &refused, ports);
        return finish(booting, step, None);
    }
    // The first turn, then the GPUs that may only show another's frames.
    let (later, first): (Vec<usize>, Vec<usize>) = booting
        .chosen
        .claimed
        .iter()
        .copied()
        .partition(|&index| matches!(booting.chosen.choices[index], Choice::Later { .. }));
    booting.probe(&first);
    booting.probe(&later);
    booting.fall_back_after_probe();
    let step = match booting.bound.is_empty() {
        // The engine's `Error::NoDevices`: each GPU said why above.
        true => vec![(Mark::Warn, String::from(crate::gpu_status::NONE_BOUND))],
        false => {
            let failed_names: Vec<&str> = booting
                .unbound
                .iter()
                .filter(|&&index| booting.draws(index))
                .map(|&index| booting.names[index].as_str())
                .collect();
            let dark = booting
                .unbound
                .iter()
                .find(|&&index| pc[index].screen)
                .map(|&index| booting.names[index].as_str());
            started(&booting.bound(), &failed_names, &refused, dark)
        }
    };
    if booting.bound.is_empty() {
        return finish(booting, step, None);
    }
    let mut lines = step.into_iter();
    let (mark, result) = lines.next().unwrap();
    booting
        .screen
        .push((mark, finished_step(TITLE, None, &result, 0)));
    booting.screen.extend(lines);
    // The display: the first bound GPU with a screen on its outputs.
    let display = booting.bound.iter().position(|&index| pc[index].screen);
    let (mut served, mut failed) = booting.open_from(0, display);
    // A drawing GPU whose Metal device did not open gives way.
    loop {
        let stages: Vec<(usize, Stage)> = failed
            .iter()
            .map(|(index, why)| {
                let census = booting.bound[*index];
                (
                    census,
                    Stage::Metal {
                        driver: String::from(pc[census].driver()),
                        error: String::from(why),
                    },
                )
            })
            .filter(|(census, _)| !booting.failed.contains(census))
            .collect();
        if stages.is_empty() {
            break;
        }
        if let Some(&first) = served.first() {
            let drawing = booting.bound[first];
            for (index, stage) in &stages {
                let line = fallback_line(&booting.adapters, *index, stage, &Next::Drawing(drawing));
                booting.log.push(format!("GPU FALLBACK {line}"));
            }
            booting
                .failed
                .extend(stages.iter().map(|&(index, _)| index));
            break;
        }
        let from = booting.bound.len();
        let claimed = booting.fall_back(&stages);
        if claimed.is_empty() {
            break;
        }
        booting.probe(&claimed);
        booting.fall_back_after_probe();
        if booting.bound.len() == from {
            break;
        }
        let display = display.or_else(|| {
            booting.bound[from..]
                .iter()
                .position(|&index| pc[index].screen)
                .map(|position| from + position)
        });
        let more = booting.open_from(from, display);
        served.extend(more.0);
        failed = more.1;
    }
    // `report_drawer`: the GPU chosen to draw opened no Metal device and
    // another did.
    let wanted = booting.metal(display).serve.first().copied();
    if let (Some(wanted), Some(&served)) = (wanted, served.first())
        && wanted != served
    {
        let line = crate::gpu_status::draws_instead(
            &booting.names[booting.bound[wanted]],
            &booting.names[booting.bound[served]],
            pc[booting.bound[wanted]].no_metal,
        );
        booting.screen.push((Mark::Warn, line));
    }
    let drawer = served.first().map(|&index| booting.bound[index]);
    finish(
        booting,
        Vec::new(),
        drawer.map(|drawer| (drawer, display.is_some())),
    )
}

/// The boot's screen and log once `step`, the step's lines where the boot
/// ended before the display, is shown: with `drawn` (the census index of
/// the GPU that draws, and whether a GPU's output took the display), a
/// picture copied to the firmware's screen when it still shows.
fn finish(
    mut booting: Booting<'_>,
    step: Vec<(Mark, String)>,
    drawn: Option<(usize, bool)>,
) -> (Vec<String>, Vec<String>) {
    let mut lines = step.into_iter();
    if let Some((mark, result)) = lines.next() {
        booting
            .screen
            .push((mark, finished_step(TITLE, None, &result, 0)));
        booting.screen.extend(lines);
    }
    // The firmware's screen still shows where no claimed GPU that holds it
    // was reset for its driver.
    let dark = booting
        .bound
        .iter()
        .chain(&booting.unbound)
        .any(|&index| booting.pc[index].screen);
    if let Some((drawer, false)) = drawn
        && !dark
        && booting.pc.iter().any(|gpu| gpu.screen)
    {
        let line = shown_by_copy(&booting.names[drawer]);
        booting.screen.push((Mark::Info, line));
    }
    let screen = booting
        .screen
        .into_iter()
        .map(|(mark, text)| match mark {
            Mark::Ok => format!("[  ok  ]  {text}"),
            Mark::Warn => format!("[ warn ]  {text}"),
            Mark::Info => format!("          {text}"),
        })
        .collect();
    (screen, booting.log)
}

/// The text of each PC, booted by each build.
fn text(pc: &[Gpu], build: Build) -> String {
    let (screen, log) = boot(pc, build);
    format!("{}\n--\n{}", screen.join("\n"), log.join("\n"))
}

/// The PC's whole text booted by `build`: the screen from the GPU step on,
/// then the log's `GPU SELECT` lines.
fn assert_boots(pc: &[Gpu], build: Build, expected: &str) {
    assert_eq!(text(pc, build), expected, "{build:?}");
}

/// An NVIDIA card alone draws in either build.
#[test]
fn a_desktop_with_an_nvidia_card_alone() {
    let pc: &[Gpu] = &[RTX_5080.screen()];
    let both = r#"[  ok  ]  Starting the GPU (NVIDIA GeForce RTX 5080)
--
GPU SELECT 0000:01:00.0 10de:2c02 rev a1 NVIDIA GeForce RTX 5080: discrete, draws; started first
GPU SELECT started in this order: NVIDIA GeForce RTX 5080 at 0000:01:00.0. A discrete GPU that draws gives the Metal device before an integrated one, and the next one does where it fails; the display is the first of them with a connected display, else the firmware's screen"#;
    assert_boots(pc, Build::Release, both);
    assert_boots(pc, Build::Full, both);
}

/// An AMD card alone: a release names it and asks for an NVIDIA GPU; the full build draws on it.
#[test]
fn a_desktop_with_an_amd_card_alone() {
    let pc: &[Gpu] = &[RX_7900_XTX.screen()];
    assert_boots(
        pc,
        Build::Release,
        r#"[ warn ]  Starting the GPU (this PC has no NVIDIA GPU, only the AMD Radeon RX 7900 XTX: Stela needs an NVIDIA GeForce GTX 16 or RTX 20, 30, 40 or 50 series GPU)
--
GPU SELECT 0000:03:00.0 1002:744c rev c8 AMD Radeon RX 7900 XTX: not used: AMDGPU is not ported to Stela
GPU SELECT started none"#,
    );
    assert_boots(
        pc,
        Build::Full,
        r#"[  ok  ]  Starting the GPU (AMD Radeon RX 7900 XTX)
--
GPU SELECT 0000:03:00.0 1002:744c rev c8 AMD Radeon RX 7900 XTX: integrated or discrete, as its probe tells, draws; started first
GPU SELECT started in this order: AMD Radeon RX 7900 XTX at 0000:03:00.0. A discrete GPU that draws gives the Metal device before an integrated one, and the next one does where it fails; the display is the first of them with a connected display, else the firmware's screen"#,
    );
}

/// An Intel Arc card alone, as an AMD card.
#[test]
fn a_desktop_with_an_intel_arc_card_alone() {
    let pc: &[Gpu] = &[ARC_A770.screen()];
    assert_boots(
        pc,
        Build::Release,
        r#"[ warn ]  Starting the GPU (this PC has no NVIDIA GPU, only the Intel Arc A770: Stela needs an NVIDIA GeForce GTX 16 or RTX 20, 30, 40 or 50 series GPU)
--
GPU SELECT 0000:03:00.0 8086:56a0 rev 08 Intel Arc A770: not used: no driver here runs Stela's Metal device on this Intel GPU
GPU SELECT started none"#,
    );
    assert_boots(
        pc,
        Build::Full,
        r#"[  ok  ]  Starting the GPU (Intel Arc A770)
--
GPU SELECT 0000:03:00.0 8086:56a0 rev 08 Intel Arc A770: discrete, draws; started first
GPU SELECT started in this order: Intel Arc A770 at 0000:03:00.0. A discrete GPU that draws gives the Metal device before an integrated one, and the next one does where it fails; the display is the first of them with a connected display, else the firmware's screen"#,
    );
}

/// A Ryzen's Radeon alone, as an AMD card.
#[test]
fn a_pc_with_an_amd_integrated_gpu_alone() {
    let pc: &[Gpu] = &[RADEON_780M.screen()];
    assert_boots(
        pc,
        Build::Release,
        r#"[ warn ]  Starting the GPU (this PC has no NVIDIA GPU, only the AMD Radeon 780M Graphics: Stela needs an NVIDIA GeForce GTX 16 or RTX 20, 30, 40 or 50 series GPU)
--
GPU SELECT 0000:65:00.0 1002:15bf rev c1 AMD Radeon 780M Graphics: not used: AMDGPU is not ported to Stela
GPU SELECT started none"#,
    );
    assert_boots(
        pc,
        Build::Full,
        r#"[  ok  ]  Starting the GPU (AMD Radeon 780M Graphics)
--
GPU SELECT 0000:65:00.0 1002:15bf rev c1 AMD Radeon 780M Graphics: integrated or discrete, as its probe tells, draws; started first
GPU SELECT started in this order: AMD Radeon 780M Graphics at 0000:65:00.0. A discrete GPU that draws gives the Metal device before an integrated one, and the next one does where it fails; the display is the first of them with a connected display, else the firmware's screen"#,
    );
}

/// An Intel processor's GPU alone, as an AMD card.
#[test]
fn a_pc_with_an_intel_integrated_gpu_alone() {
    let pc: &[Gpu] = &[IRIS_XE.screen()];
    assert_boots(
        pc,
        Build::Release,
        r#"[ warn ]  Starting the GPU (this PC has no NVIDIA GPU, only the Intel Iris Xe Graphics: Stela needs an NVIDIA GeForce GTX 16 or RTX 20, 30, 40 or 50 series GPU)
--
GPU SELECT 0000:00:02.0 8086:9a49 rev 01 Intel Iris Xe Graphics: not used: no driver here runs Stela's Metal device on this Intel GPU
GPU SELECT started none"#,
    );
    assert_boots(
        pc,
        Build::Full,
        r#"[  ok  ]  Starting the GPU (Intel Iris Xe Graphics)
--
GPU SELECT 0000:00:02.0 8086:9a49 rev 01 Intel Iris Xe Graphics: integrated, draws; started first
GPU SELECT started in this order: Intel Iris Xe Graphics at 0000:00:02.0. A discrete GPU that draws gives the Metal device before an integrated one, and the next one does where it fails; the display is the first of them with a connected display, else the firmware's screen"#,
    );
}

/// A laptop without a display mux: the NVIDIA GPU draws; the full build starts the Intel GPU to show the panel, a release copies each frame to the panel the firmware lit.
#[test]
fn a_laptop_with_nvidia_and_intel_gpus_and_the_panel_on_the_intel_one() {
    let pc: &[Gpu] = &[IRIS_XE_RPL.screen(), RTX_4060_LAPTOP];
    assert_boots(
        pc,
        Build::Release,
        r#"[  ok  ]  Starting the GPU (NVIDIA GeForce RTX 4060 Laptop GPU)
          No screen is connected to the NVIDIA GeForce RTX 4060 Laptop GPU, so Stela copies each frame of macOS to the screen the PC started on. The picture is slower than on a screen connected to the GPU.
--
GPU SELECT 0000:00:02.0 8086:a7a0 rev 04 Intel Iris Xe Graphics: not used: no driver here runs Stela's Metal device on this Intel GPU
GPU SELECT 0000:01:00.0 10de:28e0 rev a1 NVIDIA GeForce RTX 4060 Laptop GPU: discrete, draws; started first
GPU SELECT started in this order: NVIDIA GeForce RTX 4060 Laptop GPU at 0000:01:00.0. A discrete GPU that draws gives the Metal device before an integrated one, and the next one does where it fails; the display is the first of them with a connected display, else the firmware's screen"#,
    );
    assert_boots(
        pc,
        Build::Full,
        r#"[  ok  ]  Starting the GPU (NVIDIA GeForce RTX 4060 Laptop GPU)
          the Intel Iris Xe Graphics shows the picture on the screens wired to it
--
GPU SELECT 0000:00:02.0 8086:a7a0 rev 04 Intel Iris Xe Graphics: integrated, draws; started after NVIDIA GeForce RTX 4060 Laptop GPU at 0000:01:00.0: it may only show the frames that GPU draws
GPU SELECT 0000:01:00.0 10de:28e0 rev a1 NVIDIA GeForce RTX 4060 Laptop GPU: discrete, draws; started first
GPU SELECT started in this order: NVIDIA GeForce RTX 4060 Laptop GPU at 0000:01:00.0, Intel Iris Xe Graphics at 0000:00:02.0. A discrete GPU that draws gives the Metal device before an integrated one, and the next one does where it fails; the display is the first of them with a connected display, else the firmware's screen"#,
    );
}

/// Its mux set to the NVIDIA GPU: the Intel GPU is left alone, and named in the log only.
#[test]
fn a_laptop_with_nvidia_and_intel_gpus_and_the_mux_on_the_nvidia_one() {
    let pc: &[Gpu] = &[IRIS_XE_RPL, RTX_4060_LAPTOP.screen()];
    assert_boots(
        pc,
        Build::Release,
        r#"[  ok  ]  Starting the GPU (NVIDIA GeForce RTX 4060 Laptop GPU)
--
GPU SELECT 0000:00:02.0 8086:a7a0 rev 04 Intel Iris Xe Graphics: not used: no driver here runs Stela's Metal device on this Intel GPU
GPU SELECT 0000:01:00.0 10de:28e0 rev a1 NVIDIA GeForce RTX 4060 Laptop GPU: discrete, draws; started first
GPU SELECT started in this order: NVIDIA GeForce RTX 4060 Laptop GPU at 0000:01:00.0. A discrete GPU that draws gives the Metal device before an integrated one, and the next one does where it fails; the display is the first of them with a connected display, else the firmware's screen"#,
    );
    assert_boots(
        pc,
        Build::Full,
        r#"[  ok  ]  Starting the GPU (NVIDIA GeForce RTX 4060 Laptop GPU)
--
GPU SELECT 0000:00:02.0 8086:a7a0 rev 04 Intel Iris Xe Graphics: integrated, draws; left with the firmware: it would only show the frames NVIDIA GeForce RTX 4060 Laptop GPU at 0000:01:00.0 draws, and the firmware's picture is not on it
GPU SELECT 0000:01:00.0 10de:28e0 rev a1 NVIDIA GeForce RTX 4060 Laptop GPU: discrete, draws; started first
GPU SELECT started in this order: NVIDIA GeForce RTX 4060 Laptop GPU at 0000:01:00.0. A discrete GPU that draws gives the Metal device before an integrated one, and the next one does where it fails; the display is the first of them with a connected display, else the firmware's screen"#,
    );
}

/// An NVIDIA GPU beside a Ryzen's Radeon with the panel on it.
#[test]
fn a_laptop_with_nvidia_and_amd_gpus_and_the_panel_on_the_amd_one() {
    let pc: &[Gpu] = &[RTX_4060_LAPTOP, RADEON_780M.screen()];
    assert_boots(
        pc,
        Build::Release,
        r#"[  ok  ]  Starting the GPU (NVIDIA GeForce RTX 4060 Laptop GPU)
          No screen is connected to the NVIDIA GeForce RTX 4060 Laptop GPU, so Stela copies each frame of macOS to the screen the PC started on. The picture is slower than on a screen connected to the GPU.
--
GPU SELECT 0000:01:00.0 10de:28e0 rev a1 NVIDIA GeForce RTX 4060 Laptop GPU: discrete, draws; started first
GPU SELECT 0000:65:00.0 1002:15bf rev c1 AMD Radeon 780M Graphics: not used: AMDGPU is not ported to Stela
GPU SELECT started in this order: NVIDIA GeForce RTX 4060 Laptop GPU at 0000:01:00.0. A discrete GPU that draws gives the Metal device before an integrated one, and the next one does where it fails; the display is the first of them with a connected display, else the firmware's screen"#,
    );
    assert_boots(
        pc,
        Build::Full,
        r#"[  ok  ]  Starting the GPU (NVIDIA GeForce RTX 4060 Laptop GPU)
          the AMD Radeon 780M Graphics shows the picture on the screens wired to it
--
GPU SELECT 0000:01:00.0 10de:28e0 rev a1 NVIDIA GeForce RTX 4060 Laptop GPU: discrete, draws; started first
GPU SELECT 0000:65:00.0 1002:15bf rev c1 AMD Radeon 780M Graphics: integrated or discrete, as its probe tells, draws; started after NVIDIA GeForce RTX 4060 Laptop GPU at 0000:01:00.0: it may only show the frames that GPU draws
GPU SELECT started in this order: NVIDIA GeForce RTX 4060 Laptop GPU at 0000:01:00.0, AMD Radeon 780M Graphics at 0000:65:00.0. A discrete GPU that draws gives the Metal device before an integrated one, and the next one does where it fails; the display is the first of them with a connected display, else the firmware's screen"#,
    );
}

/// The mux on the NVIDIA GPU: the Radeon, whose kind only its probe tells, starts after it and has no line on the screen, as no screen hangs on it.
#[test]
fn a_laptop_with_nvidia_and_amd_gpus_and_the_mux_on_the_nvidia_one() {
    let pc: &[Gpu] = &[RTX_4060_LAPTOP.screen(), RADEON_780M];
    assert_boots(
        pc,
        Build::Release,
        r#"[  ok  ]  Starting the GPU (NVIDIA GeForce RTX 4060 Laptop GPU)
--
GPU SELECT 0000:01:00.0 10de:28e0 rev a1 NVIDIA GeForce RTX 4060 Laptop GPU: discrete, draws; started first
GPU SELECT 0000:65:00.0 1002:15bf rev c1 AMD Radeon 780M Graphics: not used: AMDGPU is not ported to Stela
GPU SELECT started in this order: NVIDIA GeForce RTX 4060 Laptop GPU at 0000:01:00.0. A discrete GPU that draws gives the Metal device before an integrated one, and the next one does where it fails; the display is the first of them with a connected display, else the firmware's screen"#,
    );
    assert_boots(
        pc,
        Build::Full,
        r#"[  ok  ]  Starting the GPU (NVIDIA GeForce RTX 4060 Laptop GPU)
--
GPU SELECT 0000:01:00.0 10de:28e0 rev a1 NVIDIA GeForce RTX 4060 Laptop GPU: discrete, draws; started first
GPU SELECT 0000:65:00.0 1002:15bf rev c1 AMD Radeon 780M Graphics: integrated or discrete, as its probe tells, draws; started after NVIDIA GeForce RTX 4060 Laptop GPU at 0000:01:00.0: it may only show the frames that GPU draws
GPU SELECT started in this order: NVIDIA GeForce RTX 4060 Laptop GPU at 0000:01:00.0, AMD Radeon 780M Graphics at 0000:65:00.0. A discrete GPU that draws gives the Metal device before an integrated one, and the next one does where it fails; the display is the first of them with a connected display, else the firmware's screen"#,
    );
}

/// Two AMD GPUs whose kinds their probes tell: both start, the discrete one draws.
#[test]
fn a_laptop_with_two_amd_gpus_and_the_panel_on_the_integrated_one() {
    let pc: &[Gpu] = &[RX_7600M_XT, RADEON_780M.screen()];
    assert_boots(
        pc,
        Build::Release,
        r#"[ warn ]  Starting the GPU (this PC has no NVIDIA GPU, only the AMD Radeon RX 7600M XT and the AMD Radeon 780M Graphics: Stela needs an NVIDIA GeForce GTX 16 or RTX 20, 30, 40 or 50 series GPU)
--
GPU SELECT 0000:03:00.0 1002:7480 rev c7 AMD Radeon RX 7600M XT: not used: AMDGPU is not ported to Stela
GPU SELECT 0000:65:00.0 1002:15bf rev c1 AMD Radeon 780M Graphics: not used: AMDGPU is not ported to Stela
GPU SELECT started none"#,
    );
    assert_boots(
        pc,
        Build::Full,
        r#"[  ok  ]  Starting the GPU (AMD Radeon RX 7600M XT)
          the AMD Radeon 780M Graphics shows the picture on the screens wired to it
--
GPU SELECT 0000:03:00.0 1002:7480 rev c7 AMD Radeon RX 7600M XT: integrated or discrete, as its probe tells, draws; started first
GPU SELECT 0000:65:00.0 1002:15bf rev c1 AMD Radeon 780M Graphics: integrated or discrete, as its probe tells, draws; started first
GPU SELECT started in this order: AMD Radeon RX 7600M XT at 0000:03:00.0, AMD Radeon 780M Graphics at 0000:65:00.0. A discrete GPU that draws gives the Metal device before an integrated one, and the next one does where it fails; the display is the first of them with a connected display, else the firmware's screen"#,
    );
}

/// The same with the mux on the discrete GPU.
#[test]
fn a_laptop_with_two_amd_gpus_and_the_mux_on_the_discrete_one() {
    let pc: &[Gpu] = &[RX_7600M_XT.screen(), RADEON_780M];
    assert_boots(
        pc,
        Build::Release,
        r#"[ warn ]  Starting the GPU (this PC has no NVIDIA GPU, only the AMD Radeon RX 7600M XT and the AMD Radeon 780M Graphics: Stela needs an NVIDIA GeForce GTX 16 or RTX 20, 30, 40 or 50 series GPU)
--
GPU SELECT 0000:03:00.0 1002:7480 rev c7 AMD Radeon RX 7600M XT: not used: AMDGPU is not ported to Stela
GPU SELECT 0000:65:00.0 1002:15bf rev c1 AMD Radeon 780M Graphics: not used: AMDGPU is not ported to Stela
GPU SELECT started none"#,
    );
    assert_boots(
        pc,
        Build::Full,
        r#"[  ok  ]  Starting the GPU (AMD Radeon RX 7600M XT)
--
GPU SELECT 0000:03:00.0 1002:7480 rev c7 AMD Radeon RX 7600M XT: integrated or discrete, as its probe tells, draws; started first
GPU SELECT 0000:65:00.0 1002:15bf rev c1 AMD Radeon 780M Graphics: integrated or discrete, as its probe tells, draws; started first
GPU SELECT started in this order: AMD Radeon RX 7600M XT at 0000:03:00.0, AMD Radeon 780M Graphics at 0000:65:00.0. A discrete GPU that draws gives the Metal device before an integrated one, and the next one does where it fails; the display is the first of them with a connected display, else the firmware's screen"#,
    );
}

/// A desktop's monitor on its card: the integrated GPU is left alone.
#[test]
fn a_desktop_with_a_card_and_an_integrated_gpu_and_the_monitor_on_the_card() {
    let pc: &[Gpu] = &[UHD_770, RTX_5080.screen()];
    assert_boots(
        pc,
        Build::Release,
        r#"[  ok  ]  Starting the GPU (NVIDIA GeForce RTX 5080)
--
GPU SELECT 0000:00:02.0 8086:a780 rev 04 Intel UHD Graphics 770: not used: no driver here runs Stela's Metal device on this Intel GPU
GPU SELECT 0000:01:00.0 10de:2c02 rev a1 NVIDIA GeForce RTX 5080: discrete, draws; started first
GPU SELECT started in this order: NVIDIA GeForce RTX 5080 at 0000:01:00.0. A discrete GPU that draws gives the Metal device before an integrated one, and the next one does where it fails; the display is the first of them with a connected display, else the firmware's screen"#,
    );
    assert_boots(
        pc,
        Build::Full,
        r#"[  ok  ]  Starting the GPU (NVIDIA GeForce RTX 5080)
--
GPU SELECT 0000:00:02.0 8086:a780 rev 04 Intel UHD Graphics 770: integrated, draws; left with the firmware: it would only show the frames NVIDIA GeForce RTX 5080 at 0000:01:00.0 draws, and the firmware's picture is not on it
GPU SELECT 0000:01:00.0 10de:2c02 rev a1 NVIDIA GeForce RTX 5080: discrete, draws; started first
GPU SELECT started in this order: NVIDIA GeForce RTX 5080 at 0000:01:00.0. A discrete GPU that draws gives the Metal device before an integrated one, and the next one does where it fails; the display is the first of them with a connected display, else the firmware's screen"#,
    );
}

/// Its monitor on the motherboard: the card draws, the integrated GPU shows the picture, or in a release each frame is copied there.
#[test]
fn a_desktop_with_a_card_and_an_integrated_gpu_and_the_monitor_on_the_motherboard() {
    let pc: &[Gpu] = &[UHD_770.screen(), RTX_5080];
    assert_boots(
        pc,
        Build::Release,
        r#"[  ok  ]  Starting the GPU (NVIDIA GeForce RTX 5080)
          No screen is connected to the NVIDIA GeForce RTX 5080, so Stela copies each frame of macOS to the screen the PC started on. The picture is slower than on a screen connected to the GPU.
--
GPU SELECT 0000:00:02.0 8086:a780 rev 04 Intel UHD Graphics 770: not used: no driver here runs Stela's Metal device on this Intel GPU
GPU SELECT 0000:01:00.0 10de:2c02 rev a1 NVIDIA GeForce RTX 5080: discrete, draws; started first
GPU SELECT started in this order: NVIDIA GeForce RTX 5080 at 0000:01:00.0. A discrete GPU that draws gives the Metal device before an integrated one, and the next one does where it fails; the display is the first of them with a connected display, else the firmware's screen"#,
    );
    assert_boots(
        pc,
        Build::Full,
        r#"[  ok  ]  Starting the GPU (NVIDIA GeForce RTX 5080)
          the Intel UHD Graphics 770 shows the picture on the screens wired to it
--
GPU SELECT 0000:00:02.0 8086:a780 rev 04 Intel UHD Graphics 770: integrated, draws; started after NVIDIA GeForce RTX 5080 at 0000:01:00.0: it may only show the frames that GPU draws
GPU SELECT 0000:01:00.0 10de:2c02 rev a1 NVIDIA GeForce RTX 5080: discrete, draws; started first
GPU SELECT started in this order: NVIDIA GeForce RTX 5080 at 0000:01:00.0, Intel UHD Graphics 770 at 0000:00:02.0. A discrete GPU that draws gives the Metal device before an integrated one, and the next one does where it fails; the display is the first of them with a connected display, else the firmware's screen"#,
    );
}

/// A card whose claim is refused: the integrated GPU takes its place and the screen says so.
#[test]
fn a_card_whose_claim_is_refused_leaves_the_integrated_gpu_to_draw() {
    let pc: &[Gpu] = &[
        UHD_770,
        RTX_5080
            .screen()
            .refused(Reason::Other, "the GPU's register BAR does not decode"),
    ];
    assert_boots(
        pc,
        Build::Release,
        r#"[ warn ]  Starting the GPU (the NVIDIA GeForce RTX 5080 at 0000:01:00.0 did not start: the GPU's register BAR does not decode. The boot log on this USB has the rest)
--
GPU SELECT 0000:00:02.0 8086:a780 rev 04 Intel UHD Graphics 770: not used: no driver here runs Stela's Metal device on this Intel GPU
GPU SELECT 0000:01:00.0 10de:2c02 rev a1 NVIDIA GeForce RTX 5080: discrete, draws; not started: its claim was refused: the GPU's register BAR does not decode
GPU SELECT started none
GPU FALLBACK 0000:01:00.0 NVIDIA GeForce RTX 5080: its claim was refused: the GPU's register BAR does not decode; no other GPU of this PC can draw in its place"#,
    );
    assert_boots(
        pc,
        Build::Full,
        r#"[  ok  ]  Starting the GPU (Intel UHD Graphics 770, as the NVIDIA GeForce RTX 5080 did not start)
[ warn ]  the NVIDIA GeForce RTX 5080 at 0000:01:00.0 did not start: the GPU's register BAR does not decode. The boot log on this USB has the rest
          No screen is connected to the Intel UHD Graphics 770, so Stela copies each frame of macOS to the screen the PC started on. The picture is slower than on a screen connected to the GPU.
--
GPU SELECT 0000:00:02.0 8086:a780 rev 04 Intel UHD Graphics 770: integrated, draws; started first
GPU SELECT 0000:01:00.0 10de:2c02 rev a1 NVIDIA GeForce RTX 5080: discrete, draws; not started: its claim was refused: the GPU's register BAR does not decode
GPU SELECT started in this order: Intel UHD Graphics 770 at 0000:00:02.0. A discrete GPU that draws gives the Metal device before an integrated one, and the next one does where it fails; the display is the first of them with a connected display, else the firmware's screen
GPU FALLBACK 0000:01:00.0 NVIDIA GeForce RTX 5080: its claim was refused: the GPU's register BAR does not decode; the Intel UHD Graphics 770 at 0000:00:02.0 is claimed and started in its place"#,
    );
}

/// A card whose driver fails on it beside the integrated GPU that shows the monitor: that one draws.
#[test]
fn a_card_whose_driver_fails_leaves_the_integrated_gpu_that_shows_the_monitor_to_draw() {
    let pc: &[Gpu] = &[UHD_770.screen(), RTX_5080.unbound()];
    assert_boots(
        pc,
        Build::Release,
        r#"[ warn ]  the NVIDIA GeForce RTX 5080 at 0000:01:00.0 did not start: its driver nouveau stopped with an error. The boot log on this USB says why
[ warn ]  Starting the GPU (no GPU started; the lines above say why)
--
GPU SELECT 0000:00:02.0 8086:a780 rev 04 Intel UHD Graphics 770: not used: no driver here runs Stela's Metal device on this Intel GPU
GPU SELECT 0000:01:00.0 10de:2c02 rev a1 NVIDIA GeForce RTX 5080: discrete, draws; started first
GPU SELECT started in this order: NVIDIA GeForce RTX 5080 at 0000:01:00.0. A discrete GPU that draws gives the Metal device before an integrated one, and the next one does where it fails; the display is the first of them with a connected display, else the firmware's screen
GPU FALLBACK 0000:01:00.0 NVIDIA GeForce RTX 5080: its driver's probe failed: nouveau did not bind it; Linux's first error: nouveau 0000:01:00.0: probe failed with error -5; no other GPU of this PC can draw in its place"#,
    );
    assert_boots(
        pc,
        Build::Full,
        r#"[ warn ]  the NVIDIA GeForce RTX 5080 at 0000:01:00.0 did not start: its driver nouveau stopped in its probe: nouveau 0000:01:00.0: probe failed with error -5. The boot log on this USB says more
[  ok  ]  Starting the GPU (Intel UHD Graphics 770, as the NVIDIA GeForce RTX 5080 did not start)
--
GPU SELECT 0000:00:02.0 8086:a780 rev 04 Intel UHD Graphics 770: integrated, draws; started after NVIDIA GeForce RTX 5080 at 0000:01:00.0: it may only show the frames that GPU draws
GPU SELECT 0000:01:00.0 10de:2c02 rev a1 NVIDIA GeForce RTX 5080: discrete, draws; started first
GPU SELECT started in this order: NVIDIA GeForce RTX 5080 at 0000:01:00.0, Intel UHD Graphics 770 at 0000:00:02.0. A discrete GPU that draws gives the Metal device before an integrated one, and the next one does where it fails; the display is the first of them with a connected display, else the firmware's screen
GPU FALLBACK 0000:01:00.0 NVIDIA GeForce RTX 5080: its driver's probe failed: nouveau did not bind it; Linux's first error: nouveau 0000:01:00.0: probe failed with error -5; the Intel UHD Graphics 770 at 0000:00:02.0, started beside it, draws in its place"#,
    );
}

/// A card whose driver fails with the monitor on it: the integrated GPU left with the firmware is claimed and started in its place, and the screen says where macOS shows.
#[test]
fn a_card_whose_driver_fails_with_the_monitor_on_it_gives_way_to_the_integrated_gpu() {
    let pc: &[Gpu] = &[UHD_770, RTX_5080.screen().unbound()];
    assert_boots(
        pc,
        Build::Release,
        r#"[ warn ]  the NVIDIA GeForce RTX 5080 at 0000:01:00.0 did not start: its driver nouveau stopped with an error. The screen the PC started on is wired to it and stays dark: the GPU was reset for its driver. The boot log on this USB says why
[ warn ]  Starting the GPU (no GPU started; the lines above say why)
--
GPU SELECT 0000:00:02.0 8086:a780 rev 04 Intel UHD Graphics 770: not used: no driver here runs Stela's Metal device on this Intel GPU
GPU SELECT 0000:01:00.0 10de:2c02 rev a1 NVIDIA GeForce RTX 5080: discrete, draws; started first
GPU SELECT started in this order: NVIDIA GeForce RTX 5080 at 0000:01:00.0. A discrete GPU that draws gives the Metal device before an integrated one, and the next one does where it fails; the display is the first of them with a connected display, else the firmware's screen
GPU FALLBACK 0000:01:00.0 NVIDIA GeForce RTX 5080: its driver's probe failed: nouveau did not bind it; Linux's first error: nouveau 0000:01:00.0: probe failed with error -5; no other GPU of this PC can draw in its place"#,
    );
    assert_boots(
        pc,
        Build::Full,
        r#"[ warn ]  the NVIDIA GeForce RTX 5080 at 0000:01:00.0 did not start: its driver nouveau stopped in its probe: nouveau 0000:01:00.0: probe failed with error -5. The screen the PC started on is wired to it and stays dark: the GPU was reset for its driver. The boot log on this USB says more
[  ok  ]  Starting the GPU (Intel UHD Graphics 770, as the NVIDIA GeForce RTX 5080 did not start)
[ warn ]  No screen shows macOS yet: the screen the PC started on is wired to the NVIDIA GeForce RTX 5080, which did not start. Connect a screen to the Intel UHD Graphics 770, and macOS shows on it
--
GPU SELECT 0000:00:02.0 8086:a780 rev 04 Intel UHD Graphics 770: integrated, draws; left with the firmware: it would only show the frames NVIDIA GeForce RTX 5080 at 0000:01:00.0 draws, and the firmware's picture is not on it
GPU SELECT 0000:01:00.0 10de:2c02 rev a1 NVIDIA GeForce RTX 5080: discrete, draws; started first
GPU SELECT started in this order: NVIDIA GeForce RTX 5080 at 0000:01:00.0. A discrete GPU that draws gives the Metal device before an integrated one, and the next one does where it fails; the display is the first of them with a connected display, else the firmware's screen
GPU FALLBACK 0000:01:00.0 NVIDIA GeForce RTX 5080: its driver's probe failed: nouveau did not bind it; Linux's first error: nouveau 0000:01:00.0: probe failed with error -5; the Intel UHD Graphics 770 at 0000:00:02.0 is claimed and started in its place"#,
    );
}

/// A server whose firmware shows its picture on the BMC's display: the card draws and each frame is copied there; the BMC is named in the log only.
#[test]
fn a_server_with_a_bmc_display_beside_a_card() {
    let pc: &[Gpu] = &[BMC.screen(), RTX_5080_SERVER];
    let both = r#"[  ok  ]  Starting the GPU (NVIDIA GeForce RTX 5080)
          No screen is connected to the NVIDIA GeForce RTX 5080, so Stela copies each frame of macOS to the screen the PC started on. The picture is slower than on a screen connected to the GPU.
--
GPU SELECT 0000:02:00.0 1a03:2000 rev 52 display adapter: not used: no Stela GPU backend for this PCI vendor
GPU SELECT 0000:41:00.0 10de:2c02 rev a1 NVIDIA GeForce RTX 5080: discrete, draws; started first
GPU SELECT started in this order: NVIDIA GeForce RTX 5080 at 0000:41:00.0. A discrete GPU that draws gives the Metal device before an integrated one, and the next one does where it fails; the display is the first of them with a connected display, else the firmware's screen"#;
    assert_boots(pc, Build::Release, both);
    assert_boots(pc, Build::Full, both);
}

/// A server with only a BMC display.
#[test]
fn a_server_with_only_a_bmc_display() {
    let pc: &[Gpu] = &[BMC.screen()];
    assert_boots(
        pc,
        Build::Release,
        r#"[ warn ]  Starting the GPU (this PC has no NVIDIA GPU: Stela needs an NVIDIA GeForce GTX 16 or RTX 20, 30, 40 or 50 series GPU)
--
GPU SELECT 0000:02:00.0 1a03:2000 rev 52 display adapter: not used: no Stela GPU backend for this PCI vendor
GPU SELECT started none"#,
    );
    assert_boots(
        pc,
        Build::Full,
        r#"[ warn ]  Starting the GPU (this PC has no GPU Stela drives: Stela needs an NVIDIA GeForce GTX 16 or RTX 20, 30, 40 or 50 series GPU, an AMD Radeon RX Vega or later GPU, or an Intel GPU from 6th generation Core processors on)
--
GPU SELECT 0000:02:00.0 1a03:2000 rev 52 display adapter: not used: no Stela GPU backend for this PCI vendor
GPU SELECT started none"#,
    );
}

/// A Radeon RX 580, which no build drives.
#[test]
fn a_desktop_with_only_a_gpu_no_build_drives() {
    let pc: &[Gpu] = &[RX_580.screen()];
    assert_boots(
        pc,
        Build::Release,
        r#"[ warn ]  Starting the GPU (this PC has no NVIDIA GPU, only the AMD Radeon RX 580 Series: Stela needs an NVIDIA GeForce GTX 16 or RTX 20, 30, 40 or 50 series GPU)
--
GPU SELECT 0000:03:00.0 1002:67df rev e7 AMD Radeon RX 580 Series: not used: AMDGPU is not ported to Stela
GPU SELECT started none"#,
    );
    assert_boots(
        pc,
        Build::Full,
        r#"[ warn ]  Starting the GPU (this PC has no GPU Stela drives, only the AMD Radeon RX 580 Series: Stela needs an NVIDIA GeForce GTX 16 or RTX 20, 30, 40 or 50 series GPU, an AMD Radeon RX Vega or later GPU, or an Intel GPU from 6th generation Core processors on)
--
GPU SELECT 0000:03:00.0 1002:67df rev e7 AMD Radeon RX 580 Series: not used: on this GPU's graphics generation (GFX8) Stela's Metal device is in no Apple GPU family: RADV has no raster order groups before GFX9, which the framebuffer reads of Apple3 need, and macOS draws its window server's framebuffers only on an Apple3 device
GPU SELECT started none"#,
    );
}

/// An NVIDIA card whose firmware the USB lacks.
#[test]
fn a_card_refused_for_missing_firmware() {
    let pc: &[Gpu] = &[RTX_5080
        .screen()
        .refused(Reason::MissingFirmware, NO_FIRMWARE)];
    let both = r#"[ warn ]  Starting the GPU (the USB has no firmware for this PC's NVIDIA GeForce RTX 5080. On another computer, plug in this USB and run `stela repair`)
--
GPU SELECT 0000:01:00.0 10de:2c02 rev a1 NVIDIA GeForce RTX 5080: discrete, draws; not started: its claim was refused: no pinned firmware for the observed GPU chipset was loaded
GPU SELECT started none
GPU FALLBACK 0000:01:00.0 NVIDIA GeForce RTX 5080: its claim was refused: no pinned firmware for the observed GPU chipset was loaded; no other GPU of this PC can draw in its place"#;
    assert_boots(pc, Build::Release, both);
    assert_boots(pc, Build::Full, both);
}

/// The same in a laptop: the full build draws on the Intel GPU and says why.
#[test]
fn a_laptop_whose_nvidia_gpu_lacks_firmware_draws_on_the_integrated_one() {
    let pc: &[Gpu] = &[
        IRIS_XE_RPL.screen(),
        RTX_4060_LAPTOP.refused(Reason::MissingFirmware, NO_FIRMWARE),
    ];
    assert_boots(
        pc,
        Build::Release,
        r#"[ warn ]  Starting the GPU (the USB has no firmware for this PC's NVIDIA GeForce RTX 4060 Laptop GPU. On another computer, plug in this USB and run `stela repair`)
--
GPU SELECT 0000:00:02.0 8086:a7a0 rev 04 Intel Iris Xe Graphics: not used: no driver here runs Stela's Metal device on this Intel GPU
GPU SELECT 0000:01:00.0 10de:28e0 rev a1 NVIDIA GeForce RTX 4060 Laptop GPU: discrete, draws; not started: its claim was refused: no pinned firmware for the observed GPU chipset was loaded
GPU SELECT started none
GPU FALLBACK 0000:01:00.0 NVIDIA GeForce RTX 4060 Laptop GPU: its claim was refused: no pinned firmware for the observed GPU chipset was loaded; no other GPU of this PC can draw in its place"#,
    );
    assert_boots(
        pc,
        Build::Full,
        r#"[  ok  ]  Starting the GPU (Intel Iris Xe Graphics, as the NVIDIA GeForce RTX 4060 Laptop GPU did not start)
[ warn ]  the USB has no firmware for this PC's NVIDIA GeForce RTX 4060 Laptop GPU. On another computer, plug in this USB and run `stela repair`
--
GPU SELECT 0000:00:02.0 8086:a7a0 rev 04 Intel Iris Xe Graphics: integrated, draws; started first
GPU SELECT 0000:01:00.0 10de:28e0 rev a1 NVIDIA GeForce RTX 4060 Laptop GPU: discrete, draws; not started: its claim was refused: no pinned firmware for the observed GPU chipset was loaded
GPU SELECT started in this order: Intel Iris Xe Graphics at 0000:00:02.0. A discrete GPU that draws gives the Metal device before an integrated one, and the next one does where it fails; the display is the first of them with a connected display, else the firmware's screen
GPU FALLBACK 0000:01:00.0 NVIDIA GeForce RTX 4060 Laptop GPU: its claim was refused: no pinned firmware for the observed GPU chipset was loaded; the Intel Iris Xe Graphics at 0000:00:02.0, started beside it, draws in its place"#,
    );
}

/// A release drives NVIDIA GPUs alone: on any PC its screen names neither
/// the AMD and Intel ports nor what they drive, and a PC with only AMD or
/// Intel GPUs gets the words of a PC without an NVIDIA GPU.
#[test]
fn a_release_never_mentions_the_amd_and_intel_ports() {
    let others: &[&[Gpu]] = &[
        &[RX_7900_XTX.screen()],
        &[ARC_A770.screen()],
        &[RADEON_780M.screen()],
        &[IRIS_XE.screen()],
        &[RX_7600M_XT, RADEON_780M.screen()],
        &[UHD_770.screen(), ARC_A770],
        &[RX_580.screen()],
    ];
    for pc in others {
        let (screen, _) = boot(pc, Build::Release);
        assert_eq!(screen.len(), 1, "{screen:?}");
        assert!(
            screen[0]
                .starts_with("[ warn ]  Starting the GPU (this PC has no NVIDIA GPU, only the "),
            "{screen:?}"
        );
        assert!(
            screen[0].ends_with(
                ": Stela needs an NVIDIA GeForce GTX 16 or RTX 20, 30, 40 or 50 series GPU)"
            ),
            "{screen:?}"
        );
    }
    let mixed: &[&[Gpu]] = &[
        &[IRIS_XE_RPL.screen(), RTX_4060_LAPTOP],
        &[RTX_4060_LAPTOP, RADEON_780M.screen()],
        &[UHD_770.screen(), RTX_5080],
        &[BMC.screen()],
    ];
    for pc in others.iter().chain(mixed) {
        let (screen, _) = boot(pc, Build::Release);
        let screen = screen.join(
            "
",
        );
        for word in [
            "ported",
            "AMDGPU",
            "amdgpu",
            "i915",
            " xe ",
            "RADV",
            "ANV",
            "Vega or later",
            "Core processors",
            "drives",
        ] {
            assert!(!screen.contains(word), "{word}: {screen}");
        }
    }
}

/// The screen from the GPU step on and the log's `GPU FALLBACK` lines of
/// `pc` booted by `build`.
fn fallback_text(pc: &[Gpu], build: Build) -> String {
    let (screen, log) = boot(pc, build);
    let fallback: Vec<String> = log
        .into_iter()
        .filter(|line| line.starts_with("GPU FALLBACK "))
        .collect();
    let mut text = format!("{}\n--", screen.join("\n"));
    for line in fallback {
        text.push('\n');
        text.push_str(&line);
    }
    text
}

/// The stages a GPU chosen to draw fails at, each applied to the GPU at
/// `index` of `pc`.
fn failing(pc: &[Gpu], index: usize, stage: &str) -> Vec<Gpu> {
    let mut pc = pc.to_vec();
    pc[index] = match stage {
        "claim" => pc[index].refused(Reason::Other, "the GPU's register BAR does not decode"),
        "probe" => pc[index].unbound(),
        _ => pc[index].no_metal("Metal could not open: DeviceLost"),
    };
    pc
}

/// Each stage of a PC: what the boot screen says and what the log's
/// `GPU FALLBACK` lines say, in a release and in the full build.
fn assert_falls_back(pc: &[Gpu], failed: usize, cells: &[(&str, &str, &str)]) {
    for &(stage, release, full) in cells {
        let pc = failing(pc, failed, stage);
        assert_eq!(
            fallback_text(&pc, Build::Release),
            release,
            "{stage} Release"
        );
        assert_eq!(fallback_text(&pc, Build::Full), full, "{stage} Full");
    }
}

/// A desktop's card with the monitor on it, beside the processor's GPU. The discrete GPU fails at each stage in turn.
#[test]
fn a_desktop_card_with_the_monitor_on_it_and_an_integrated_gpu_falls_back_at_each_stage() {
    let pc: &[Gpu] = &[UHD_770, RTX_5080.screen()];
    assert_falls_back(
        pc,
        1,
        &[
            (
                "claim",
                r#"[ warn ]  Starting the GPU (the NVIDIA GeForce RTX 5080 at 0000:01:00.0 did not start: the GPU's register BAR does not decode. The boot log on this USB has the rest)
--
GPU FALLBACK 0000:01:00.0 NVIDIA GeForce RTX 5080: its claim was refused: the GPU's register BAR does not decode; no other GPU of this PC can draw in its place"#,
                r#"[  ok  ]  Starting the GPU (Intel UHD Graphics 770, as the NVIDIA GeForce RTX 5080 did not start)
[ warn ]  the NVIDIA GeForce RTX 5080 at 0000:01:00.0 did not start: the GPU's register BAR does not decode. The boot log on this USB has the rest
          No screen is connected to the Intel UHD Graphics 770, so Stela copies each frame of macOS to the screen the PC started on. The picture is slower than on a screen connected to the GPU.
--
GPU FALLBACK 0000:01:00.0 NVIDIA GeForce RTX 5080: its claim was refused: the GPU's register BAR does not decode; the Intel UHD Graphics 770 at 0000:00:02.0 is claimed and started in its place"#,
            ),
            (
                "probe",
                r#"[ warn ]  the NVIDIA GeForce RTX 5080 at 0000:01:00.0 did not start: its driver nouveau stopped with an error. The screen the PC started on is wired to it and stays dark: the GPU was reset for its driver. The boot log on this USB says why
[ warn ]  Starting the GPU (no GPU started; the lines above say why)
--
GPU FALLBACK 0000:01:00.0 NVIDIA GeForce RTX 5080: its driver's probe failed: nouveau did not bind it; Linux's first error: nouveau 0000:01:00.0: probe failed with error -5; no other GPU of this PC can draw in its place"#,
                r#"[ warn ]  the NVIDIA GeForce RTX 5080 at 0000:01:00.0 did not start: its driver nouveau stopped in its probe: nouveau 0000:01:00.0: probe failed with error -5. The screen the PC started on is wired to it and stays dark: the GPU was reset for its driver. The boot log on this USB says more
[  ok  ]  Starting the GPU (Intel UHD Graphics 770, as the NVIDIA GeForce RTX 5080 did not start)
[ warn ]  No screen shows macOS yet: the screen the PC started on is wired to the NVIDIA GeForce RTX 5080, which did not start. Connect a screen to the Intel UHD Graphics 770, and macOS shows on it
--
GPU FALLBACK 0000:01:00.0 NVIDIA GeForce RTX 5080: its driver's probe failed: nouveau did not bind it; Linux's first error: nouveau 0000:01:00.0: probe failed with error -5; the Intel UHD Graphics 770 at 0000:00:02.0 is claimed and started in its place"#,
            ),
            (
                "metal",
                r#"[  ok  ]  Starting the GPU (NVIDIA GeForce RTX 5080)
--
GPU FALLBACK 0000:01:00.0 NVIDIA GeForce RTX 5080: nouveau bound it, and its Metal device did not open: Metal could not open: DeviceLost; it still shows the screens wired to it; no other GPU of this PC can draw in its place"#,
                r#"[  ok  ]  Starting the GPU (NVIDIA GeForce RTX 5080)
[ warn ]  the NVIDIA GeForce RTX 5080 could not draw for macOS: its Metal device did not open (Metal could not open: DeviceLost), so the Intel UHD Graphics 770 draws instead. The boot log on this USB says more
--
GPU FALLBACK 0000:01:00.0 NVIDIA GeForce RTX 5080: nouveau bound it, and its Metal device did not open: Metal could not open: DeviceLost; it still shows the screens wired to it; the Intel UHD Graphics 770 at 0000:00:02.0 is claimed and started in its place"#,
            ),
        ],
    );
}

/// A desktop's card beside the processor's GPU, the monitor on the motherboard. The discrete GPU fails at each stage in turn.
#[test]
fn a_desktop_card_and_an_integrated_gpu_with_the_monitor_on_it_falls_back_at_each_stage() {
    let pc: &[Gpu] = &[UHD_770.screen(), RTX_5080];
    assert_falls_back(
        pc,
        1,
        &[
            (
                "claim",
                r#"[ warn ]  Starting the GPU (the NVIDIA GeForce RTX 5080 at 0000:01:00.0 did not start: the GPU's register BAR does not decode. The boot log on this USB has the rest)
--
GPU FALLBACK 0000:01:00.0 NVIDIA GeForce RTX 5080: its claim was refused: the GPU's register BAR does not decode; no other GPU of this PC can draw in its place"#,
                r#"[  ok  ]  Starting the GPU (Intel UHD Graphics 770, as the NVIDIA GeForce RTX 5080 did not start)
[ warn ]  the NVIDIA GeForce RTX 5080 at 0000:01:00.0 did not start: the GPU's register BAR does not decode. The boot log on this USB has the rest
--
GPU FALLBACK 0000:01:00.0 NVIDIA GeForce RTX 5080: its claim was refused: the GPU's register BAR does not decode; the Intel UHD Graphics 770 at 0000:00:02.0, started beside it, draws in its place"#,
            ),
            (
                "probe",
                r#"[ warn ]  the NVIDIA GeForce RTX 5080 at 0000:01:00.0 did not start: its driver nouveau stopped with an error. The boot log on this USB says why
[ warn ]  Starting the GPU (no GPU started; the lines above say why)
--
GPU FALLBACK 0000:01:00.0 NVIDIA GeForce RTX 5080: its driver's probe failed: nouveau did not bind it; Linux's first error: nouveau 0000:01:00.0: probe failed with error -5; no other GPU of this PC can draw in its place"#,
                r#"[ warn ]  the NVIDIA GeForce RTX 5080 at 0000:01:00.0 did not start: its driver nouveau stopped in its probe: nouveau 0000:01:00.0: probe failed with error -5. The boot log on this USB says more
[  ok  ]  Starting the GPU (Intel UHD Graphics 770, as the NVIDIA GeForce RTX 5080 did not start)
--
GPU FALLBACK 0000:01:00.0 NVIDIA GeForce RTX 5080: its driver's probe failed: nouveau did not bind it; Linux's first error: nouveau 0000:01:00.0: probe failed with error -5; the Intel UHD Graphics 770 at 0000:00:02.0, started beside it, draws in its place"#,
            ),
            (
                "metal",
                r#"[  ok  ]  Starting the GPU (NVIDIA GeForce RTX 5080)
--
GPU FALLBACK 0000:01:00.0 NVIDIA GeForce RTX 5080: nouveau bound it, and its Metal device did not open: Metal could not open: DeviceLost; it still shows the screens wired to it; no other GPU of this PC can draw in its place"#,
                r#"[  ok  ]  Starting the GPU (NVIDIA GeForce RTX 5080)
          the Intel UHD Graphics 770 shows the picture on the screens wired to it
[ warn ]  the NVIDIA GeForce RTX 5080 could not draw for macOS: its Metal device did not open (Metal could not open: DeviceLost), so the Intel UHD Graphics 770 draws instead. The boot log on this USB says more
--
GPU FALLBACK 0000:01:00.0 NVIDIA GeForce RTX 5080: nouveau bound it, and its Metal device did not open: Metal could not open: DeviceLost; it still shows the screens wired to it; the Intel UHD Graphics 770 at 0000:00:02.0, started beside it, draws in its place"#,
            ),
        ],
    );
}

/// A laptop whose panel hangs on the processor's GPU. The discrete GPU fails at each stage in turn.
#[test]
fn a_laptop_with_the_panel_on_the_integrated_gpu_falls_back_at_each_stage() {
    let pc: &[Gpu] = &[IRIS_XE_RPL.screen(), RTX_4060_LAPTOP];
    assert_falls_back(
        pc,
        1,
        &[
            (
                "claim",
                r#"[ warn ]  Starting the GPU (the NVIDIA GeForce RTX 4060 Laptop GPU at 0000:01:00.0 did not start: the GPU's register BAR does not decode. The boot log on this USB has the rest)
--
GPU FALLBACK 0000:01:00.0 NVIDIA GeForce RTX 4060 Laptop GPU: its claim was refused: the GPU's register BAR does not decode; no other GPU of this PC can draw in its place"#,
                r#"[  ok  ]  Starting the GPU (Intel Iris Xe Graphics, as the NVIDIA GeForce RTX 4060 Laptop GPU did not start)
[ warn ]  the NVIDIA GeForce RTX 4060 Laptop GPU at 0000:01:00.0 did not start: the GPU's register BAR does not decode. The boot log on this USB has the rest
--
GPU FALLBACK 0000:01:00.0 NVIDIA GeForce RTX 4060 Laptop GPU: its claim was refused: the GPU's register BAR does not decode; the Intel Iris Xe Graphics at 0000:00:02.0, started beside it, draws in its place"#,
            ),
            (
                "probe",
                r#"[ warn ]  the NVIDIA GeForce RTX 4060 Laptop GPU at 0000:01:00.0 did not start: its driver nouveau stopped with an error. The boot log on this USB says why
[ warn ]  Starting the GPU (no GPU started; the lines above say why)
--
GPU FALLBACK 0000:01:00.0 NVIDIA GeForce RTX 4060 Laptop GPU: its driver's probe failed: nouveau did not bind it; Linux's first error: nouveau 0000:01:00.0: probe failed with error -5; no other GPU of this PC can draw in its place"#,
                r#"[ warn ]  the NVIDIA GeForce RTX 4060 Laptop GPU at 0000:01:00.0 did not start: its driver nouveau stopped in its probe: nouveau 0000:01:00.0: probe failed with error -5. The boot log on this USB says more
[  ok  ]  Starting the GPU (Intel Iris Xe Graphics, as the NVIDIA GeForce RTX 4060 Laptop GPU did not start)
--
GPU FALLBACK 0000:01:00.0 NVIDIA GeForce RTX 4060 Laptop GPU: its driver's probe failed: nouveau did not bind it; Linux's first error: nouveau 0000:01:00.0: probe failed with error -5; the Intel Iris Xe Graphics at 0000:00:02.0, started beside it, draws in its place"#,
            ),
            (
                "metal",
                r#"[  ok  ]  Starting the GPU (NVIDIA GeForce RTX 4060 Laptop GPU)
--
GPU FALLBACK 0000:01:00.0 NVIDIA GeForce RTX 4060 Laptop GPU: nouveau bound it, and its Metal device did not open: Metal could not open: DeviceLost; it still shows the screens wired to it; no other GPU of this PC can draw in its place"#,
                r#"[  ok  ]  Starting the GPU (NVIDIA GeForce RTX 4060 Laptop GPU)
          the Intel Iris Xe Graphics shows the picture on the screens wired to it
[ warn ]  the NVIDIA GeForce RTX 4060 Laptop GPU could not draw for macOS: its Metal device did not open (Metal could not open: DeviceLost), so the Intel Iris Xe Graphics draws instead. The boot log on this USB says more
--
GPU FALLBACK 0000:01:00.0 NVIDIA GeForce RTX 4060 Laptop GPU: nouveau bound it, and its Metal device did not open: Metal could not open: DeviceLost; it still shows the screens wired to it; the Intel Iris Xe Graphics at 0000:00:02.0, started beside it, draws in its place"#,
            ),
        ],
    );
}

/// A laptop whose display mux puts the panel on the discrete GPU. The discrete GPU fails at each stage in turn.
#[test]
fn a_laptop_with_the_panel_on_the_discrete_gpu_falls_back_at_each_stage() {
    let pc: &[Gpu] = &[IRIS_XE_RPL, RTX_4060_LAPTOP.screen()];
    assert_falls_back(
        pc,
        1,
        &[
            (
                "claim",
                r#"[ warn ]  Starting the GPU (the NVIDIA GeForce RTX 4060 Laptop GPU at 0000:01:00.0 did not start: the GPU's register BAR does not decode. The boot log on this USB has the rest)
--
GPU FALLBACK 0000:01:00.0 NVIDIA GeForce RTX 4060 Laptop GPU: its claim was refused: the GPU's register BAR does not decode; no other GPU of this PC can draw in its place"#,
                r#"[  ok  ]  Starting the GPU (Intel Iris Xe Graphics, as the NVIDIA GeForce RTX 4060 Laptop GPU did not start)
[ warn ]  the NVIDIA GeForce RTX 4060 Laptop GPU at 0000:01:00.0 did not start: the GPU's register BAR does not decode. The boot log on this USB has the rest
          No screen is connected to the Intel Iris Xe Graphics, so Stela copies each frame of macOS to the screen the PC started on. The picture is slower than on a screen connected to the GPU.
--
GPU FALLBACK 0000:01:00.0 NVIDIA GeForce RTX 4060 Laptop GPU: its claim was refused: the GPU's register BAR does not decode; the Intel Iris Xe Graphics at 0000:00:02.0 is claimed and started in its place"#,
            ),
            (
                "probe",
                r#"[ warn ]  the NVIDIA GeForce RTX 4060 Laptop GPU at 0000:01:00.0 did not start: its driver nouveau stopped with an error. The screen the PC started on is wired to it and stays dark: the GPU was reset for its driver. The boot log on this USB says why
[ warn ]  Starting the GPU (no GPU started; the lines above say why)
--
GPU FALLBACK 0000:01:00.0 NVIDIA GeForce RTX 4060 Laptop GPU: its driver's probe failed: nouveau did not bind it; Linux's first error: nouveau 0000:01:00.0: probe failed with error -5; no other GPU of this PC can draw in its place"#,
                r#"[ warn ]  the NVIDIA GeForce RTX 4060 Laptop GPU at 0000:01:00.0 did not start: its driver nouveau stopped in its probe: nouveau 0000:01:00.0: probe failed with error -5. The screen the PC started on is wired to it and stays dark: the GPU was reset for its driver. The boot log on this USB says more
[  ok  ]  Starting the GPU (Intel Iris Xe Graphics, as the NVIDIA GeForce RTX 4060 Laptop GPU did not start)
[ warn ]  No screen shows macOS yet: the screen the PC started on is wired to the NVIDIA GeForce RTX 4060 Laptop GPU, which did not start. Connect a screen to the Intel Iris Xe Graphics, and macOS shows on it
--
GPU FALLBACK 0000:01:00.0 NVIDIA GeForce RTX 4060 Laptop GPU: its driver's probe failed: nouveau did not bind it; Linux's first error: nouveau 0000:01:00.0: probe failed with error -5; the Intel Iris Xe Graphics at 0000:00:02.0 is claimed and started in its place"#,
            ),
            (
                "metal",
                r#"[  ok  ]  Starting the GPU (NVIDIA GeForce RTX 4060 Laptop GPU)
--
GPU FALLBACK 0000:01:00.0 NVIDIA GeForce RTX 4060 Laptop GPU: nouveau bound it, and its Metal device did not open: Metal could not open: DeviceLost; it still shows the screens wired to it; no other GPU of this PC can draw in its place"#,
                r#"[  ok  ]  Starting the GPU (NVIDIA GeForce RTX 4060 Laptop GPU)
[ warn ]  the NVIDIA GeForce RTX 4060 Laptop GPU could not draw for macOS: its Metal device did not open (Metal could not open: DeviceLost), so the Intel Iris Xe Graphics draws instead. The boot log on this USB says more
--
GPU FALLBACK 0000:01:00.0 NVIDIA GeForce RTX 4060 Laptop GPU: nouveau bound it, and its Metal device did not open: Metal could not open: DeviceLost; it still shows the screens wired to it; the Intel Iris Xe Graphics at 0000:00:02.0 is claimed and started in its place"#,
            ),
        ],
    );
}

/// A desktop with two cards, the monitor on the first. The discrete GPU fails at each stage in turn.
#[test]
fn a_desktop_with_two_cards_falls_back_at_each_stage() {
    let pc: &[Gpu] = &[RTX_5080.screen(), RX_7900_XTX];
    assert_falls_back(
        pc,
        0,
        &[
            (
                "claim",
                r#"[ warn ]  Starting the GPU (the NVIDIA GeForce RTX 5080 at 0000:01:00.0 did not start: the GPU's register BAR does not decode. The boot log on this USB has the rest)
--
GPU FALLBACK 0000:01:00.0 NVIDIA GeForce RTX 5080: its claim was refused: the GPU's register BAR does not decode; no other GPU of this PC can draw in its place"#,
                r#"[  ok  ]  Starting the GPU (AMD Radeon RX 7900 XTX, as the NVIDIA GeForce RTX 5080 did not start)
[ warn ]  the NVIDIA GeForce RTX 5080 at 0000:01:00.0 did not start: the GPU's register BAR does not decode. The boot log on this USB has the rest
          No screen is connected to the AMD Radeon RX 7900 XTX, so Stela copies each frame of macOS to the screen the PC started on. The picture is slower than on a screen connected to the GPU.
--
GPU FALLBACK 0000:01:00.0 NVIDIA GeForce RTX 5080: its claim was refused: the GPU's register BAR does not decode; the AMD Radeon RX 7900 XTX at 0000:03:00.0, started beside it, draws in its place"#,
            ),
            (
                "probe",
                r#"[ warn ]  the NVIDIA GeForce RTX 5080 at 0000:01:00.0 did not start: its driver nouveau stopped with an error. The screen the PC started on is wired to it and stays dark: the GPU was reset for its driver. The boot log on this USB says why
[ warn ]  Starting the GPU (no GPU started; the lines above say why)
--
GPU FALLBACK 0000:01:00.0 NVIDIA GeForce RTX 5080: its driver's probe failed: nouveau did not bind it; Linux's first error: nouveau 0000:01:00.0: probe failed with error -5; no other GPU of this PC can draw in its place"#,
                r#"[ warn ]  the NVIDIA GeForce RTX 5080 at 0000:01:00.0 did not start: its driver nouveau stopped in its probe: nouveau 0000:01:00.0: probe failed with error -5. The screen the PC started on is wired to it and stays dark: the GPU was reset for its driver. The boot log on this USB says more
[  ok  ]  Starting the GPU (AMD Radeon RX 7900 XTX, as the NVIDIA GeForce RTX 5080 did not start)
[ warn ]  No screen shows macOS yet: the screen the PC started on is wired to the NVIDIA GeForce RTX 5080, which did not start. Connect a screen to the AMD Radeon RX 7900 XTX, and macOS shows on it
--
GPU FALLBACK 0000:01:00.0 NVIDIA GeForce RTX 5080: its driver's probe failed: nouveau did not bind it; Linux's first error: nouveau 0000:01:00.0: probe failed with error -5; the AMD Radeon RX 7900 XTX at 0000:03:00.0, started beside it, draws in its place"#,
            ),
            (
                "metal",
                r#"[  ok  ]  Starting the GPU (NVIDIA GeForce RTX 5080)
--
GPU FALLBACK 0000:01:00.0 NVIDIA GeForce RTX 5080: nouveau bound it, and its Metal device did not open: Metal could not open: DeviceLost; it still shows the screens wired to it; no other GPU of this PC can draw in its place"#,
                r#"[  ok  ]  Starting the GPU (NVIDIA GeForce RTX 5080)
[ warn ]  the NVIDIA GeForce RTX 5080 could not draw for macOS: its Metal device did not open (Metal could not open: DeviceLost), so the AMD Radeon RX 7900 XTX draws instead. The boot log on this USB says more
--
GPU FALLBACK 0000:01:00.0 NVIDIA GeForce RTX 5080: nouveau bound it, and its Metal device did not open: Metal could not open: DeviceLost; it still shows the screens wired to it; the AMD Radeon RX 7900 XTX at 0000:03:00.0, started beside it, draws in its place"#,
            ),
        ],
    );
}

/// A release starts no GPU of another maker in place of an NVIDIA GPU that
/// fails at any stage: its screen is the one it showed before fallbacks,
/// and its log says no other GPU can draw.
#[test]
fn a_release_falls_back_to_no_other_maker() {
    let pcs: &[(&[Gpu], usize)] = &[
        (&[UHD_770, RTX_5080.screen()], 1),
        (&[UHD_770.screen(), RTX_5080], 1),
        (&[IRIS_XE_RPL.screen(), RTX_4060_LAPTOP], 1),
        (&[IRIS_XE_RPL, RTX_4060_LAPTOP.screen()], 1),
        (&[RTX_5080.screen(), RX_7900_XTX], 0),
        (&[RTX_4060_LAPTOP, RADEON_780M.screen()], 0),
        (&[RTX_5080.screen()], 0),
    ];
    for &(pc, failed) in pcs {
        for stage in ["claim", "probe", "metal"] {
            let (screen, log) = boot(&failing(pc, failed, stage), Build::Release);
            let screen = screen.join("\n");
            for words in [
                "in its place",
                "draws instead",
                "No screen shows macOS yet",
                ", as the ",
            ] {
                assert!(!screen.contains(words), "{stage}: {screen}");
            }
            let fallback: Vec<&String> = log
                .iter()
                .filter(|line| line.starts_with("GPU FALLBACK "))
                .collect();
            assert_eq!(fallback.len(), 1, "{stage}: {log:?}");
            assert!(
                fallback[0].ends_with("; no other GPU of this PC can draw in its place"),
                "{stage}: {log:?}"
            );
        }
    }
}

/// Two GPUs that fail in turn: the second, claimed in place of the first,
/// gives way to none, and the boot says so for each.
#[test]
fn a_gpu_claimed_in_place_of_another_fails_in_turn() {
    let pc: &[Gpu] = &[UHD_770.unbound(), RTX_5080.screen().unbound()];
    assert_eq!(
        fallback_text(pc, Build::Full),
        r#"[ warn ]  the NVIDIA GeForce RTX 5080 at 0000:01:00.0 did not start: its driver nouveau stopped in its probe: nouveau 0000:01:00.0: probe failed with error -5. The screen the PC started on is wired to it and stays dark: the GPU was reset for its driver. The boot log on this USB says more
[ warn ]  the Intel UHD Graphics 770 at 0000:00:02.0 did not start: its driver i915 stopped with an error. The boot log on this USB says why
[ warn ]  Starting the GPU (no GPU started; the lines above say why)
--
GPU FALLBACK 0000:01:00.0 NVIDIA GeForce RTX 5080: its driver's probe failed: nouveau did not bind it; Linux's first error: nouveau 0000:01:00.0: probe failed with error -5; the Intel UHD Graphics 770 at 0000:00:02.0 is claimed and started in its place
GPU FALLBACK 0000:00:02.0 Intel UHD Graphics 770: its driver's probe failed: i915 did not bind it; Linux's first error: i915 0000:00:02.0: probe failed with error -5; no other GPU of this PC can draw in its place"#
    );
    let pc: &[Gpu] = &[RTX_5080.screen().unbound(), RX_7900_XTX.unbound(), UHD_770];
    assert_eq!(
        fallback_text(pc, Build::Full),
        r#"[ warn ]  the NVIDIA GeForce RTX 5080 at 0000:01:00.0 did not start: its driver nouveau stopped in its probe: nouveau 0000:01:00.0: probe failed with error -5. The screen the PC started on is wired to it and stays dark: the GPU was reset for its driver. The boot log on this USB says more
[ warn ]  the AMD Radeon RX 7900 XTX at 0000:03:00.0 did not start: its driver amdgpu stopped in its probe: amdgpu 0000:03:00.0: probe failed with error -5. The boot log on this USB says more
[  ok  ]  Starting the GPU (Intel UHD Graphics 770, as the NVIDIA GeForce RTX 5080 and the AMD Radeon RX 7900 XTX did not start)
[ warn ]  No screen shows macOS yet: the screen the PC started on is wired to the NVIDIA GeForce RTX 5080, which did not start. Connect a screen to the Intel UHD Graphics 770, and macOS shows on it
--
GPU FALLBACK 0000:01:00.0 NVIDIA GeForce RTX 5080: its driver's probe failed: nouveau did not bind it; Linux's first error: nouveau 0000:01:00.0: probe failed with error -5; the Intel UHD Graphics 770 at 0000:00:02.0 is claimed and started in its place
GPU FALLBACK 0000:03:00.0 AMD Radeon RX 7900 XTX: its driver's probe failed: amdgpu did not bind it; Linux's first error: amdgpu 0000:03:00.0: probe failed with error -5; the Intel UHD Graphics 770 at 0000:00:02.0 is claimed and started in its place"#
    );
}
