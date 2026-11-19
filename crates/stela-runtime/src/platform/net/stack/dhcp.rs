//! What the DHCPv4 exchange did, for the log: the DISCOVERs and REQUESTs
//! sent, the replies seen by type and destination, and for each reply the
//! client does not take, why, by smoltcp's own rules
//! (`socket::dhcpv4::Socket::process` and `parse_ack`). smoltcp says why
//! only in debug output, which no build prints, so a network that answered
//! is told apart from one that did not and from an adapter that dropped or
//! damaged the answers. [`GuestDhcp`] does the same for a bridged guest's
//! own DHCP client, by the window it went unanswered.
use alloc::{format, string::String};
use core::net::Ipv4Addr;

const ETHERTYPE_IPV4: [u8; 2] = [0x08, 0x00];
const ETHERTYPE_VLAN: [u8; 2] = [0x81, 0x00];
const UDP: u8 = 17;
const SERVER_PORT: u16 = 67;
const CLIENT_PORT: u16 = 68;
const MAGIC: [u8; 4] = [0x63, 0x82, 0x53, 0x63];
/// The fixed BOOTP header, then the magic cookie (RFC 2131 section 2).
const OPTIONS: usize = 240;

/// DHCP message types (RFC 2132 section 9.6).
const DISCOVER: u8 = 1;
const OFFER: u8 = 2;
const REQUEST: u8 = 3;
const ACK: u8 = 5;
const NAK: u8 = 6;

/// A DHCP message in a frame, with what the client's checks need.
struct Message<'a> {
    /// The frame went to the broadcast address.
    broadcast: bool,
    /// The IPv4 header and UDP checksums hold.
    checksums: bool,
    bootp: &'a [u8],
    kind: Option<u8>,
}

impl Message<'_> {
    fn xid(&self) -> u32 {
        u32::from_be_bytes(self.bootp[4..8].try_into().unwrap())
    }
    fn chaddr(&self) -> &[u8] {
        &self.bootp[28..34]
    }
    fn yiaddr(&self) -> Ipv4Addr {
        address(&self.bootp[16..20])
    }
    fn option(&self, code: u8) -> Option<&[u8]> {
        let mut options = &self.bootp[OPTIONS..];
        while let [kind, rest @ ..] = options {
            match *kind {
                0 => options = rest,
                255 => return None,
                kind => {
                    let [length, rest @ ..] = rest else {
                        return None;
                    };
                    let value = rest.get(..usize::from(*length))?;
                    if kind == code {
                        return Some(value);
                    }
                    options = &rest[usize::from(*length)..];
                }
            }
        }
        None
    }
    fn option_address(&self, code: u8) -> Option<Ipv4Addr> {
        self.option(code)
            .filter(|value| value.len() >= 4)
            .map(|value| address(&value[..4]))
    }
}

fn address(bytes: &[u8]) -> Ipv4Addr {
    Ipv4Addr::new(bytes[0], bytes[1], bytes[2], bytes[3])
}

/// The one's complement sum of `bytes`, as 16-bit big-endian words.
fn sum(bytes: &[u8], mut total: u32) -> u32 {
    let (words, rest) = bytes.as_chunks::<2>();
    for &word in words {
        total += u32::from(u16::from_be_bytes(word));
    }
    if let [last] = rest {
        total += u32::from(*last) << 8;
    }
    total
}

fn folds_to_ones(mut total: u32) -> bool {
    while total > 0xffff {
        total = (total & 0xffff) + (total >> 16);
    }
    total == 0xffff
}

