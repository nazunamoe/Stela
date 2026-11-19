//! A scan of a host stack's words from its stack pointer up, for the code
//! addresses on it, as a hook that records an allocating stack makes.
//!
//! Only the stack pointer's own page is known to be mapped. A stack may end
//! anywhere above it: a fresh job's stack, a thread's first frames and a
//! stack switched to near its top all run within a page of the end, above
//! which lies an absent guard page. So every word past that page is read
//! through a read a fault ends ([`crate::nofault_read`] on the host), and
//! the scan stops at the first such word that does not read.

const PAGE: u64 = 4096;
const WORD: u64 = 8;

/// How a word is read.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Read {
    /// In the stack pointer's own page, which is mapped.
    Own,
    /// Past it, where the page may be absent: the read must not fault.
    Guarded,
}

/// The words of the stack pointer's own page from `stack` on.
pub fn own_words(stack: u64) -> u64 {
    (PAGE - stack % PAGE) / WORD
}

/// Scans at most `scanned` words up from `stack`, which is 8-byte aligned.
/// `read` reads the word at an address in the given way, or answers `None`
/// when a guarded read faulted, which ends the scan. `keep` hears each word
/// read and answers whether to go on. Returns the words read.
pub fn scan(
    stack: u64,
    scanned: usize,
    mut read: impl FnMut(u64, Read) -> Option<u64>,
    mut keep: impl FnMut(u64) -> bool,
) -> usize {
    let own = own_words(stack);
    for word in 0..scanned as u64 {
        let Some(address) = word
            .checked_mul(WORD)
            .and_then(|offset| stack.checked_add(offset))
        else {
            return word as usize;
        };
        let how = if word < own { Read::Own } else { Read::Guarded };
        let Some(value) = read(address, how) else {
            return word as usize;
        };
        if !keep(value) {
            return word as usize + 1;
        }
    }
    scanned
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    /// A stack ending at `top`, read as the host
    /// would: an unguarded read past `top` is the fault the scan must avoid.
    fn reader(top: u64, faults: &mut Vec<u64>) -> impl FnMut(u64, Read) -> Option<u64> + '_ {
        move |address, how| {
            if address < top {
                return Some(address);
            }
            match how {
                Read::Own => {
                    faults.push(address);
                    Some(0)
                }
                Read::Guarded => None,
            }
        }
    }

    #[test]
    fn a_stack_pointer_near_its_top_reads_nothing_past_the_end() {
        // A fresh job's stack: the hook ran 0x200 bytes below the top of a
        // stack whose guard page follows; a scan of 512 words used to read
        // the guard page's first word.
        let top = 0xffff_8010_0d34_3000;
        let stack = top - 0x200;
        let mut faults = Vec::new();
        let mut kept = Vec::new();
        let read = scan(stack, 512, reader(top, &mut faults), |word| {
            kept.push(word);
            true
        });
        assert!(faults.is_empty(), "an unguarded read past the top");
        assert_eq!(read, 0x200 / 8);
        assert_eq!(kept.last(), Some(&(top - 8)));
    }

    #[test]
    fn words_past_the_own_page_are_guarded_and_go_on_while_mapped() {
        let stack = 0x10_0ff0;
        let mut ways = Vec::new();
        let read = scan(
            stack,
            6,
            |address, how| {
                ways.push(how);
                Some(address)
            },
            |_| true,
        );
        assert_eq!(read, 6);
        assert_eq!(
            ways,
            [
                Read::Own,
                Read::Own,
                Read::Guarded,
                Read::Guarded,
                Read::Guarded,
                Read::Guarded
            ]
        );
    }

    #[test]
    fn the_keeper_ends_the_scan() {
        let mut seen = 0;
        let read = scan(
            0x2000,
            512,
            |address, _| Some(address),
            |_| {
                seen += 1;
                seen < 3
            },
        );
        assert_eq!(read, 3);
    }

    #[test]
    fn own_words_count_to_the_page_end() {
        assert_eq!(own_words(0x1000), 512);
        assert_eq!(own_words(0x1ff8), 1);
        assert_eq!(own_words(0x1e00), 64);
    }
}
