//! The chain's account (`stela_compiler::x86_64::chain::Account`), which
//! native code keeps in a host register: a native run allowed `L - 1`
//! transfers ends after `L` blocks in the state single blocks reach, one
//! native run each, for every `L` up to the program's end: registers, flags,
//! memory, PC, retired instructions and any data abort's syndrome, address
//! and return address. So every transfer kind the programs take is the one
//! that finds the budget spent in some run, with one unit left just before:
//! links in and out of a page, thunks, BL's paired frames and RET's paired
//! returns, BLR and BR through the probes, self-loops and regions that
//! settle before their exits, and faults in the middle of a chained run.
#![cfg(all(
    feature = "test-support",
    feature = "std",
    target_arch = "x86_64",
    target_os = "linux"
))]
use anyhow::{Result, ensure};
use stela_compiler::{GuestState, TargetCpu};
use stela_engine::machine::{Stop, system::Register};
#[path = "support/store_host.rs"]
mod store_host;
use store_host::{Mapping, machine};

const PAGE: u64 = 4096;
const WORDS_PER_PAGE: usize = PAGE as usize / 4;
const BYTES: u64 = 4 * PAGE;
/// The most blocks a case's single-block reference runs.
const MOST_BLOCKS: usize = 400;

const RET: u32 = 0xd65f_03c0;
const BLR_X19: u32 = 0xd63f_0260;
const BR_X20: u32 = 0xd61f_0280;
const LDR_X8_POST: u32 = 0xf840_8408; // ldr x8,[x0],#8
const ADDS_X9: u32 = 0xab08_0129; // adds x9,x9,x8
const SUBS_X2: u32 = 0xf100_0442; // subs x2,x2,#1
const SUBS_X3: u32 = 0xf100_0463; // subs x3,x3,#1
const MOV_X2_X4: u32 = 0xaa04_03e2; // mov x2,x4
const CSEL_X15_HI: u32 = 0x9a88_81ef; // csel x15,x15,x8,hi
const SUFFIX: u32 = 0x9100_04e7; // add x7,x7,#1
const NE: u32 = 1;

fn add_one(register: u32) -> u32 {
    0x9100_0400 | register << 5 | register
}
fn branch(op: u32, from: usize, to: usize) -> u32 {
    op | ((to as i64 - from as i64) as u32 & 0x03ff_ffff)
}
/// B and BL from word `from` to word `to`.
fn b(from: usize, to: usize) -> u32 {
    branch(0x1400_0000, from, to)
}
fn bl(from: usize, to: usize) -> u32 {
    branch(0x9400_0000, from, to)
}
fn b_cond(condition: u32, from: usize, to: usize) -> u32 {
    0x5400_0000 | (((to as i64 - from as i64) as u32 & 0x7ffff) << 5) | condition
}
/// TBZ Wt, #0 from word `from` to word `to`.
fn tbz0(rt: u32, from: usize, to: usize) -> u32 {
    0x3600_0000 | (((to as i64 - from as i64) as u32 & 0x3fff) << 5) | rt
}

/// The words of four pages, each page's own from its first word, the
/// rest of each but the last filled with BRK; the program ends after the
/// last page's words.
fn layout(pages: [Vec<u32>; 4]) -> Vec<u32> {
    let mut words = Vec::new();
    for (page, code) in pages.into_iter().enumerate() {
        assert!(words.len() == page * WORDS_PER_PAGE && code.len() <= WORDS_PER_PAGE);
        words.extend(code);
        if page < 3 {
            words.resize((page + 1) * WORDS_PER_PAGE, 0xd420_0000);
        }
    }
    words
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }
}

struct Program {
    name: &'static str,
    words: Vec<u32>,
    /// Registers and data of a case, from the code and data mappings.
    setup: fn(&mut Rng, u64, u64, &mut GuestState),
}