/// The DHCP message `frame` carries from `from` port to `to` port, if any.
fn message(frame: &[u8], from: u16, to: u16) -> Option<(Message<'_>, bool)> {
    let tagged = frame.get(12..14)? == ETHERTYPE_VLAN;
    let ip = if tagged { 18 } else { 14 };
    if frame.get(ip - 2..ip)? != ETHERTYPE_IPV4 {
        return None;
    }
    let header = frame.get(ip..)?;
    let length = usize::from(header.first()? & 0x0f) * 4;
    let total = usize::from(u16::from_be_bytes(header.get(2..4)?.try_into().ok()?));
    if header[0] >> 4 != 4 || length < 20 || total < length + 8 || header.len() < total {
        return None;
    }
    if header[9] != UDP || u16::from_be_bytes([header[6], header[7]]) & 0x3fff != 0 {
        return None;
    }
    let udp = &header[length..total];
    if u16::from_be_bytes([udp[0], udp[1]]) != from || u16::from_be_bytes([udp[2], udp[3]]) != to {
        return None;
    }
    let udp_length = usize::from(u16::from_be_bytes([udp[4], udp[5]]));
    if udp_length < 8 || udp_length > udp.len() {
        return None;
    }
    let udp = &udp[..udp_length];
    let header_sum = folds_to_ones(sum(&header[..length], 0));
    let pseudo = sum(&header[12..20], u32::from(UDP) + udp_length as u32);
    let udp_sum = udp[6..8] == [0, 0] || folds_to_ones(sum(udp, pseudo));
    let bootp = &udp[8..];
    if bootp.len() < OPTIONS || bootp[236..240] != MAGIC {
        return None;
    }
    let mut message = Message {
        broadcast: frame[..6] == [0xff; 6],
        checksums: header_sum && udp_sum,
        bootp,
        kind: None,
    };
    message.kind = message.option(53).and_then(|kind| kind.first().copied());
    Some((message, tagged))
}

/// Replies the client did not take, by smoltcp's reason.
#[derive(Default)]
struct Refused {
    checksum: u32,
    xid: u32,
    server_id: u32,
    your_ip: u32,
    mask: u32,
}

/// The last ACK's lease: its seconds, server identifier, mask and router.
struct Lease {
    seconds: Option<u32>,
    server: Option<Ipv4Addr>,
    mask: Option<Ipv4Addr>,
    router: Option<Ipv4Addr>,
}

pub(super) struct DhcpLog {
    mac: [u8; 6],
    /// The transaction of the last DISCOVER or REQUEST sent.
    xid: Option<u32>,
    discovers: u32,
    requests: u32,
    offers: u32,
    acks: u32,
    naks: u32,
    other: u32,
    broadcast: u32,
    unicast: u32,
    /// Replies to another client's request, which are not ours to take.
    others: u32,
    refused: Refused,
    /// Frames seen with an 802.1Q tag, which smoltcp drops whole.
    tagged: u32,
    /// Broadcast frames seen in all: none means the adapter drops them.
    broadcasts: u32,
    lease: Option<Lease>,
}

impl DhcpLog {
    pub(super) fn new(mac: [u8; 6]) -> Self {
        Self {
            mac,
            xid: None,
            discovers: 0,
            requests: 0,
            offers: 0,
            acks: 0,
            naks: 0,
            other: 0,
            broadcast: 0,
            unicast: 0,
            others: 0,
            refused: Refused::default(),
            tagged: 0,
            broadcasts: 0,
            lease: None,
        }
    }

    /// A frame the client sent.
    pub(super) fn transmitted(&mut self, frame: &[u8]) {
        let Some((message, _)) = message(frame, CLIENT_PORT, SERVER_PORT) else {
            return;
        };
        match message.kind {
            Some(DISCOVER) => self.discovers = self.discovers.saturating_add(1),
            Some(REQUEST) => self.requests = self.requests.saturating_add(1),
            _ => return,
        }
        self.xid = Some(message.xid());
    }

