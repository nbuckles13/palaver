//! Asking a STUN server what address this node's packets come from (RFC 8489, the
//! Binding request and nothing more).
//!
//! Behind a router that rewrites addresses a node cannot see its own public address. A
//! STUN server answers with the address it saw the question come from. The question must
//! leave from the same socket the node's QUIC traffic uses, because the address the
//! router gives is per socket: so the question and its answer share that socket, and
//! [`StunSocket`] sits under the QUIC stack to take the answers out before it sees them.
//!
//! Two servers that see different ports for the same socket mean the router gives a port
//! per destination (a symmetric NAT). Other nodes then cannot be told where to send, and
//! a hole punch will not work.
//!
//! What a server learns: this node's public address, and that it asked. Nothing about a
//! meeting is sent.

use quinn::udp::{RecvMeta, Transmit};
use quinn::{AsyncUdpSocket, UdpPoller};
use std::collections::BTreeMap;
use std::future::poll_fn;
use std::io::{self, IoSliceMut};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll, ready};
use std::time::Duration;
use tokio::sync::oneshot;
use tokio::time::{Instant, timeout_at};

/// The fixed bytes 4 to 8 of every STUN message.
const MAGIC: [u8; 4] = [0x21, 0x12, 0xA4, 0x42];
const HEADER: usize = 20;
const BINDING_REQUEST: u16 = 0x0001;
const BINDING_SUCCESS: u16 = 0x0101;
const MAPPED_ADDRESS: u16 = 0x0001;
const XOR_MAPPED_ADDRESS: u16 = 0x0020;
const FAMILY_V4: u8 = 1;
const FAMILY_V6: u8 = 2;
/// How long the first question waits for its answer before it is asked again. Each
/// further wait is twice the last.
const FIRST_WAIT: Duration = Duration::from_millis(250);

/// What ties an answer to its question.
pub type TransactionId = [u8; 12];

/// Whether `packet` is a STUN message and not a QUIC packet. A STUN message starts with
/// two zero bits and carries the magic bytes; a QUIC packet has one of those two bits set.
pub fn is_stun(packet: &[u8]) -> bool {
    packet.len() >= HEADER
        && packet.first().is_some_and(|first| first & 0xC0 == 0)
        && packet.get(4..8) == Some(&MAGIC)
}

fn header(kind: u16, length: u16, id: &TransactionId) -> Vec<u8> {
    let mut message = Vec::with_capacity(HEADER + usize::from(length));
    message.extend_from_slice(&kind.to_be_bytes());
    message.extend_from_slice(&length.to_be_bytes());
    message.extend_from_slice(&MAGIC);
    message.extend_from_slice(id);
    message
}

/// The question: "what address does this come from?"
pub fn binding_request(id: &TransactionId) -> Vec<u8> {
    header(BINDING_REQUEST, 0, id)
}

/// The mask an address is combined with on the wire: the magic bytes, then the id.
fn mask(id: &TransactionId) -> [u8; 16] {
    let mut mask = [0u8; 16];
    for (to, from) in mask.iter_mut().zip(MAGIC.iter().chain(id)) {
        *to = *from;
    }
    mask
}

/// The answer a server gives: `address` is where the question came from.
pub fn binding_response(id: &TransactionId, address: SocketAddr) -> Vec<u8> {
    let mask = mask(id);
    let (family, ip): (u8, Vec<u8>) = match address.ip() {
        IpAddr::V4(ip) => (FAMILY_V4, ip.octets().to_vec()),
        IpAddr::V6(ip) => (FAMILY_V6, ip.octets().to_vec()),
    };
    let value_len = u16::try_from(4 + ip.len()).unwrap_or(u16::MAX);
    let mut message = header(BINDING_SUCCESS, 4 + value_len, id);
    message.extend_from_slice(&XOR_MAPPED_ADDRESS.to_be_bytes());
    message.extend_from_slice(&value_len.to_be_bytes());
    message.extend_from_slice(&[0, family]);
    let port = address.port() ^ u16::from_be_bytes([MAGIC[0], MAGIC[1]]);
    message.extend_from_slice(&port.to_be_bytes());
    message.extend(ip.iter().zip(mask).map(|(byte, mask)| byte ^ mask));
    message
}

/// The address in an attribute's value, each byte combined with `mask` (all zero for the
/// older attribute that is sent as it is).
fn address_in(value: &[u8], mask: &[u8; 16]) -> Option<SocketAddr> {
    let family = *value.get(1)?;
    let port = u16::from_be_bytes([*value.get(2)?, *value.get(3)?])
        ^ u16::from_be_bytes([mask[0], mask[1]]);
    let plain: Vec<u8> = value
        .get(4..)?
        .iter()
        .zip(mask)
        .map(|(byte, mask)| byte ^ mask)
        .collect();
    let ip = match family {
        FAMILY_V4 => IpAddr::V4(Ipv4Addr::from(<[u8; 4]>::try_from(plain.get(..4)?).ok()?)),
        FAMILY_V6 => IpAddr::V6(Ipv6Addr::from(<[u8; 16]>::try_from(plain.get(..16)?).ok()?)),
        _ => return None,
    };
    Some(SocketAddr::new(ip, port))
}

