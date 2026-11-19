//! Which of a PC's display controllers Stela starts, in which order, and why
//! the others are left alone, from what each reports: whether a stack of
//! this image drives it, whether that stack draws (its Vulkan driver is in
//! the image), whether its driver's table calls it integrated, discrete or
//! leaves that to its probe, and whether the firmware's picture may be on
//! it. Nothing here knows a PC, a GPU model or a maker beyond that.
//!
//! The rule, a working picture first and the faster GPU second:
//!
//! - Every GPU whose stack draws may give the Metal device; a discrete GPU
//!   draws before an integrated one ([`crate::stack::metal_devices`]), so on
//!   a laptop the discrete GPU draws whether or not the panel is wired to it.
//! - Any display shows any drawing GPU's frames: the DCP model copies each
//!   frame from guest memory into the buffer of whatever shows it, a bound
//!   GPU's output or the firmware's framebuffer, at the same cost. So the
//!   GPU that draws need not drive the display.
//! - A GPU that would only show the frames another draws (an integrated GPU
//!   beside a drawing discrete one, or one whose stack does not draw) is
//!   started only where the firmware's picture may be on it: then its own
//!   driver lights that display (a laptop's panel on its integrated GPU).
//!   Elsewhere it is left with the firmware, untouched: no display needs it,
//!   and its start would cost time and could stop the boot.
//! - A GPU whose kind only its probe tells may be a second discrete GPU and
//!   is started, after the GPUs that surely draw, so its probe never delays
//!   or ends theirs.
//! - A GPU no stack drives is left with the firmware; its picture, if the
//!   firmware's is on it, keeps showing macOS by copy.
//! - When a GPU's claim is refused, the next one takes its place: a GPU left
//!   out only beside it is started instead. So does a GPU that started and
//!   cannot draw after all, its driver's probe failed or its Metal device
//!   did not open ([`fall_back`]): every GPU a stack drives gets its turn,
//!   in this order, before the boot goes on without one.
use crate::bus::pci::Address;
use crate::stack::{drawing_kind, may_only_show_frames_of, only_shows_frames_of};
use alloc::{format, string::String, vec::Vec};

/// A display controller of the census, as the selection sees it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Found {
    /// What its stack says of it, or why no stack of this image drives it.
    pub stack: Result<Driven, &'static str>,
    /// The firmware's picture may be on it.
    pub firmware_screen: bool,
}

/// What a display controller's stack says of it before its start.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Driven {
    /// The stack draws: the image holds its Vulkan driver.
    pub draws: bool,
    /// Its driver's table calls it integrated, discrete, or leaves that to
    /// its probe ([`crate::stack::Kernel::table_integrated`]).
    pub integrated: Option<bool>,
}

/// What became of one display controller.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Choice {
    /// No stack of this image drives it.
    Unused(&'static str),
    /// Started in the first turn: it may give the Metal device.
    First,
    /// Started after the first turn: it may only show the frames the GPU
    /// `beside` (an index of the census) draws.
    Later { beside: usize },
    /// Left with the firmware: it would only show the frames `beside` draws,
    /// and the firmware's picture is not on it.
    Left { beside: usize },
    /// Its claim was refused; the claim says why.
    Refused,
}

/// The selection: each controller's choice, by its index in the census,
/// and the order the started ones are claimed in.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Selection {
    pub choices: Vec<Choice>,
    /// The claimed controllers, in the order they were claimed: drawing
    /// discrete ones first, then those whose kind their probe tells, then
    /// drawing integrated ones, then those that only show, each group in the
    /// firmware's order. Their DRM minors follow it.
    pub claimed: Vec<usize>,
}

/// Selects among `found`, in the firmware's order. `claim` claims the
/// controller at an index, in the claim order, and answers whether it was
/// claimed.
pub fn select(found: &[Found], mut claim: impl FnMut(usize) -> bool) -> Selection {
    let mut choices: Vec<Choice> = found
        .iter()
        .map(|found| match found.stack {
            Ok(_) => Choice::Refused,
            Err(reason) => Choice::Unused(reason),
        })
        .collect();
    let mut order: Vec<(usize, Driven)> = found
        .iter()
        .enumerate()
        .filter_map(|(index, found)| found.stack.ok().map(|driven| (index, driven)))
        .collect();
    order.sort_by_key(|(_, driven)| crate::stack::claim_rank(driven.draws, driven.integrated));
    // The claimed drawing GPUs, each as it counts against the next.
    let mut drawing: Vec<(usize, bool)> = Vec::new();
    let mut claimed = Vec::new();
    for &(index, driven) in &order {
        let kinds: Vec<bool> = drawing.iter().map(|&(_, kind)| kind).collect();
        if let Some(beside) = only_shows_frames_of(&kinds, driven.draws, driven.integrated)
            && !found[index].firmware_screen
        {
            choices[index] = Choice::Left {
                beside: drawing[beside].0,
            };
            continue;
        }
        if !claim(index) {
            continue;
        }
        if driven.draws {
            drawing.push((index, drawing_kind(driven.integrated)));
        }
        claimed.push(index);
    }
    // The probe turns, in the claim order: a GPU that may only show another's
    // frames is probed after the GPUs that may give the Metal device.
    let mut first: Vec<(usize, bool)> = Vec::new();
    for &index in &claimed {
        let Ok(driven) = found[index].stack else {
            continue;
        };
        let kinds: Vec<bool> = first.iter().map(|&(_, kind)| kind).collect();
        choices[index] = match may_only_show_frames_of(&kinds, driven.draws, driven.integrated) {
            Some(beside) => Choice::Later {
                beside: first[beside].0,
            },
            None => {
                if driven.draws {
                    first.push((index, drawing_kind(driven.integrated)));
                }
                Choice::First
            }
        };
    }
    Selection { choices, claimed }
}