    /// A frame the adapter received.
    pub(super) fn received(&mut self, frame: &[u8]) {
        if frame.get(..6) == Some(&[0xff; 6]) {
            self.broadcasts = self.broadcasts.saturating_add(1);
        }
        if frame.get(12..14) == Some(&ETHERTYPE_VLAN) {
            self.tagged = self.tagged.saturating_add(1);
        }
        let Some((message, tagged)) = message(frame, SERVER_PORT, CLIENT_PORT) else {
            return;
        };
        if message.bootp[0] != 2 || tagged {
            return;
        }
        if message.chaddr() != self.mac {
            self.others = self.others.saturating_add(1);
            return;
        }
        let count = match message.kind {
            Some(OFFER) => &mut self.offers,
            Some(ACK) => &mut self.acks,
            Some(NAK) => &mut self.naks,
            _ => &mut self.other,
        };
        *count = count.saturating_add(1);
        let destination = match message.broadcast {
            true => &mut self.broadcast,
            false => &mut self.unicast,
        };
        *destination = destination.saturating_add(1);
        let reason = if !message.checksums {
            &mut self.refused.checksum
        } else if Some(message.xid()) != self.xid {
            &mut self.refused.xid
        } else if message.option(54).is_none() {
            &mut self.refused.server_id
        } else if matches!(message.kind, Some(OFFER | ACK)) && !unicast(message.yiaddr()) {
            &mut self.refused.your_ip
        } else if message.kind == Some(ACK) && message.option(1).is_none() {
            &mut self.refused.mask
        } else {
            if message.kind == Some(ACK) {
                self.lease = Some(Lease {
                    seconds: message
                        .option(51)
                        .filter(|value| value.len() >= 4)
                        .map(|value| u32::from_be_bytes(value[..4].try_into().unwrap())),
                    server: message.option_address(54),
                    mask: message.option_address(1),
                    router: message.option_address(3),
                });
            }
            return;
        };
        *reason = reason.saturating_add(1);
    }

    /// The exchange so far, in one line's words.
    pub(super) fn summary(&self) -> String {
        let r = &self.refused;
        format!(
            "sent discover={} request={}; replies offer={} ack={} nak={} other={} \
             (broadcast={} unicast={}) for_other_clients={}; not taken: checksum={} \
             xid={} no_server_id={} your_ip={} no_mask={}; broadcast_frames={} \
             vlan_tagged={}",
            self.discovers,
            self.requests,
            self.offers,
            self.acks,
            self.naks,
            self.other,
            self.broadcast,
            self.unicast,
            self.others,
            r.checksum,
            r.xid,
            r.server_id,
            r.your_ip,
            r.mask,
            self.broadcasts,
            self.tagged,
        )
    }

    /// The last ACK's lease time, server, mask and router.
    pub(super) fn lease(&self) -> String {
        let Some(lease) = &self.lease else {
            return String::from("lease=unseen");
        };
        let shown = |address: Option<Ipv4Addr>| match address {
            Some(address) => format!("{address}"),
            None => String::from("none"),
        };
        format!(
            "lease={} server={} mask={} router={}",
            lease
                .seconds
                .map_or(String::from("none"), |seconds| format!("{seconds}s")),
            shown(lease.server),
            shown(lease.mask),
            shown(lease.router),
        )
    }
}

/// How long a bridged guest's DHCP client goes without an offer or ACK
/// after its first DISCOVER or REQUEST before [`GuestDhcp`] reports the
/// window: past macOS's first retransmissions (RFC 2131 section 4.1).
pub const GUEST_WAIT_MS: u64 = 10_000;
/// Unanswered windows reported one by one; after these, one in
/// [`GUEST_REPORT_EVERY`].
const GUEST_REPORTS: u32 = 3;
const GUEST_REPORT_EVERY: u32 = 6;

/// One unanswered stretch of a bridged guest's DHCP exchange: what the
/// guest sent and what the host adapter received meanwhile.
#[derive(Default)]
struct Window {
    started: u64,
    /// The client hardware address of the guest's first message.
    client: [u8; 6],
    /// The transaction of the guest's last DISCOVER or REQUEST.
    xid: u32,
    discovers: u32,
    requests: u32,
    frames: u32,
    broadcasts: u32,
    tagged: u32,
    offers: u32,
    acks: u32,
    naks: u32,
    other: u32,
    broadcast: u32,
    unicast: u32,
    others: u32,
    checksum: u32,
    other_xid: u32,
}

/// What a bridged guest's own DHCP client sent through the host adapter
/// and what that adapter received while no offer or ACK of the guest's
/// transaction came back. A guest that gets no address then shows whether
/// the host adapter received nothing at all, received broadcasts but no
/// server reply, or received replies that were damaged or of another
/// transaction. Headers only, no address is kept or logged.
#[derive(Default)]
pub struct GuestDhcp {
    window: Option<Window>,
    reported: u32,
}