const P1: usize = WORDS_PER_PAGE;
const P2: usize = 2 * WORDS_PER_PAGE;
const P3: usize = 3 * WORDS_PER_PAGE;

fn programs() -> Vec<Program> {
    vec![
        Program {
            name: "calls across pages, flags across a link",
            words: layout([
                vec![
                    bl(0, P1),        //  0: bl f1
                    add_one(10),      //     add x10,x10,#1
                    BLR_X19,          //     blr x19 (f2)
                    LDR_X8_POST,      //     ldr x8,[x0],#8
                    ADDS_X9,          //     adds x9,x9,x8
                    tbz0(8, 5, 7),    //     tbz w8,#0,7f
                    add_one(11),      //     add x11,x11,#1
                    CSEL_X15_HI,      //  7: csel x15,x15,x8,hi
                    SUBS_X2,          //     subs x2,x2,#1
                    b_cond(NE, 9, 0), //     b.ne 0b
                    BR_X20,           //     br x20 (end)
                ],
                vec![add_one(12), RET],
                vec![
                    tbz0(10, P2, P2 + 2), //     tbz w10,#0,1f
                    add_one(14),          //     add x14,x14,#1
                    RET,                  //  1: ret
                ],
                vec![SUFFIX],
            ]),
            setup: |rng, code, data, state| {
                state.x[2] = 1 + rng.below(40);
                state.x[0] = data + PAGE * (1 + rng.below(2)) - 8 * rng.below(40);
                state.x[19] = code + 4 * P2 as u64;
                state.x[20] = code + 4 * P3 as u64;
            },
        },
        Program {
            name: "a self-loop's settle, then a call",
            words: layout([
                vec![
                    LDR_X8_POST,      //  0: ldr x8,[x0],#8
                    ADDS_X9,          //     adds x9,x9,x8
                    SUBS_X2,          //     subs x2,x2,#1
                    b_cond(NE, 3, 0), //     b.ne 0b
                    bl(4, P1),        //     bl f1
                    CSEL_X15_HI,      //     csel x15,x15,x8,hi
                    MOV_X2_X4,        //     mov x2,x4
                    SUBS_X3,          //     subs x3,x3,#1
                    b_cond(NE, 8, 0), //     b.ne 0b
                    b(9, P3),         //     b end
                ],
                vec![add_one(12), RET],
                vec![],
                vec![SUFFIX],
            ]),
            setup: |rng, _, data, state| {
                state.x[4] = 1 + rng.below(8);
                state.x[2] = state.x[4];
                state.x[3] = 1 + rng.below(8);
                state.x[0] = data + PAGE * (1 + rng.below(2)) - 8 * rng.below(64);
            },
        },
        Program {
            name: "memmoves by 16 bytes and their byte tails, out of the page",
            words: layout([
                vec![
                    0xaa06_03e2,       // 0: mov x2,x6
                    0xf100_405f,       //    cmp x2,#16
                    0x5400_00c3,       //    b.lo 2f
                    0xa8c1_1023,       // 1: ldp x3,x4,[x1],#16
                    0xa881_1003,       //    stp x3,x4,[x0],#16
                    0xd100_4042,       //    sub x2,x2,#16
                    0xf100_405f,       //    cmp x2,#16
                    0x54ff_ff82,       //    b.hs 1b
                    0xb400_00a2,       // 2: cbz x2,4f
                    0x3840_1423,       // 3: ldrb w3,[x1],#1
                    0x3800_1403,       //    strb w3,[x0],#1
                    0xf100_0442,       //    subs x2,x2,#1
                    0x54ff_ffa1,       //    b.ne 3b
                    0xf100_04a5,       // 4: subs x5,x5,#1
                    b_cond(NE, 14, 0), //    b.ne 0b
                    b(15, P3),         //    b end
                ],
                vec![],
                vec![],
                vec![SUFFIX],
            ]),
            setup: |rng, _, data, state| {
                state.x[1] = data + rng.below(2 * PAGE);
                state.x[0] = data + rng.below(2 * PAGE);
                state.x[6] = rng.below(64);
                state.x[5] = 1 + rng.below(10);
            },
        },
    ]
}

