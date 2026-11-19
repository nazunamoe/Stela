//! Two heartbeat lines that say, in any boot, where the PEs' time went and
//! what the board's code cache did since the last beat. A report of slow
//! app launches or a sluggish desktop is then one of: no PE idle and the
//! time in guest code (the guest has more work than its PEs run), the time
//! in the engine outside guest code (exits, device models such as the GPU's
//! and the disks', the host's service), the time translating, or the code
//! cache replaced so fast that most installs are of code it held before.
//! Both are sums the engine keeps anyway: neither line times anything new.
use alloc::{format, string::String};

/// One PE's interval, every time in one unit: idle (its host core halted
/// or waited for an event), in compiled guest code, translating (reading
/// and compiling blocks, taking them from the store, installing them,
/// walking and refilling fetch pages) and waiting on other PEs; and the
/// runs of guest code it entered, each of which ended in an exit to the
/// engine.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PeTime {
    pub idle: u64,
    pub guest: u64,
    pub translate: u64,
    pub sync: u64,
    pub runs: u64,
}

impl PeTime {
    /// Its shares of `interval` in percent, the rest last: the engine's
    /// work outside the others (exits, device models, the host's service).
    fn shares(&self, interval: u64) -> [u64; 5] {
        let percent =
            |time: u64| (u128::from(time) * 100 / u128::from(interval.max(1))).min(100) as u64;
        let known = [
            percent(self.idle),
            percent(self.guest),
            percent(self.translate),
            percent(self.sync),
        ];
        let rest = 100u64.saturating_sub(known.iter().sum());
        [known[0], known[1], known[2], known[3], rest]
    }
}

/// `PES SHARE idle%/guest%/translate%/sync%/other%/runs 0:0/55/6/0/39/560057
/// … all:9/52/6/0/33/3912400`: each PE's shares of an `interval` in the
/// times' unit and its runs, then all PEs' together.
pub fn pes_share(interval: u64, pes: &[PeTime]) -> String {
    let mut line = String::from("PES SHARE idle%/guest%/translate%/sync%/other%/runs");
    let mut all = PeTime::default();
    for (pe, time) in pes.iter().enumerate() {
        let [idle, guest, translate, sync, other] = time.shares(interval);
        line.push_str(&format!(
            " {pe}:{idle}/{guest}/{translate}/{sync}/{other}/{}",
            time.runs
        ));
        all.idle += time.idle;
        all.guest += time.guest;
        all.translate += time.translate;
        all.sync += time.sync;
        all.runs += time.runs;
    }
    let [idle, guest, translate, sync, other] =
        all.shares(interval.saturating_mul(pes.len() as u64));
    line.push_str(&format!(
        " all:{idle}/{guest}/{translate}/{sync}/{other}/{}",
        all.runs
    ));
    line
}

/// The board's code cache at a beat: the window's bytes, its parts and
/// those holding code, the blocks it holds, the parts opened and the times
/// a PE forgot every block at once since boot (`flushes`: each PE that
/// applies one guest invalidation of the whole instruction cache forgets
/// them, so one such invalidation on 8 PEs counts 8); and since the last beat
/// the parts taken away to make room, those of them taken early for want
/// of a block slot, the installs and those of blocks the cache held before.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CodeCache {
    pub window: u64,
    pub parts: u32,
    pub held: u32,
    pub blocks: u64,
    pub opened: u32,
    pub flushes: u64,
    pub taken: u64,
    pub forced: u64,
    pub installs: u64,
    pub again: u64,
}

/// `CODE CACHE size=768M parts=31/32 blocks=1594153 turns=14.7 pe_flushes=6
/// taken=10 forced=0 installs=523569 again=52%`: `turns` is how often the
/// whole window was filled since boot (parts opened over its parts),
/// `pe_flushes` the [`CodeCache::flushes`] by every PE together, and
/// `again` the share of the beat's installs that were of blocks the cache
/// held before and took away. A cache under pressure shows several parts
/// taken a beat and `again` near or past half; a larger window lowers both.
pub fn code_cache(cache: &CodeCache) -> String {
    let parts = u64::from(cache.parts.max(1));
    let tenths = u64::from(cache.opened) * 10 / parts;
    format!(
        "CODE CACHE size={}M parts={}/{} blocks={} turns={}.{} pe_flushes={} taken={} forced={} \
         installs={} again={}%",
        cache.window >> 20,
        cache.held,
        cache.parts,
        cache.blocks,
        tenths / 10,
        tenths % 10,
        cache.flushes,
        cache.taken,
        cache.forced,
        cache.installs,
        u128::from(cache.again) * 100 / u128::from(cache.installs.max(1)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_pe_shows_its_shares_and_runs_and_the_rest_as_other() {
        let pes = [
            // Busy: 55% guest code, 6% translating, the rest in the engine.
            PeTime {
                idle: 0,
                guest: 5_500,
                translate: 600,
                sync: 40,
                runs: 560_057,
            },
            // Mostly idle.
            PeTime {
                idle: 7_600,
                guest: 1_500,
                translate: 100,
                sync: 0,
                runs: 9_000,
            },
        ];
        assert_eq!(
            pes_share(10_000, &pes),
            "PES SHARE idle%/guest%/translate%/sync%/other%/runs 0:0/55/6/0/39/560057 \
             1:76/15/1/0/8/9000 all:38/35/3/0/24/569057"
        );
    }

    #[test]
    fn shares_stay_within_the_interval() {
        // Counts published a little after the beat can pass the interval:
        // no share is over 100 and the rest is never negative.
        let late = PeTime {
            idle: 6_000,
            guest: 5_000,
            translate: 20_000,
            sync: 0,
            runs: 1,
        };
        assert_eq!(late.shares(10_000), [60, 50, 100, 0, 0]);
        // No interval and no PE divide by nothing.
        assert_eq!(late.shares(0), [100, 100, 100, 0, 0]);
        assert_eq!(
            pes_share(0, &[]),
            "PES SHARE idle%/guest%/translate%/sync%/other%/runs all:0/0/0/0/100/0"
        );
    }

    #[test]
    fn the_cache_line_states_size_fill_turns_each_pes_flushes_and_what_came_back() {
        // A 16 GiB PC's desktop at 358 s: 768 MiB in 32 parts, 241 opened.
        let cache = CodeCache {
            window: 768 << 20,
            parts: 32,
            held: 31,
            blocks: 1_594_153,
            opened: 241,
            flushes: 6,
            taken: 10,
            forced: 0,
            installs: 523_569,
            again: 273_589,
        };
        assert_eq!(
            code_cache(&cache),
            "CODE CACHE size=768M parts=31/32 blocks=1594153 turns=7.5 pe_flushes=6 taken=10 \
             forced=0 installs=523569 again=52%"
        );
        // A quiet beat and a board with no window yet divide by nothing.
        assert_eq!(
            code_cache(&CodeCache::default()),
            "CODE CACHE size=0M parts=0/0 blocks=0 turns=0.0 pe_flushes=0 taken=0 forced=0 \
             installs=0 again=0%"
        );
    }
}