/// The selection again once the GPUs of `failed`, by census index, could
/// not draw after all: their drivers did not bind them, or bound them and
/// opened no Metal device. Each counts as a GPU whose claim was refused, so
/// the rule picks again with them out: a GPU claimed before stays claimed,
/// one whose claim was refused before is not asked again, and a GPU left with
/// the firmware only beside a failed one is claimed now (`claim`, which
/// answers whether it was), as [`select`] would have claimed it in the failed
/// one's place. The GPUs newly claimed are [`newly_claimed`]'s.
pub fn fall_back(
    found: &[Found],
    before: &Selection,
    failed: &[usize],
    mut claim: impl FnMut(usize) -> bool,
) -> Selection {
    select(found, |index| {
        if failed.contains(&index) || before.choices.get(index) == Some(&Choice::Refused) {
            false
        } else if before.claimed.contains(&index) {
            true
        } else {
            claim(index)
        }
    })
}

/// The GPUs `after` claimed that `before` had not, in the claim order.
pub fn newly_claimed(before: &Selection, after: &Selection) -> Vec<usize> {
    after
        .claimed
        .iter()
        .copied()
        .filter(|index| !before.claimed.contains(index))
        .collect()
}

/// How the GPUs that start use this PC's RAM, for the memory plan, which
/// is made before any GPU is claimed ([`memory`]).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Memory {
    /// No integrated GPU starts: Linux's pages, a discrete GPU's
    /// system-memory buffers among them, come from the heap.
    Own,
    /// An integrated GPU gives the Metal device: it takes its memory from
    /// a share of this PC's RAM, which bounds Linux's pages.
    Shared,
    /// An integrated GPU starts only to show the display, beside a discrete
    /// GPU that draws (or with no drawing GPU at all): its driver and
    /// scanout buffers need some pages, the discrete GPU's buffers many,
    /// and neither may be bounded by a share sized for the other.
    Display,
}

/// The GPUs of `found` that start, by index, as [`select`] starts them when
/// every claim succeeds: the loader's question, asked before any claim.
pub fn started(found: &[Found]) -> Vec<usize> {
    select(found, |_| true).claimed
}

/// The memory the GPUs of `found` use, as [`select`] starts them when every
/// claim succeeds: the loader's question, asked before any claim. A kind
/// left to a GPU's probe counts as integrated, as the loader takes a GPU it
/// could not tell. Should the discrete GPU fail to start, the integrated one
/// draws after all on a plan of [`Memory::Display`] or [`Memory::Own`]; the
/// platform then budgets its Metal device by what the heap holds past its
/// own needs.
pub fn memory(found: &[Found]) -> Memory {
    let started: Vec<Driven> = started(found)
        .into_iter()
        .filter_map(|index| found[index].stack.ok())
        .collect();
    let integrated = |driven: &Driven| driven.integrated.unwrap_or(true);
    let discrete_draws = started.iter().any(|gpu| gpu.draws && !integrated(gpu));
    if !discrete_draws && started.iter().any(|gpu| gpu.draws && integrated(gpu)) {
        Memory::Shared
    } else if started.iter().any(integrated) {
        Memory::Display
    } else {
        Memory::Own
    }
}

/// Whether an integrated GPU this image draws with may draw in place of the
/// discrete GPU the plan of `found` ([`memory`]) gives the Metal device,
/// should that one not start: the plan shares no RAM with a GPU, and a GPU
/// of `found` whose stack draws is integrated, or of a kind only its probe
/// tells. The memory plan then keeps room for its pages. A GPU no stack of
/// this image drives never counts, so an image without an integrated GPU's
/// driver plans as before.
pub fn integrated_may_draw_instead(found: &[Found]) -> bool {
    memory(found) != Memory::Shared
        && found.iter().any(|found| {
            found
                .stack
                .is_ok_and(|driven| driven.draws && driven.integrated.unwrap_or(true))
        })
}

