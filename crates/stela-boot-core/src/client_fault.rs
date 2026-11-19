//! Where a host fault in the GPU clients' page window lies among the
//! clients' mappings, for its refault and for the line that names it.
//!
//! The window holds more than clients' mappings: the host's stacks between
//! guard pages and Linux's own kernel mappings share it. Only a page inside
//! a client's mapping is Linux's to refault, as Linux's fault handler serves
//! only an address inside a VMA. Any other page is no client's: an access
//! there is the host code's own error, which no refault can answer.
use core::fmt;

const PAGE: u64 = 4096;

/// A client's mapping of a file in the window.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Span {
    pub start: u64,
    pub pages: u64,
    /// The client's descriptor and the file offset it mapped.
    pub fd: i32,
    pub offset: u64,
}

impl Span {
    pub fn end(&self) -> u64 {
        self.start.saturating_add(self.pages.saturating_mul(PAGE))
    }
}

/// Where a fault lies.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Place {
    /// In a client's mapping, at byte `at` of it: Linux's to refault.
    Inside { span: Span, at: u64 },
    /// In no client's mapping, between the nearest ones.
    Outside {
        below: Option<Span>,
        above: Option<Span>,
    },
}

/// Where `address` lies, given the mapping that starts at or below it and
/// the one that starts above it.
pub fn place(below: Option<Span>, above: Option<Span>, address: u64) -> Place {
    match below {
        Some(span) if address >= span.start && address < span.end() => Place::Inside {
            span,
            at: address - span.start,
        },
        _ => Place::Outside { below, above },
    }
}

/// The line that names a fault Linux did not serve, after `GPU `.
pub struct Unserved {
    pub address: u64,
    pub write: bool,
    pub place: Place,
}

impl fmt::Display for Unserved {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let access = if self.write { "write" } else { "read" };
        match self.place {
            Place::Inside { span, at } => write!(
                f,
                "CLIENT MAPPING REFUSED a {access} at {:#x}: Linux did not back byte {at:#x} of \
                 fd {}'s mapping of {} KiB from file offset {:#x} at {:#x}",
                self.address,
                span.fd,
                (span.pages * PAGE) >> 10,
                span.offset,
                span.start,
            ),
            Place::Outside { below, above } => {
                write!(
                    f,
                    "FAULT OUTSIDE CLIENT MAPPINGS a {access} at {:#x} is in no client's \
                     mapping, so it is the host's own access to a page of the window that \
                     maps nothing",
                    self.address
                )?;
                if let Some(span) = below {
                    write!(
                        f,
                        "; below it fd {}'s mapping {:#x}..{:#x}",
                        span.fd,
                        span.start,
                        span.end()
                    )?;
                }
                if let Some(span) = above {
                    write!(
                        f,
                        "; above it fd {}'s mapping {:#x}..{:#x}",
                        span.fd,
                        span.start,
                        span.end()
                    )?;
                }
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;

    const BELOW: Span = Span {
        start: 0xffff_8010_0c00_0000,
        pages: 16,
        fd: 7,
        offset: 0x1_0000_0000,
    };
    const ABOVE: Span = Span {
        start: 0xffff_8010_0e00_0000,
        pages: 1,
        fd: 9,
        offset: 0,
    };

    #[test]
    fn a_page_inside_a_mapping_is_the_clients() {
        let address = BELOW.start + 3 * PAGE + 0x10;
        assert_eq!(
            place(Some(BELOW), Some(ABOVE), address),
            Place::Inside {
                span: BELOW,
                at: 3 * PAGE + 0x10
            }
        );
        assert_eq!(
            place(Some(BELOW), None, BELOW.end() - 1),
            Place::Inside {
                span: BELOW,
                at: 16 * PAGE - 1
            }
        );
    }

    #[test]
    fn a_page_past_every_mapping_is_no_clients() {
        // The boot that stopped: a read of the guard page above a host
        // stack, which lies in the window past a client's mapping, was sent
        // to Linux as a client's fault and stopped as one Linux refused.
        let address = 0xffff_8010_0d34_3000;
        assert_eq!(
            place(Some(BELOW), Some(ABOVE), address),
            Place::Outside {
                below: Some(BELOW),
                above: Some(ABOVE)
            }
        );
        assert_eq!(
            place(None, Some(ABOVE), address),
            Place::Outside {
                below: None,
                above: Some(ABOVE)
            }
        );
        assert_eq!(
            place(Some(BELOW), None, BELOW.end()),
            Place::Outside {
                below: Some(BELOW),
                above: None
            }
        );
    }

    #[test]
    fn the_lines_name_the_mapping_or_its_neighbours() {
        let refused = Unserved {
            address: BELOW.start + PAGE,
            write: true,
            place: place(Some(BELOW), None, BELOW.start + PAGE),
        }
        .to_string();
        assert_eq!(
            refused,
            "CLIENT MAPPING REFUSED a write at 0xffff80100c001000: Linux did not back byte \
             0x1000 of fd 7's mapping of 64 KiB from file offset 0x100000000 at \
             0xffff80100c000000"
        );
        let outside = Unserved {
            address: 0xffff_8010_0d34_3000,
            write: false,
            place: place(Some(BELOW), Some(ABOVE), 0xffff_8010_0d34_3000),
        }
        .to_string();
        assert_eq!(
            outside,
            "FAULT OUTSIDE CLIENT MAPPINGS a read at 0xffff80100d343000 is in no client's \
             mapping, so it is the host's own access to a page of the window that maps \
             nothing; below it fd 7's mapping 0xffff80100c000000..0xffff80100c010000; above \
             it fd 9's mapping 0xffff80100e000000..0xffff80100e001000"
        );
    }
}