/// Where a run stopped.
#[derive(Debug, PartialEq)]
struct Outcome {
    stop: Stop,
    blocks: u64,
    pc: u64,
    state: GuestState,
    retired: u64,
    memory: Vec<u8>,
    abort: Option<(u64, u64, u64)>,
}

struct Case<'a> {
    words: &'a [u32],
    initial: &'a GuestState,
    contents: &'a [u8],
    protected: Option<u64>,
    regions: bool,
}

/// The offset of the lower-level synchronous vector in the vector table.
const LOWER_SYNCHRONOUS: u64 = 0x400;
/// Bytes of the vector table.
const VECTORS_BYTES: u64 = 0x800;
/// Marks an [`Outcome::pc`] in the machine's own vector table, an offset
/// from it.
const IN_VECTORS: u64 = 1 << 63;

/// Run a case: `blocks` blocks at most in native runs that chain as far
/// as the limit lets them, or with `single`, one block per native run. What
/// each run's stop left, the native runs' transfers in all.
fn run(
    code: &Mapping,
    data: &Mapping,
    case: &Case,
    blocks: u64,
    single: bool,
) -> Result<(Vec<Outcome>, u64)> {
    let mut machine = machine(code, data, case.words, TargetCpu::detect())?;
    machine.set_regions(case.regions);
    ensure!(data.write(data.0, case.contents), "write data");
    if let Some(page) = case.protected {
        data.protect(page as usize * PAGE as usize, PAGE as usize, false)?;
    }
    machine.cpu.state = case.initial.clone();
    machine.cpu.pc = code.0;
    let (start, retired) = (machine.statistics.blocks, machine.cpu.retired);
    let vector = machine.host.lower_synchronous();
    let vectors = vector - LOWER_SYNCHRONOUS;
    let mut outcomes = Vec::new();
    loop {
        let limit = match single {
            true => machine.statistics.blocks + 1,
            false => start + blocks,
        };
        let stop = machine.run(limit);
        // Each machine maps its own vector page, wherever the process's
        // other mappings leave room: an abort stops at its vector, a run
        // whose budget ran out as it took the exception stops in its table,
        // each an offset from it, and the program's end at its own address.
        let (stop, pc) = match stop {
            Stop::Breakpoint(at) if at == vector => {
                (Stop::Breakpoint(0), machine.cpu.pc.wrapping_sub(vector))
            }
            Stop::Breakpoint(at) => (Stop::Breakpoint(1), at.wrapping_sub(code.0)),
            stop if machine.cpu.pc.wrapping_sub(vectors) < VECTORS_BYTES => {
                (stop, IN_VECTORS | machine.cpu.pc.wrapping_sub(vectors))
            }
            stop => (stop, machine.cpu.pc.wrapping_sub(code.0)),
        };
        let abort = (stop == Stop::Breakpoint(0)).then(|| {
            (
                machine.cpu.registers.get(Register::ESR_EL1, 0),
                machine.cpu.registers.get(Register::FAR_EL1, 0),
                machine
                    .cpu
                    .registers
                    .get(Register::ELR_EL1, 0)
                    .wrapping_sub(code.0),
            )
        });
        let state = machine.cpu.state.clone();
        let ended = stop != Stop::Budget;
        let blocks_run = machine.statistics.blocks - start;
        // The protected page read too, then protected again.
        if let Some(page) = case.protected {
            data.protect(page as usize * PAGE as usize, PAGE as usize, true)?;
        }
        let memory = data.read(data.0, BYTES as usize).expect("readable data");
        if let Some(page) = case.protected {
            data.protect(page as usize * PAGE as usize, PAGE as usize, false)?;
        }
        outcomes.push(Outcome {
            stop,
            blocks: blocks_run,
            pc,
            state,
            retired: machine.cpu.retired - retired,
            memory,
            abort,
        });
        if ended || !single || blocks_run >= blocks {
            break;
        }
    }
    if let Some(page) = case.protected {
        data.protect(page as usize * PAGE as usize, PAGE as usize, true)?;
    }
    Ok((outcomes, machine.statistics.chained))
}