/// What `stack` says of the display controller `adapter` before its start,
/// the same for the loader's memory plan and the engine's census: whether
/// it draws, and its kind as its driver's table gives it, else `told`, the
/// kind the loader read before the plan (an AMD GPU's IP discovery table, or
/// integrated where nothing told it), which the engine takes from the plan
/// ([`Planned::kind`]). So both select the same GPUs for the same census.
pub fn driven(
    stack: &crate::stack::GpuStack,
    adapter: &crate::Adapter,
    told: Option<bool>,
) -> Driven {
    Driven {
        draws: stack.draws(),
        integrated: stack.kernel.table_integrated(adapter).or(told),
    }
}

/// The display controllers the memory plan saw, at most this many.
const PLANNED: usize = 16;

/// What the loader's memory plan saw of the display controllers, which the
/// engine's census takes so that both select alike: each controller's PCI
/// address and the kind the plan took for it ([`driven`]'s `told`), and the
/// memory planned. A controller the plan did not see, as a GPU below a port
/// powered on after the firmware's exit, is told apart ([`Self::saw`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Planned {
    seen: [(Address, Option<bool>); PLANNED],
    count: usize,
    pub memory: Memory,
}

impl Planned {
    pub const fn new(memory: Memory) -> Self {
        const NONE: Address = Address {
            segment: 0,
            bus: 0,
            device: 0,
            function: 0,
        };
        Self {
            seen: [(NONE, None); PLANNED],
            count: 0,
            memory,
        }
    }

    /// The plan saw the controller at `address` and took `kind` for it.
    /// Past [`PLANNED`] controllers the rest are not kept, and the engine
    /// tells their kinds as before the plan.
    pub fn see(&mut self, address: Address, kind: Option<bool>) {
        if self.count < PLANNED {
            self.seen[self.count] = (address, kind);
            self.count += 1;
        }
    }

    fn find(&self, address: Address) -> Option<Option<bool>> {
        self.seen[..self.count]
            .iter()
            .find(|(seen, _)| *seen == address)
            .map(|&(_, kind)| kind)
    }

    /// The kind the plan took for the controller at `address`.
    pub fn kind(&self, address: Address) -> Option<bool> {
        self.find(address).flatten()
    }

    /// Whether the plan saw the controller at `address`.
    pub fn saw(&self, address: Address) -> bool {
        self.find(address).is_some()
    }
}

/// The started GPUs of `chosen`, by census index, at `addresses`, that the
/// plan did not see: the plan's memory then stands for GPUs it did not
/// count, which the engine says.
pub fn unplanned(planned: &Planned, addresses: &[Address], chosen: &Selection) -> Vec<usize> {
    chosen
        .claimed
        .iter()
        .copied()
        .filter(|&index| !planned.saw(addresses[index]))
        .collect()
}

impl Memory {
    /// The memory in words, for a line.
    pub fn describe(self) -> &'static str {
        match self {
            Self::Own => "no integrated GPU starts",
            Self::Shared => "an integrated GPU draws in a share of RAM",
            Self::Display => "an integrated GPU only shows the display beside a discrete GPU",
        }
    }
}

impl Driven {
    /// Its kind and whether it draws, in words.
    pub fn describe(self) -> String {
        let kind = match self.integrated {
            Some(true) => "integrated",
            Some(false) => "discrete",
            None => "integrated or discrete, as its probe tells",
        };
        let draws = match self.draws {
            true => "draws",
            false => "shows only: this image has no Vulkan driver for it",
        };
        format!("{kind}, {draws}")
    }
}