impl GuestDhcp {
    /// A frame the guest sent, at `now` milliseconds.
    pub fn outbound(&mut self, frame: &[u8], now: u64) {
        let Some((message, _)) = message(frame, CLIENT_PORT, SERVER_PORT) else {
            return;
        };
        if message.bootp[0] != 1 || !matches!(message.kind, Some(DISCOVER | REQUEST)) {
            return;
        }
        let window = self.window.get_or_insert_with(|| Window {
            started: now,
            client: message.chaddr().try_into().unwrap(),
            ..Window::default()
        });
        let count = match message.kind {
            Some(DISCOVER) => &mut window.discovers,
            _ => &mut window.requests,
        };
        *count = count.saturating_add(1);
        window.xid = message.xid();
    }

    /// A frame the host adapter received for the guest. An offer or ACK of
    /// the guest's transaction ends the window unreported.
    pub fn inbound(&mut self, frame: &[u8]) {
        let Some(window) = self.window.as_mut() else {
            return;
        };
        window.frames = window.frames.saturating_add(1);
        if frame.get(..6) == Some(&[0xff; 6]) {
            window.broadcasts = window.broadcasts.saturating_add(1);
        }
        if frame.get(12..14) == Some(&ETHERTYPE_VLAN) {
            window.tagged = window.tagged.saturating_add(1);
        }
        let Some((message, _)) = message(frame, SERVER_PORT, CLIENT_PORT) else {
            return;
        };
        if message.bootp[0] != 2 {
            return;
        }
        if message.chaddr() != window.client {
            window.others = window.others.saturating_add(1);
            return;
        }
        let count = match message.kind {
            Some(OFFER) => &mut window.offers,
            Some(ACK) => &mut window.acks,
            Some(NAK) => &mut window.naks,
            _ => &mut window.other,
        };
        *count = count.saturating_add(1);
        let destination = match message.broadcast {
            true => &mut window.broadcast,
            false => &mut window.unicast,
        };
        *destination = destination.saturating_add(1);
        if !message.checksums {
            window.checksum = window.checksum.saturating_add(1);
        } else if message.xid() != window.xid {
            window.other_xid = window.other_xid.saturating_add(1);
        } else if matches!(message.kind, Some(OFFER | ACK)) {
            self.window = None;
        }
    }

    /// The line for a window that went [`GUEST_WAIT_MS`] without an
    /// answer by `now`, with the host adapter's `link`; the window then
    /// ends, and the guest's next message opens another. The first
    /// [`GUEST_REPORTS`] windows are each reported, later ones one in
    /// [`GUEST_REPORT_EVERY`].
    pub fn overdue(&mut self, now: u64, link: Option<bool>) -> Option<String> {
        if self
            .window
            .as_ref()
            .is_none_or(|window| now.saturating_sub(window.started) < GUEST_WAIT_MS)
        {
            return None;
        }
        let w = self.window.take()?;
        self.reported = self.reported.saturating_add(1);
        if self.reported > GUEST_REPORTS
            && !(self.reported - GUEST_REPORTS).is_multiple_of(GUEST_REPORT_EVERY)
        {
            return None;
        }
        Some(format!(
            "no offer or ack in {} s (window {}): guest sent discover={} request={}; host \
             adapter received frames={} broadcast={} vlan_tagged={}, replies for the guest \
             offer={} ack={} nak={} other={} (broadcast={} unicast={}) bad_checksum={} \
             other_xid={}, for_other_clients={}; host_link={}",
            now.saturating_sub(w.started) / 1000,
            self.reported,
            w.discovers,
            w.requests,
            w.frames,
            w.broadcasts,
            w.tagged,
            w.offers,
            w.acks,
            w.naks,
            w.other,
            w.broadcast,
            w.unicast,
            w.checksum,
            w.other_xid,
            w.others,
            match link {
                Some(true) => "up",
                Some(false) => "down",
                None => "unknown",
            },
        ))
    }
}

