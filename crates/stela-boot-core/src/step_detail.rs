//! Which ended details of a running step the boot log keeps as `STEP
//! DETAIL` lines. A detail held [`LOG_MILLIS`] or more is worth a line,
//! such as a device a driver took long to start; but a detail that counts
//! (the guest's executed blocks, replaced each second for as long as macOS
//! runs) would then write one line a second, a fifth of a long boot's log.
//! So each shape of detail (its text with each run of digits as `#`) is
//! remembered for the step, the last [`SHAPES`] of them, and a detail of a
//! shape the step logged before is logged only:
//!
//! - when it was held [`LONG_MILLIS`] or more, or at least twice as long as
//!   the last one logged of its shape: a stall, or a device slower than the
//!   ones before;
//! - when [`REPEAT_MILLIS`] passed since the last line of its shape;
//! - when it is the step's last detail.
//!
//! Every other detail is counted, and the next line of the shape says how
//! many like it were not logged. A shape's first detail in a step is
//! always logged. The boot progress names the stage the sampled instruction
//! was in, so its shape changes back and forth between a few stages every
//! second or two: each of those shapes keeps its own minute.
use alloc::{string::String, vec::Vec};

/// The shortest hold of a detail worth a line.
pub const LOG_MILLIS: u64 = 250;
/// A hold this long is always logged.
pub const LONG_MILLIS: u64 = 5_000;
/// The longest a shape goes without a line while its details are held.
pub const REPEAT_MILLIS: u64 = 60_000;
/// The most shapes a step remembers; a new one beyond them takes the place
/// of the one logged longest ago.
pub const SHAPES: usize = 8;

/// One shape's last logged detail, and what was left out since.
#[derive(Debug)]
struct Shape {
    text: String,
    held: u64,
    logged_at: u64,
    unlogged: u32,
}

/// The shapes the running step logged.
#[derive(Debug)]
pub struct Thinning {
    shapes: Vec<Shape>,
}

/// Whether `detail`'s shape is `shape`: the same text, each run of digits
/// in either standing for any other.
fn same_shape(shape: &str, detail: &str) -> bool {
    fn runs(text: &str) -> impl Iterator<Item = char> + '_ {
        let mut digits = false;
        text.chars().filter_map(move |c| {
            let was = core::mem::replace(&mut digits, c.is_ascii_digit());
            match (c.is_ascii_digit(), was) {
                (true, true) => None,
                (true, false) => Some('#'),
                (false, _) => Some(c),
            }
        })
    }
    runs(shape).eq(runs(detail))
}

impl Default for Thinning {
    fn default() -> Self {
        Self::new()
    }
}

impl Thinning {
    pub const fn new() -> Self {
        Self { shapes: Vec::new() }
    }

    /// A step begins: each shape's first detail is logged again.
    pub fn begin(&mut self) {
        self.shapes.clear();
    }