/// The id and the address in a server's answer. `None` for anything else, well formed
/// or not.
pub fn parse_binding_response(packet: &[u8]) -> Option<(TransactionId, SocketAddr)> {
    if !is_stun(packet) {
        return None;
    }
    let kind = u16::from_be_bytes([*packet.first()?, *packet.get(1)?]);
    let length = usize::from(u16::from_be_bytes([*packet.get(2)?, *packet.get(3)?]));
    if kind != BINDING_SUCCESS {
        return None;
    }
    let id: TransactionId = packet.get(8..HEADER)?.try_into().ok()?;
    let mut rest = packet.get(HEADER..HEADER.checked_add(length)?)?;
    let mut plain = None;
    while rest.len() >= 4 {
        let kind = u16::from_be_bytes([*rest.first()?, *rest.get(1)?]);
        let length = usize::from(u16::from_be_bytes([*rest.get(2)?, *rest.get(3)?]));
        let value = rest.get(4..4usize.checked_add(length)?)?;
        match kind {
            // The masked form is preferred: some routers rewrite addresses they
            // recognise inside packets, and the mask hides it from them.
            XOR_MAPPED_ADDRESS => return Some((id, address_in(value, &mask(&id))?)),
            MAPPED_ADDRESS => plain = address_in(value, &[0; 16]),
            _ => {}
        }
        // Values are padded to a multiple of four bytes.
        let padded = length.checked_add(3)? & !3;
        rest = rest.get(4usize.checked_add(padded)?..).unwrap_or_default();
    }
    Some((id, plain?))
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The QUIC stack's socket, with STUN answers taken out of what it receives.
#[derive(Debug)]
pub(crate) struct StunSocket {
    inner: Arc<dyn AsyncUdpSocket>,
    /// Questions asked and not yet answered.
    waiting: Mutex<BTreeMap<TransactionId, oneshot::Sender<SocketAddr>>>,
}

impl StunSocket {
    pub(crate) fn new(inner: Arc<dyn AsyncUdpSocket>) -> Self {
        Self {
            inner,
            waiting: Mutex::new(BTreeMap::new()),
        }
    }

    /// Ask `server` where this socket's packets come from. The question is repeated,
    /// at growing intervals, until an answer comes or `patience` runs out.
    pub(crate) async fn mapped_address(
        &self,
        server: SocketAddr,
        patience: Duration,
    ) -> Option<SocketAddr> {
        let mut id = TransactionId::default();
        getrandom::fill(&mut id).ok()?;
        let (answer, mut answered) = oneshot::channel();
        lock(&self.waiting).insert(id, answer);
        let request = binding_request(&id);
        let give_up = Instant::now() + patience;
        let mut wait = FIRST_WAIT;
        let found = loop {
            self.send(server, &request).await;
            let until = (Instant::now() + wait).min(give_up);
            match timeout_at(until, &mut answered).await {
                Ok(answer) => break answer.ok(),
                Err(_) if Instant::now() >= give_up => break None,
                Err(_) => wait = wait.saturating_mul(2),
            }
        };
        lock(&self.waiting).remove(&id);
        found
    }

    /// Send one packet, waiting if the socket is not ready to take it (as a socket just
    /// opened is not). An error is left to the caller's retry.
    async fn send(&self, destination: SocketAddr, contents: &[u8]) {
        let mut poller = Arc::clone(&self.inner).create_io_poller();
        loop {
            let sent = self.inner.try_send(&Transmit {
                destination,
                ecn: None,
                contents,
                segment_size: None,
                src_ip: None,
            });
            match sent {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    if poll_fn(|cx| poller.as_mut().poll_writable(cx))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                _ => return,
            }
        }
    }

    /// Hand a STUN answer to whoever asked. Anything else that looks like STUN is dropped:
    /// it is not QUIC either.
    fn take(&self, packet: &[u8]) {
        if let Some((id, address)) = parse_binding_response(packet)
            && let Some(asker) = lock(&self.waiting).remove(&id)
        {
            let _ = asker.send(address);
        }
    }
}

impl AsyncUdpSocket for StunSocket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        Arc::clone(&self.inner).create_io_poller()
    }

    fn try_send(&self, transmit: &Transmit) -> io::Result<()> {
        self.inner.try_send(transmit)
    }

    fn poll_recv(
        &self,
        cx: &mut Context,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        loop {
            let received = ready!(self.inner.poll_recv(cx, bufs, meta))?;
            let mut kept = received.min(bufs.len()).min(meta.len());
            let mut at = 0;
            while at < kept {
                let packet = meta
                    .get(at)
                    .and_then(|meta| bufs.get(at)?.get(..meta.len))
                    .unwrap_or_default();
                if is_stun(packet) {
                    self.take(packet);
                    // Move the last kept packet into its place.
                    kept = kept.saturating_sub(1);
                    bufs.swap(at, kept);
                    meta.swap(at, kept);
                } else {
                    at = at.saturating_add(1);
                }
            }
            // If every packet was STUN there is nothing for QUIC: look again, which also
            // registers the waker if nothing more is waiting.
            if kept > 0 || received == 0 {
                return Poll::Ready(Ok(kept));
            }
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }

    fn max_transmit_segments(&self) -> usize {
        self.inner.max_transmit_segments()
    }

    fn max_receive_segments(&self) -> usize {
        self.inner.max_receive_segments()
    }

    fn may_fragment(&self) -> bool {
        self.inner.may_fragment()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: TransactionId = [
        0xb7, 0xe7, 0xa7, 0x01, 0xbc, 0x34, 0xd6, 0x86, 0xfa, 0x87, 0xdf, 0xae,
    ];

    #[test]
    fn a_request_is_twenty_bytes_with_the_magic_and_the_id() {
        let request = binding_request(&ID);
        assert_eq!(request.len(), 20);
        assert_eq!(request[..8], [0, 1, 0, 0, 0x21, 0x12, 0xA4, 0x42]);
        assert_eq!(request[8..], ID);
        assert!(is_stun(&request));
    }

    #[test]
    fn the_masked_address_has_the_bytes_of_the_rfc_sample() {
        // RFC 5769, section 2.2: 192.0.2.1 port 32853 under this transaction id.
        let response = binding_response(&ID, "192.0.2.1:32853".parse().unwrap());
        assert_eq!(
            response[20..],
            [
                0x00, 0x20, 0x00, 0x08, 0x00, 0x01, 0xa1, 0x47, 0xe1, 0x12, 0xa6, 0x43
            ]
        );
        assert_eq!(
            parse_binding_response(&response),
            Some((ID, "192.0.2.1:32853".parse().unwrap()))
        );
    }

    #[test]
    fn an_ipv6_address_round_trips() {
        let address: SocketAddr = "[2001:db8:1234:5678:11:2233:4455:6677]:32853"
            .parse()
            .unwrap();
        let response = binding_response(&ID, address);
        // RFC 5769, section 2.3: the same address under the same id.
        assert_eq!(
            response[24..],
            [
                0x00, 0x02, 0xa1, 0x47, 0x01, 0x13, 0xa9, 0xfa, 0xa5, 0xd3, 0xf1, 0x79, 0xbc, 0x25,
                0xf4, 0xb5, 0xbe, 0xd2, 0xb9, 0xd9
            ]
        );
        assert_eq!(parse_binding_response(&response), Some((ID, address)));
    }

    #[test]
    fn the_older_unmasked_attribute_is_read_and_the_masked_one_preferred() {
        let mut response = header(BINDING_SUCCESS, 12 + 8, &ID);
        // An attribute this code does not know, three bytes long, so padded by one.
        response.extend_from_slice(&[0x80, 0x22, 0, 3, b'a', b'b', b'c', 0]);
        response.extend_from_slice(&[0, 1, 0, 8, 0, 1, 0x80, 0x55, 192, 0, 2, 1]);
        assert_eq!(
            parse_binding_response(&response),
            Some((ID, "192.0.2.1:32853".parse().unwrap()))
        );

        let mut both = header(BINDING_SUCCESS, 24, &ID);
        both.extend_from_slice(&[0, 1, 0, 8, 0, 1, 0, 9, 10, 0, 0, 1]);
        both.extend_from_slice(&binding_response(&ID, "192.0.2.1:32853".parse().unwrap())[20..]);
        assert_eq!(
            parse_binding_response(&both).map(|(_, address)| address),
            Some("192.0.2.1:32853".parse().unwrap())
        );
    }

    #[test]
    fn what_is_not_an_answer_is_refused() {
        let good = binding_response(&ID, "192.0.2.1:32853".parse().unwrap());
        assert!(parse_binding_response(&binding_request(&ID)).is_none());
        // Cut short anywhere.
        for len in 0..good.len() {
            assert!(parse_binding_response(&good[..len]).is_none(), "{len}");
        }
        // No address in it.
        assert!(parse_binding_response(&header(BINDING_SUCCESS, 0, &ID)).is_none());
        // A family that does not exist.
        let mut odd = good.clone();
        odd[25] = 7;
        assert!(parse_binding_response(&odd).is_none());
        // Wrong magic.
        let mut other = good;
        other[4] = 0;
        assert!(parse_binding_response(&other).is_none());
    }

    #[test]
    fn quic_packets_are_not_taken_for_stun() {
        // A long header has the top bit set; a short header has the next one set.
        let mut long = vec![0xC0; 30];
        long[4..8].copy_from_slice(&MAGIC);
        let mut short = vec![0x40; 30];
        short[4..8].copy_from_slice(&MAGIC);
        assert!(!is_stun(&long));
        assert!(!is_stun(&short));
        assert!(!is_stun(&[0; 19]));
    }
}