#[test]
fn chained_runs_stop_where_single_blocks_do_at_every_transfer() -> Result<()> {
    chained_differential()
}

/// Two machines in one process, as a board's PEs are, run the differential
/// at once and each agrees with single blocks: neither takes state of the
/// other. Their mappings interleave, so a PC in a machine's vector table
/// is an offset from it: an offset from the code named where the process
/// had room for the table, and disagreed in about half the runs.
#[test]
fn two_machines_at_once_stop_where_single_blocks_do() -> Result<()> {
    let runs: Vec<_> = (0..2)
        .map(|_| std::thread::spawn(chained_differential))
        .collect();
    for run in runs {
        run.join().expect("a differential's thread")?;
    }
    Ok(())
}

/// Each case's single-block reference and its chained runs, with another
/// mapping made between them, as another machine of the process makes
/// them: the chained runs' machines map their vector tables elsewhere than
/// the reference's did.
fn chained_differential() -> Result<()> {
    let data = Mapping::new()?;
    let code = Mapping::new()?;
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    let (mut compared, mut chained, mut aborts, mut ended) = (0u64, 0u64, 0u64, 0u64);
    for program in programs() {
        let mut program_chained = 0;
        for case in 0..8 {
            let mut initial = GuestState {
                x: std::array::from_fn(|_| rng.next()),
                nzcv: (rng.next() as u32) & 0xf000_0000,
                ..GuestState::default()
            };
            let contents: Vec<u8> = (0..BYTES).map(|_| rng.next() as u8).collect();
            (program.setup)(&mut rng, code.0, data.0, &mut initial);
            // Some cases fault on a page the loop reaches.
            let protected = (case % 4 == 3).then(|| 1 + rng.below(2));
            let case = Case {
                words: &program.words,
                initial: &initial,
                contents: &contents,
                protected,
                regions: case % 2 == 1,
            };
            let what = |blocks: usize| {
                format!(
                    "{} regions {} protected {protected:?} blocks {blocks}",
                    program.name, case.regions
                )
            };
            let (single, _) = run(&code, &data, &case, MOST_BLOCKS as u64, true)?;
            let _elsewhere = Mapping::new()?;
            for (index, expected) in single.iter().enumerate() {
                let blocks = index + 1;
                let (outcome, transfers) = run(&code, &data, &case, blocks as u64, false)?;
                let [outcome] = outcome.try_into().expect("one stop");
                assert_eq!(
                    (
                        outcome.stop,
                        outcome.blocks,
                        outcome.pc,
                        outcome.retired,
                        outcome.abort
                    ),
                    (
                        expected.stop.clone(),
                        expected.blocks,
                        expected.pc,
                        expected.retired,
                        expected.abort
                    ),
                    "{}",
                    what(blocks)
                );
                assert_eq!(outcome.state, expected.state, "{}", what(blocks));
                assert!(
                    outcome.memory == expected.memory,
                    "{}: memory",
                    what(blocks)
                );
                compared += 1;
                program_chained += transfers;
            }
            let last = single.last().expect("a stop");
            aborts += u64::from(last.abort.is_some());
            ended += u64::from(last.stop == Stop::Breakpoint(1));
        }
        assert!(
            program_chained > 100,
            "{}: {program_chained} transfers",
            program.name
        );
        chained += program_chained;
    }
    assert!(compared > 800, "{compared} runs compared");
    assert!(aborts > 0 && ended > 0, "{aborts} aborts, {ended} ends");
    std::eprintln!("{compared} runs compared, {chained} transfers, {aborts} aborts");
    Ok(())
}