impl Choice {
    /// The choice in words; `name` names a controller of the census.
    pub fn describe(self, name: &dyn Fn(usize) -> String) -> String {
        match self {
            Self::Unused(reason) => format!("not used: {reason}"),
            Self::First => String::from("started first"),
            Self::Later { beside } => format!(
                "started after {}: it may only show the frames that GPU draws",
                name(beside)
            ),
            Self::Left { beside } => format!(
                "left with the firmware: it would only show the frames {} draws, and the \
                 firmware's picture is not on it",
                name(beside)
            ),
            Self::Refused => String::from("not started: its claim was refused"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    const DISCRETE: Driven = Driven {
        draws: true,
        integrated: Some(false),
    };
    /// An integrated GPU its table names (an Intel GPU, an older AMD APU).
    const INTEGRATED: Driven = Driven {
        draws: true,
        integrated: Some(true),
    };
    /// An AMD GPU amdgpu matches by class: its probe tells its kind.
    const PROBED: Driven = Driven {
        draws: true,
        integrated: None,
    };
    /// A stack without its Vulkan driver in the image (xe today).
    const SHOWS: Driven = Driven {
        draws: false,
        integrated: Some(true),
    };

    fn gpu(driven: Driven, firmware_screen: bool) -> Found {
        Found {
            stack: Ok(driven),
            firmware_screen,
        }
    }

    fn unsupported(firmware_screen: bool) -> Found {
        Found {
            stack: Err("no Stela GPU backend for this PCI vendor"),
            firmware_screen,
        }
    }

    /// The selection when every claim succeeds.
    fn all(found: &[Found]) -> Selection {
        select(found, |_| true)
    }

    #[test]
    fn a_desktop_with_one_card_starts_it() {
        for screen in [true, false] {
            let chosen = all(&[gpu(DISCRETE, screen)]);
            assert_eq!(chosen.choices, [Choice::First]);
            assert_eq!(chosen.claimed, [0]);
        }
    }

    #[test]
    fn a_desktop_with_a_card_and_an_integrated_gpu() {
        // The monitor on the card: the integrated GPU (bus 0, first in the
        // firmware's order) would only show the card's frames.
        let chosen = all(&[gpu(INTEGRATED, false), gpu(DISCRETE, true)]);
        assert_eq!(chosen.choices, [Choice::Left { beside: 1 }, Choice::First]);
        assert_eq!(chosen.claimed, [1]);
        // The monitor on the motherboard: the integrated GPU lights it with
        // the card's frames, probed after the card.
        let chosen = all(&[gpu(INTEGRATED, true), gpu(DISCRETE, false)]);
        assert_eq!(chosen.choices, [Choice::Later { beside: 1 }, Choice::First]);
        assert_eq!(chosen.claimed, [1, 0]);
        // An AMD iGPU of Ryzen 7000 or later, whose probe tells its kind: it
        // may be a second card, so it starts either way, after the card.
        for screen in [true, false] {
            let chosen = all(&[gpu(PROBED, screen), gpu(DISCRETE, !screen)]);
            assert_eq!(chosen.choices, [Choice::Later { beside: 1 }, Choice::First]);
            assert_eq!(chosen.claimed, [1, 0]);
        }
    }

    #[test]
    fn a_muxless_laptop_draws_on_the_discrete_gpu_and_shows_on_the_panels() {
        // The panel on the integrated GPU, the discrete GPU with no outputs
        // or only external ports: the discrete GPU draws, the integrated one
        // shows the panel.
        let laptop = [gpu(INTEGRATED, true), gpu(DISCRETE, false)];
        assert_eq!(
            all(&laptop).choices,
            [Choice::Later { beside: 1 }, Choice::First]
        );
        // With an integrated GPU whose stack only shows, the same.
        let laptop = [gpu(SHOWS, true), gpu(DISCRETE, false)];
        assert_eq!(
            all(&laptop).choices,
            [Choice::Later { beside: 1 }, Choice::First]
        );
        // With an integrated GPU no stack drives, the firmware's picture on
        // it stays and shows the discrete GPU's frames by copy.
        let laptop = [unsupported(true), gpu(DISCRETE, false)];
        let chosen = all(&laptop);
        assert!(matches!(chosen.choices[0], Choice::Unused(_)));
        assert_eq!(chosen.choices[1], Choice::First);
        assert_eq!(chosen.claimed, [1]);
    }

    #[test]
    fn a_laptop_with_a_mux_follows_the_firmwares_setting() {
        // Set to the discrete GPU: the panel is on it, and the integrated
        // GPU, if the firmware left it on, is not needed.
        let chosen = all(&[gpu(INTEGRATED, false), gpu(DISCRETE, true)]);
        assert_eq!(chosen.choices, [Choice::Left { beside: 1 }, Choice::First]);
        // Set to hybrid: as a muxless laptop.
        let chosen = all(&[gpu(INTEGRATED, true), gpu(DISCRETE, false)]);
        assert_eq!(chosen.choices, [Choice::Later { beside: 1 }, Choice::First]);
    }

    #[test]
    fn two_cards_both_start_first_in_the_firmwares_order() {
        let chosen = all(&[gpu(DISCRETE, false), gpu(DISCRETE, true)]);
        assert_eq!(chosen.choices, [Choice::First, Choice::First]);
        assert_eq!(chosen.claimed, [0, 1]);
    }

    #[test]
    fn a_gpu_no_stack_drives_stays_beside_one_that_starts() {
        // ASPEED's BMC display showing the firmware's picture beside a card.
        let chosen = all(&[unsupported(true), gpu(DISCRETE, false)]);
        assert_eq!(
            chosen.choices[0],
            Choice::Unused("no Stela GPU backend for this PCI vendor")
        );
        assert_eq!(chosen.choices[1], Choice::First);
        // A display-only stack alone still starts, to show the picture.
        assert_eq!(all(&[gpu(SHOWS, true)]).choices, [Choice::First]);
        assert_eq!(all(&[gpu(SHOWS, false)]).choices, [Choice::First]);
    }

    #[test]
    fn a_refused_preferred_gpu_lets_the_next_one_start() {
        // The card's claim is refused: the integrated GPU, which would only
        // have shown its frames, starts instead and draws.
        let found = [gpu(INTEGRATED, false), gpu(DISCRETE, true)];
        let mut asked = Vec::new();
        let chosen = select(&found, |index| {
            asked.push(index);
            index != 1
        });
        assert_eq!(asked, [1, 0]);
        assert_eq!(chosen.choices, [Choice::First, Choice::Refused]);
        assert_eq!(chosen.claimed, [0]);
        // Of two cards, the second starts alone.
        let chosen = select(&[gpu(DISCRETE, true), gpu(DISCRETE, false)], |index| {
            index == 1
        });
        assert_eq!(chosen.choices, [Choice::Refused, Choice::First]);
    }

    /// A GPU that was started and could not draw (its driver did not bind
    /// it, or opened no Metal device on it) gives way as a refused one does:
    /// the GPU left with the firmware beside it is claimed, and each GPU
    /// gets its chance in the rule's order until none is left.
    #[test]
    fn a_gpu_that_fails_after_its_claim_gives_way_to_the_next() {
        // A desktop's monitor on its card: the integrated GPU was left.
        let found = [gpu(INTEGRATED, false), gpu(DISCRETE, true)];
        let first = all(&found);
        assert_eq!(first.choices, [Choice::Left { beside: 1 }, Choice::First]);
        let mut asked = Vec::new();
        let after = fall_back(&found, &first, &[1], |index| {
            asked.push(index);
            true
        });
        assert_eq!(asked, [0]);
        assert_eq!(after.choices, [Choice::First, Choice::Refused]);
        assert_eq!(newly_claimed(&first, &after), [0]);
        // Its claim refused in turn: nothing is left to start.
        let refused = fall_back(&found, &first, &[1], |_| false);
        assert!(newly_claimed(&first, &refused).is_empty());
        assert_eq!(refused.choices, [Choice::Refused, Choice::Refused]);
        // Once it failed too, nothing is asked again.
        let none = fall_back(&found, &after, &[0, 1], |_| panic!("asked again"));
        assert!(none.claimed.is_empty());

        // The monitor on the motherboard, or a laptop's panel on its
        // integrated GPU: that one was started already and draws now,
        // without a claim of its own.
        let found = [gpu(INTEGRATED, true), gpu(DISCRETE, false)];
        let first = all(&found);
        let after = fall_back(&found, &first, &[1], |_| panic!("claimed again"));
        assert_eq!(after.choices, [Choice::First, Choice::Refused]);
        assert!(newly_claimed(&first, &after).is_empty());

        // Two cards: the second keeps drawing; an integrated GPU left beside
        // both stays left while one of them draws.
        let found = [
            gpu(INTEGRATED, false),
            gpu(DISCRETE, true),
            gpu(DISCRETE, false),
        ];
        let first = all(&found);
        assert_eq!(first.claimed, [1, 2]);
        let after = fall_back(&found, &first, &[1], |_| panic!("claimed"));
        assert_eq!(
            after.choices,
            [Choice::Left { beside: 2 }, Choice::Refused, Choice::First]
        );
        // Both cards failed: the integrated GPU is the last chance.
        let last = fall_back(&found, &after, &[1, 2], |_| true);
        assert_eq!(newly_claimed(&after, &last), [0]);

        // A card whose claim was refused at the start is not asked again,
        // and an adapter no stack drives never starts.
        let found = [
            unsupported(true),
            gpu(INTEGRATED, false),
            gpu(DISCRETE, false),
            gpu(DISCRETE, true),
        ];
        let first = select(&found, |index| index != 2);
        assert_eq!(
            first.choices[1..],
            [Choice::Left { beside: 3 }, Choice::Refused, Choice::First]
        );
        let after = fall_back(&found, &first, &[3], |index| {
            assert_eq!(index, 1);
            true
        });
        assert_eq!(newly_claimed(&first, &after), [1]);
        assert!(matches!(after.choices[0], Choice::Unused(_)));
        assert_eq!(after.choices[2..], [Choice::Refused, Choice::Refused]);
    }

    #[test]
    fn the_claim_order_puts_drawing_discrete_gpus_first() {
        let found = [
            gpu(SHOWS, true),
            gpu(INTEGRATED, true),
            gpu(PROBED, false),
            gpu(DISCRETE, false),
        ];
        let mut asked = Vec::new();
        let chosen = select(&found, |index| {
            asked.push(index);
            true
        });
        assert_eq!(asked, [3, 2, 1, 0]);
        assert_eq!(chosen.claimed, [3, 2, 1, 0]);
        assert_eq!(
            chosen.choices,
            [
                Choice::Later { beside: 3 },
                Choice::Later { beside: 3 },
                Choice::Later { beside: 3 },
                Choice::First
            ]
        );
    }

    /// The memory plan follows the selection: a share where an integrated
    /// GPU draws, a display reserve where one only shows beside a drawing
    /// discrete GPU, the heap alone where no integrated GPU starts.
    #[test]
    fn the_memory_plan_follows_which_gpus_start() {
        // A desktop card; two cards.
        assert_eq!(memory(&[gpu(DISCRETE, true)]), Memory::Own);
        assert_eq!(
            memory(&[gpu(DISCRETE, false), gpu(DISCRETE, true)]),
            Memory::Own
        );
        // An integrated GPU alone draws, whether its table or its probe
        // tells its kind.
        assert_eq!(memory(&[gpu(INTEGRATED, true)]), Memory::Shared);
        assert_eq!(memory(&[gpu(PROBED, true)]), Memory::Shared);
        // The commonest laptop: an NVIDIA GPU (NVK) draws, the AMD or Intel
        // graphics drives the panel. Its driver's pages are a display's, and
        // the NVIDIA GPU's system-memory buffers must not be held to them.
        for integrated in [INTEGRATED, SHOWS] {
            let laptop = [gpu(integrated, true), gpu(DISCRETE, false)];
            assert_eq!(memory(&laptop), Memory::Display);
            // Both start: the memory plan counts each one's aperture.
            let mut both = started(&laptop);
            both.sort_unstable();
            assert_eq!(both, [0, 1]);
        }
        // A mux set to the discrete GPU: the integrated one is left.
        let muxed = [gpu(INTEGRATED, false), gpu(DISCRETE, true)];
        assert_eq!(memory(&muxed), Memory::Own);
        assert_eq!(started(&muxed), [1]);
        // An integrated GPU the loader could not tell starts beside a card.
        let desktop = [gpu(PROBED, false), gpu(DISCRETE, true)];
        assert_eq!(memory(&desktop), Memory::Display);
        // A discrete GPU whose stack does not draw (no NVK in the image)
        // beside an integrated one that does: the integrated one draws.
        let shows = Driven {
            draws: false,
            integrated: Some(false),
        };
        assert_eq!(
            memory(&[gpu(INTEGRATED, true), gpu(shows, false)]),
            Memory::Shared
        );
        // Only a display-only integrated GPU: no Metal device, its pages a
        // display's.
        assert_eq!(memory(&[gpu(SHOWS, true)]), Memory::Display);
        // An integrated GPU no stack drives beside a card.
        assert_eq!(
            memory(&[unsupported(true), gpu(DISCRETE, false)]),
            Memory::Own
        );
        // Where GPUs draw, the plan shares memory exactly when the GPU that
        // `metal_devices` will serve the Metal device with is integrated.
        for kinds in [&[true][..], &[true, false], &[false, true], &[true, true]] {
            let found: Vec<Found> = kinds
                .iter()
                .map(|&integrated| gpu(if integrated { INTEGRATED } else { DISCRETE }, true))
                .collect();
            let candidates: Vec<_> = kinds
                .iter()
                .map(|&integrated| crate::stack::Candidate {
                    draws: true,
                    integrated,
                    drives_display: false,
                })
                .collect();
            let serves = crate::stack::metal_devices(&candidates).serve[0];
            assert_eq!(memory(&found) == Memory::Shared, kinds[serves], "{kinds:?}");
        }
    }

    /// The plan keeps room for an integrated GPU's pages exactly where one
    /// this image draws with may draw in a discrete GPU's place: an NVIDIA
    /// or AMD card beside an AMD or Intel integrated GPU, whether the
    /// monitor is on the card (the integrated GPU left) or the panel on the
    /// integrated GPU (it shows the display). An image without the
    /// integrated GPU's driver (a release, NVK alone) sees no stack for it,
    /// and an integrated GPU that draws already, or two cards, need none.
    #[test]
    fn room_is_kept_where_an_integrated_gpu_may_draw_in_a_discrete_ones_place() {
        for integrated in [INTEGRATED, PROBED] {
            for card_screen in [true, false] {
                let pc = [gpu(integrated, !card_screen), gpu(DISCRETE, card_screen)];
                assert!(
                    integrated_may_draw_instead(&pc),
                    "{integrated:?} {card_screen}"
                );
                assert_ne!(memory(&pc), Memory::Shared);
                // The release: no stack drives the integrated GPU.
                let release = [unsupported(!card_screen), gpu(DISCRETE, card_screen)];
                assert!(!integrated_may_draw_instead(&release));
                assert_eq!(memory(&release), Memory::Own);
            }
        }
        // An integrated GPU whose stack does not draw cannot take the
        // Metal device; one that draws alone has its share already.
        assert!(!integrated_may_draw_instead(&[
            gpu(SHOWS, true),
            gpu(DISCRETE, false)
        ]));
        assert!(!integrated_may_draw_instead(&[gpu(INTEGRATED, true)]));
        assert!(!integrated_may_draw_instead(&[
            gpu(DISCRETE, true),
            gpu(DISCRETE, false)
        ]));
        assert!(!integrated_may_draw_instead(&[gpu(DISCRETE, true)]));
    }

    #[test]
    fn choices_are_said_in_plain_words() {
        let name = |index: usize| format!("GPU {index}");
        assert_eq!(
            Choice::Left { beside: 1 }.describe(&name),
            "left with the firmware: it would only show the frames GPU 1 draws, and the \
             firmware's picture is not on it"
        );
        assert_eq!(
            Choice::Later { beside: 0 }.describe(&name),
            "started after GPU 0: it may only show the frames that GPU draws"
        );
        assert_eq!(
            PROBED.describe(),
            "integrated or discrete, as its probe tells, draws"
        );
        assert_eq!(
            vec![DISCRETE.describe(), SHOWS.describe()],
            [
                "discrete, draws",
                "integrated, shows only: this image has no Vulkan driver for it"
            ]
        );
    }

    /// The loader's memory plan and the engine's census tell each GPU's kind
    /// alike ([`driven`], the plan's kinds passed on in [`Planned`]), so on
    /// the same census they start the same GPUs and agree on the memory; a
    /// GPU the plan did not see, below a port powered on after the
    /// firmware's exit, is named.
    #[cfg(feature = "amdgpu-port")]
    #[test]
    fn the_plan_and_the_engine_select_alike_over_one_census() {
        use crate::{
            Adapter,
            stack::{AMD, GpuStack, NVIDIA},
        };
        fn at(bus: u8, vendor: crate::Vendor, device: u16) -> Adapter {
            Adapter {
                address: Address {
                    segment: 0,
                    bus,
                    device: 0,
                    function: 0,
                },
                vendor,
                device,
                class: [0x03, 0x00, 0x00],
                revision: 0,
            }
        }
        // As images with the Vulkan drivers: this test binary links none.
        let drawn = |driven: Driven| Driven {
            draws: true,
            ..driven
        };
        // A GPU, the kind its IP discovery table gives the loader (None
        // where its driver's table tells), and whether the firmware's
        // picture is on it.
        type Gpu = (Adapter, &'static GpuStack, Option<bool>, bool);
        let nvidia = at(1, crate::Vendor::Nvidia, 0x2c02);
        // amdgpu takes these by class; only IP discovery tells their kind.
        let igpu = at(0x65, crate::Vendor::Amd, 0x13c0);
        let dgpu = at(3, crate::Vendor::Amd, 0x744c);
        assert_eq!(AMD.kernel.table_integrated(&igpu), None);
        let plan = |census: &[Gpu]| -> (Planned, Vec<Found>) {
            let found: Vec<Found> = census
                .iter()
                .map(|&(adapter, stack, discovered, screen)| Found {
                    // The loader takes a GPU it cannot tell as integrated.
                    stack: Ok(drawn(driven(
                        stack,
                        &adapter,
                        Some(discovered.unwrap_or(true)),
                    ))),
                    firmware_screen: screen,
                })
                .collect();
            let mut planned = Planned::new(memory(&found));
            for (&(adapter, ..), found) in census.iter().zip(&found) {
                planned.see(adapter.address, found.stack.ok().and_then(|d| d.integrated));
            }
            (planned, found)
        };
        let engine = |census: &[Gpu], planned: &Planned| -> Vec<Found> {
            census
                .iter()
                .map(|&(adapter, stack, _, screen)| Found {
                    stack: Ok(drawn(driven(
                        stack,
                        &adapter,
                        planned.kind(adapter.address),
                    ))),
                    firmware_screen: screen,
                })
                .collect()
        };
        let addresses =
            |census: &[Gpu]| -> Vec<Address> { census.iter().map(|gpu| gpu.0.address).collect() };
        let alike = |census: &[Gpu]| {
            let (planned, found) = plan(census);
            let now = engine(census, &planned);
            let chosen = all(&now);
            assert_eq!(all(&found), chosen);
            assert_eq!(memory(&now), planned.memory);
            assert!(unplanned(&planned, &addresses(census), &chosen).is_empty());
            (planned.memory, chosen.choices)
        };
        // An NVIDIA card with the monitor, an AMD iGPU without: the iGPU is
        // left for both, and the plan reserves nothing for it. Told by its
        // driver's table alone, the engine claimed it (the defect).
        assert_eq!(
            alike(&[
                (igpu, &AMD, Some(true), false),
                (nvidia, &NVIDIA, None, true)
            ]),
            (Memory::Own, vec![Choice::Left { beside: 1 }, Choice::First])
        );
        assert_eq!(
            all(&[
                gpu(drawn(driven(&AMD, &igpu, None)), false),
                gpu(drawn(driven(&NVIDIA, &nvidia, None)), true)
            ])
            .choices[0],
            Choice::Later { beside: 1 }
        );
        // The panel on the iGPU: it shows the card's frames for both.
        assert_eq!(
            alike(&[
                (igpu, &AMD, Some(true), true),
                (nvidia, &NVIDIA, None, false)
            ]),
            (
                Memory::Display,
                vec![Choice::Later { beside: 1 }, Choice::First]
            )
        );
        // An AMD card beside the AMD iGPU, both told by IP discovery.
        assert_eq!(
            alike(&[
                (igpu, &AMD, Some(true), true),
                (dgpu, &AMD, Some(false), false)
            ]),
            (
                Memory::Display,
                vec![Choice::Later { beside: 1 }, Choice::First]
            )
        );
        // The iGPU alone draws in its share.
        assert_eq!(
            alike(&[(igpu, &AMD, Some(true), true)]),
            (Memory::Shared, vec![Choice::First])
        );
        // A laptop whose discrete GPU's port was off at the loader's census:
        // the plan saw the iGPU alone and gave it a share; the engine's
        // census, after the port's power-on, starts the card too, which the
        // plan did not count, and names it.
        for card in [(nvidia, &NVIDIA), (dgpu, &AMD)] {
            let (planned, _) = plan(&[(igpu, &AMD, Some(true), true)]);
            assert_eq!(planned.memory, Memory::Shared);
            let census: [Gpu; 2] = [(igpu, &AMD, None, true), (card.0, card.1, None, false)];
            let chosen = all(&engine(&census, &planned));
            assert!(chosen.claimed.contains(&1));
            assert_eq!(unplanned(&planned, &addresses(&census), &chosen), [1]);
        }
        // Intel's GPUs are told by their driver's table, i915's or xe's,
        // whichever this image offers the GPU first: an Alder Lake iGPU is
        // integrated and an Arc A770 (DG2) discrete, for the plan and the
        // engine alike, whatever the loader passes on.
        #[cfg(any(feature = "xe-port", feature = "i915-port"))]
        {
            let intel = |bus: u8, device: u16| {
                let adapter = at(bus, crate::Vendor::Intel, device);
                let stack = GpuStack::select(&adapter).expect("an Intel stack takes it");
                (adapter, stack)
            };
            let (alder_lake, igpu_stack) = intel(0, 0x46a6);
            let (arc, arc_stack) = intel(3, 0x56a0);
            assert_eq!(igpu_stack.kernel.table_integrated(&alder_lake), Some(true));
            assert_eq!(arc_stack.kernel.table_integrated(&arc), Some(false));
            for told in [None, Some(true), Some(false)] {
                assert_eq!(driven(igpu_stack, &alder_lake, told).integrated, Some(true));
                assert_eq!(driven(arc_stack, &arc, told).integrated, Some(false));
            }
            // A laptop whose panel is on the Intel iGPU and whose NVIDIA or
            // AMD card draws: the iGPU shows the card's frames, and the plan
            // reserves its display memory. The AMD card's kind is told by its
            // IP discovery table.
            for card in [(nvidia, &NVIDIA, None), (dgpu, &AMD, Some(false))] {
                assert_eq!(
                    alike(&[
                        (alder_lake, igpu_stack, None, true),
                        (card.0, card.1, card.2, false)
                    ]),
                    (
                        Memory::Display,
                        vec![Choice::Later { beside: 1 }, Choice::First]
                    )
                );
            }
            // The same with an Arc card beside the iGPU.
            assert_eq!(
                alike(&[
                    (alder_lake, igpu_stack, None, true),
                    (arc, arc_stack, None, false)
                ]),
                (
                    Memory::Display,
                    vec![Choice::Later { beside: 1 }, Choice::First]
                )
            );
            // A monitor on the Arc card: the iGPU is left, nothing shared.
            assert_eq!(
                alike(&[
                    (alder_lake, igpu_stack, None, false),
                    (arc, arc_stack, None, true)
                ]),
                (Memory::Own, vec![Choice::Left { beside: 1 }, Choice::First])
            );
            // The iGPU alone draws in its share.
            assert_eq!(
                alike(&[(alder_lake, igpu_stack, None, true)]),
                (Memory::Shared, vec![Choice::First])
            );
        }
    }
}