    /// The running step's `detail`, held `held` ms, ended at `now` ms on
    /// any clock that does not go back; `last` when the step ends with it.
    /// `Some` with the count of details of its shape left out since its
    /// last line when it is to be logged.
    pub fn ended(&mut self, detail: &str, held: u64, now: u64, last: bool) -> Option<u32> {
        if detail.is_empty() || held < LOG_MILLIS {
            return None;
        }
        let Some(shape) = self
            .shapes
            .iter_mut()
            .find(|shape| same_shape(&shape.text, detail))
        else {
            if self.shapes.len() >= SHAPES
                && let Some(oldest) = self
                    .shapes
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, shape)| shape.logged_at)
                    .map(|(index, _)| index)
            {
                self.shapes.swap_remove(oldest);
            }
            self.shapes.push(Shape {
                text: String::from(detail),
                held,
                logged_at: now,
                unlogged: 0,
            });
            return Some(0);
        };
        if !last
            && held < LONG_MILLIS
            && held < shape.held.saturating_mul(2)
            && now.saturating_sub(shape.logged_at) < REPEAT_MILLIS
        {
            shape.unlogged = shape.unlogged.saturating_add(1);
            return None;
        }
        let unlogged = shape.unlogged;
        shape.text.clear();
        shape.text.push_str(detail);
        shape.held = held;
        shape.logged_at = now;
        shape.unlogged = 0;
        Some(unlogged)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{format, vec::Vec};

    /// The guest's boot progress, replaced each second for ten minutes:
    /// the first, one a minute and the last are logged.
    #[test]
    fn a_counting_detail_is_logged_once_a_minute_with_what_was_left_out() {
        let mut thinning = Thinning::default();
        thinning.begin();
        let mut logged = Vec::new();
        for second in 1..=600u64 {
            let detail = format!(
                "macOS kernel | {} blocks executed, {}/s",
                second * 1_000_000,
                1_000_000 + second % 7
            );
            if let Some(unlogged) = thinning.ended(&detail, 1_000, second * 1_000, second == 600) {
                logged.push((second, unlogged));
            }
        }
        assert_eq!(logged.len(), 11, "{logged:?}");
        assert_eq!(logged[0], (1, 0));
        assert_eq!(logged[1], (61, 59));
        assert_eq!(logged[9], (541, 59));
        // The step's last detail, whenever it comes.
        assert_eq!(logged[10], (600, 58));
    }

    /// The boot progress as a VM boot logged it: the sampled stage flips
    /// between three every second or two. Each stage keeps its own minute:
    /// 31 lines in ten minutes, not one a second.
    #[test]
    fn alternating_stages_are_each_logged_once_a_minute() {
        let stages = ["macOS kernel", "Boot execution", "Secure monitor (SPTM)"];
        let mut thinning = Thinning::default();
        thinning.begin();
        let mut logged = Vec::new();
        for second in 1..=600u64 {
            let stage = stages[(second % 3) as usize];
            let detail = format!(
                "{stage} | {} blocks executed, {}/s",
                second * 1_000_000,
                1_000_000 + second % 7
            );
            if let Some(unlogged) = thinning.ended(&detail, 1_000, second * 1_000, second == 600) {
                logged.push((second, unlogged));
            }
        }
        assert_eq!(logged.len(), 31, "{logged:?}");
        // Each stage's first, then a minute after it with the 19 left out.
        assert_eq!(&logged[..3], [(1, 0), (2, 0), (3, 0)]);
        assert_eq!(&logged[3..6], [(61, 19), (62, 19), (63, 19)]);
        assert_eq!(logged[30], (600, 18));
    }

    /// More shapes than a step remembers: the one logged longest ago goes,
    /// and its next detail is logged as a first.
    #[test]
    fn a_shape_beyond_the_remembered_ones_replaces_the_oldest() {
        let mut thinning = Thinning::default();
        let shape = |index: usize| format!("{} | 1 blocks", "x".repeat(index + 1));
        for index in 0..SHAPES {
            assert_eq!(
                thinning.ended(&shape(index), 1_000, index as u64, false),
                Some(0)
            );
        }
        // All remembered: counted.
        assert_eq!(thinning.ended(&shape(1), 1_000, 100, false), None);
        // One more shape takes the oldest's place (shape 0).
        assert_eq!(thinning.ended(&shape(SHAPES), 1_000, 200, false), Some(0));
        assert_eq!(thinning.ended(&shape(0), 1_000, 300, false), Some(0));
        // Shape 1 was logged longest ago now and went; shape 2 stays.
        assert_eq!(thinning.ended(&shape(2), 1_000, 400, false), None);
    }

    #[test]
    fn changes_stalls_and_slower_details_are_always_logged() {
        let mut thinning = Thinning::default();
        let counting =
            |blocks: u64| format!("Secure monitor (SPTM) | {blocks} blocks executed, 9/s");
        assert_eq!(thinning.ended(&counting(1), 1_000, 1_000, false), Some(0));
        assert_eq!(thinning.ended(&counting(2), 1_000, 2_000, false), None);
        // A new stage: a change of shape.
        let kernel = "macOS kernel | 3 blocks executed, 9/s";
        assert_eq!(thinning.ended(kernel, 1_000, 3_000, false), Some(0));
        assert_eq!(thinning.ended(kernel, 1_000, 4_000, false), None);
        // A stall: held 5 s, and held twice as long as the last logged.
        assert_eq!(thinning.ended(kernel, 5_000, 9_000, false), Some(1));
        assert_eq!(thinning.ended(kernel, 4_000, 13_000, false), None);
        assert_eq!(thinning.ended(kernel, 10_000, 23_000, false), Some(1));
        // Short details are never logged, and not counted.
        assert_eq!(thinning.ended(kernel, 249, 23_249, false), None);
        assert_eq!(thinning.ended("", 9_000, 32_249, true), None);
        // A new step logs its first detail again.
        thinning.begin();
        assert_eq!(thinning.ended(kernel, 1_000, 33_249, false), Some(0));
    }

    /// Devices a driver starts one after another: each slow one that is
    /// no slower than the last logged is counted; a slower one is logged.
    #[test]
    fn a_device_slower_than_the_last_logged_one_is_logged() {
        let mut thinning = Thinning::default();
        assert_eq!(thinning.ended("PCI 0000:01:00.0", 300, 300, false), Some(0));
        assert_eq!(thinning.ended("PCI 0000:02:00.0", 400, 700, false), None);
        assert_eq!(
            thinning.ended("PCI 0000:03:00.0", 900, 1_600, false),
            Some(1)
        );
        // Another text is another shape.
        assert_eq!(
            thinning.ended("adapter 0000:04:00.0", 300, 1_900, false),
            Some(0)
        );
    }

    #[test]
    fn a_shape_stands_each_run_of_digits_for_any_other() {
        assert!(same_shape("a 12 of 345 MB", "a 9 of 1000 MB"));
        assert!(!same_shape("a 12 of 345 MB", "a 12 of 345 GB"));
        assert!(!same_shape("a 12 MB", "a 12 of 345 MB"));
        assert!(same_shape("x", "x"));
        assert!(!same_shape("1", "a"));
    }
}