/// What `PES RETIRED` reports is the instructions the guest ran, counted
/// apart from the engine: a ring of blocks with a self-loop whose taken
/// TBZ skips an instruction, and links within and across pages, stopped
/// every few transfers, retires exactly what its registers say it ran: in
/// blocks, self-loops and regions, profiled or not, on both link gates. A
/// self-loop entry of one even iteration settles one below zero before its
/// fall-through, so a run that stops there reads the account below zero.
#[test]
fn native_runs_retire_exactly_what_they_ran() -> Result<()> {
    use stela_compiler::x86_64::chain::LinkGate;
    let data = Mapping::new()?;
    let code = Mapping::new()?;
    let words = layout([
        vec![
            add_one(5),       // 0: add x5,x5,#1
            tbz0(5, 1, 3),    //    tbz w5,#0,3f
            add_one(6),       //    add x6,x6,#1
            SUBS_X2,          // 3: subs x2,x2,#1
            b_cond(NE, 4, 0), //    b.ne 0b
            b(5, P1),         //    b P1
        ],
        vec![
            add_one(7),   // P1: add x7,x7,#1
            MOV_X2_X4,    //     mov x2,x4
            b(P1 + 2, 0), //     b 0
        ],
        vec![],
        vec![],
    ]);
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut gates = Vec::new();
    for target in [TargetCpu::detect(), TargetCpu::Baseline] {
        gates.push(LinkGate::for_target(target));
        // Entries of one iteration, odd and even in turn, and of three.
        for (config, iterations) in (0..8).flat_map(|config| [(config, 1), (config, 3)]) {
            let (self_loops, regions, profiling) =
                (config & 1 != 0, config & 2 != 0, config & 4 != 0);
            let what = format!(
                "{:?} loops {self_loops} regions {regions} profiling {profiling} \
                 iterations {iterations}",
                LinkGate::for_target(target)
            );
            let mut machine = machine(&code, &data, &words, target)?;
            machine.set_self_loops(self_loops);
            machine.set_regions(regions);
            machine.set_step_timing(profiling);
            let mut state = GuestState::default();
            state.x[4] = iterations;
            state.x[2] = state.x[4];
            machine.cpu.state = state;
            machine.cpu.pc = code.0;
            let (retired, start) = (machine.cpu.retired, machine.statistics.blocks);
            for _ in 0..300 {
                let stop = machine.run(machine.statistics.blocks + 1 + rng.below(12));
                ensure!(stop == Stop::Budget, "{what}: {stop:?}");
                let state = &machine.cpu.state;
                // Four per iteration and one more on odd x5; four per pass
                // through P1, of which `b P1` runs before x7 counts it.
                let ran = 4 * state.x[5] + state.x[6] + 4 * state.x[7];
                let at = (machine.cpu.pc - code.0) as usize / 4;
                let expected = match at {
                    0 | 5 => ran,
                    P1 => ran + 1,
                    // In an iteration: its ADD and TBZ, and its ADD of x6.
                    2 | 3 => ran - 2,
                    _ => anyhow::bail!("{what}: stopped at word {at}"),
                };
                let counted = machine.cpu.retired - retired;
                ensure!(
                    counted == expected,
                    "{what}: {counted} retired at word {at}, {expected} ran (x5 {} x6 {} x7 {})",
                    state.x[5],
                    state.x[6],
                    state.x[7]
                );
            }
            let transfers = machine.statistics.blocks - start;
            ensure!(transfers > 1000, "{what}: {transfers} transfers");
        }
    }
    if TargetCpu::detect().supports(TargetCpu::Baseline.with_bmi2().with_fast_pdep()) {
        ensure!(gates == [LinkGate::Pext, LinkGate::Jrcxz], "{gates:?}");
    }
    Ok(())
}