/// smoltcp's `x_is_unicast` for IPv4.
fn unicast(address: Ipv4Addr) -> bool {
    !(address.is_broadcast() || address.is_multicast() || address.is_unspecified())
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use alloc::{vec, vec::Vec};

    const MAC: [u8; 6] = [2, 0, 0, 0, 0, 1];

    /// A DHCP frame from `from` to `to` ports, of `kind`, to `chaddr`, with
    /// `options` after the message type.
    pub(crate) fn frame(
        from: u16,
        to: u16,
        kind: u8,
        xid: u32,
        chaddr: [u8; 6],
        broadcast: bool,
        options: &[u8],
    ) -> Vec<u8> {
        let mut bootp = vec![0u8; OPTIONS];
        bootp[0] = if from == SERVER_PORT { 2 } else { 1 };
        bootp[1] = 1;
        bootp[2] = 6;
        bootp[4..8].copy_from_slice(&xid.to_be_bytes());
        bootp[16..20].copy_from_slice(&[10, 0, 2, 15]);
        bootp[28..34].copy_from_slice(&chaddr);
        bootp[236..240].copy_from_slice(&MAGIC);
        bootp.extend_from_slice(&[53, 1, kind]);
        bootp.extend_from_slice(options);
        bootp.push(255);
        let udp_length = 8 + bootp.len();
        let mut udp = Vec::new();
        udp.extend_from_slice(&from.to_be_bytes());
        udp.extend_from_slice(&to.to_be_bytes());
        udp.extend_from_slice(&(udp_length as u16).to_be_bytes());
        udp.extend_from_slice(&[0, 0]);
        udp.extend_from_slice(&bootp);
        let source = [10, 0, 2, 2];
        let destination = if broadcast { [255; 4] } else { [10, 0, 2, 15] };
        let mut ip = vec![0x45, 0, 0, 0, 0, 0, 0, 0, 64, UDP, 0, 0];
        ip[2..4].copy_from_slice(&((20 + udp_length) as u16).to_be_bytes());
        ip.extend_from_slice(&source);
        ip.extend_from_slice(&destination);
        let check = !fold(sum(&ip, 0));
        ip[10..12].copy_from_slice(&check.to_be_bytes());
        let pseudo = sum(&ip[12..20], u32::from(UDP) + udp_length as u32);
        let check = !fold(sum(&udp, pseudo));
        udp[6..8].copy_from_slice(&check.to_be_bytes());
        let mut frame = Vec::new();
        frame.extend_from_slice(&if broadcast { [0xff; 6] } else { chaddr });
        frame.extend_from_slice(&[2, 0, 0, 0, 0, 2]);
        frame.extend_from_slice(&ETHERTYPE_IPV4);
        frame.extend_from_slice(&ip);
        frame.extend_from_slice(&udp);
        frame
    }

    fn fold(mut total: u32) -> u16 {
        while total > 0xffff {
            total = (total & 0xffff) + (total >> 16);
        }
        total as u16
    }

    const SERVER_ID: [u8; 6] = [54, 4, 10, 0, 2, 2];

    #[test]
    fn replies_are_counted_by_type_and_destination_with_the_reason_one_is_not_taken() {
        let mut log = DhcpLog::new(MAC);
        log.transmitted(&frame(68, 67, DISCOVER, 7, MAC, true, &[]));
        // Taken: an OFFER of our transaction, broadcast.
        log.received(&frame(67, 68, OFFER, 7, MAC, true, &SERVER_ID));
        // Not taken: another transaction, no server identifier, a damaged
        // checksum; and one for another client, not ours to take.
        log.received(&frame(67, 68, OFFER, 8, MAC, false, &SERVER_ID));
        log.received(&frame(67, 68, OFFER, 7, MAC, false, &[]));
        let mut damaged = frame(67, 68, OFFER, 7, MAC, false, &SERVER_ID);
        let last = damaged.len() - 2;
        damaged[last] ^= 1;
        log.received(&damaged);
        log.received(&frame(
            67,
            68,
            OFFER,
            7,
            [2, 0, 0, 0, 0, 9],
            true,
            &SERVER_ID,
        ));
        log.transmitted(&frame(68, 67, REQUEST, 9, MAC, true, &[]));
        // An ACK without a mask is refused; one with it gives the lease.
        log.received(&frame(67, 68, ACK, 9, MAC, false, &SERVER_ID));
        let options = [
            &SERVER_ID[..],
            &[
                1, 4, 255, 255, 255, 0, 51, 4, 0, 0, 14, 16, 3, 4, 10, 0, 2, 1,
            ],
        ]
        .concat();
        log.received(&frame(67, 68, ACK, 9, MAC, false, &options));
        assert_eq!(
            log.summary(),
            "sent discover=1 request=1; replies offer=4 ack=2 nak=0 other=0 (broadcast=1 \
             unicast=5) for_other_clients=1; not taken: checksum=1 xid=1 no_server_id=1 \
             your_ip=0 no_mask=1; broadcast_frames=2 vlan_tagged=0"
        );
        assert_eq!(
            log.lease(),
            "lease=3600s server=10.0.2.2 mask=255.255.255.0 router=10.0.2.1"
        );
    }

    #[test]
    fn a_tagged_reply_is_counted_and_not_taken() {
        let mut log = DhcpLog::new(MAC);
        log.transmitted(&frame(68, 67, DISCOVER, 7, MAC, true, &[]));
        let untagged = frame(67, 68, OFFER, 7, MAC, true, &SERVER_ID);
        let mut tagged = untagged[..12].to_vec();
        tagged.extend_from_slice(&[0x81, 0, 0, 0]);
        tagged.extend_from_slice(&untagged[12..]);
        log.received(&tagged);
        assert!(log.summary().contains("offer=0"), "{}", log.summary());
        assert!(
            log.summary().ends_with("vlan_tagged=1"),
            "{}",
            log.summary()
        );
    }

    /// A guest whose DISCOVERs go unanswered gets one line per window with
    /// what came in meanwhile; an offer of its transaction ends the window
    /// unreported, and later windows are reported one in six.
    #[test]
    fn an_unanswered_guest_dhcp_window_says_what_the_host_adapter_received() {
        let mut guest = GuestDhcp::default();
        let other = [2, 0, 0, 0, 0, 9];
        guest.inbound(&frame(67, 68, OFFER, 7, MAC, true, &SERVER_ID));
        assert_eq!(guest.overdue(60_000, Some(true)), None);
        guest.outbound(&frame(68, 67, DISCOVER, 7, MAC, true, &[]), 1_000);
        guest.outbound(&frame(68, 67, DISCOVER, 7, MAC, true, &[]), 5_000);
        guest.inbound(&frame(67, 68, OFFER, 8, MAC, false, &SERVER_ID));
        guest.inbound(&frame(67, 68, OFFER, 3, other, true, &SERVER_ID));
        assert_eq!(guest.overdue(10_999, Some(true)), None);
        assert_eq!(
            guest.overdue(11_000, Some(true)).as_deref(),
            Some(
                "no offer or ack in 10 s (window 1): guest sent discover=2 request=0; host \
                 adapter received frames=2 broadcast=1 vlan_tagged=0, replies for the guest \
                 offer=1 ack=0 nak=0 other=0 (broadcast=0 unicast=1) bad_checksum=0 \
                 other_xid=1, for_other_clients=1; host_link=up"
            )
        );
        assert_eq!(guest.overdue(30_000, Some(true)), None);

        // Answered: no line.
        guest.outbound(&frame(68, 67, DISCOVER, 9, MAC, true, &[]), 40_000);
        guest.inbound(&frame(67, 68, OFFER, 9, MAC, true, &SERVER_ID));
        assert_eq!(guest.overdue(60_000, Some(true)), None);

        let mut lines = 0;
        for window in 0..14u64 {
            let at = 100_000 + window * 20_000;
            guest.outbound(&frame(68, 67, REQUEST, 9, MAC, true, &[]), at);
            if let Some(line) = guest.overdue(at + GUEST_WAIT_MS, None) {
                assert!(line.ends_with("host_link=unknown"), "{line}");
                assert!(line.contains("request=1;"), "{line}");
                lines += 1;
            }
        }
        // Windows 2 and 3, then 9 and 15.
        assert_eq!(lines, 4);
    }
}
