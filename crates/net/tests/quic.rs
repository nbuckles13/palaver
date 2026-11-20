//! The QUIC transport on the loopback interface: real sockets, real TLS, real time.
//! Nothing here sleeps; each test waits only for things that happen in milliseconds.

#![allow(clippy::unwrap_used)]

use bytes::Bytes;
use palaver_net::admission::{AdmissionKey, Admitted};
use palaver_net::identity::Identity;
use palaver_net::quic::{QuicConnection, QuicEndpoint, QuicOptions};
use palaver_net::transport::{
    ClosedBy, Connection, DatagramError, DialError, Endpoint, Peer, RecvStream, SendStream, Side,
    StreamError,
};
use palaver_proto::{PathKind, Redacted, Secret, Signer};
use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

fn options() -> QuicOptions {
    QuicOptions {
        bind: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
        dial_timeout: Duration::from_millis(250),
        ..QuicOptions::default()
    }
}

struct Node {
    identity: Identity,
    endpoint: QuicEndpoint,
}

impl Node {
    fn new() -> Self {
        Self::with(options())
    }

    fn with(options: QuicOptions) -> Self {
        let identity = Identity::generate().unwrap();
        let endpoint = QuicEndpoint::bind(&identity, options).unwrap();
        Self { identity, endpoint }
    }

    /// Where to find this node.
    fn address(&self) -> Peer {
        Peer {
            key: self.identity.public_key(),
            addresses: vec![Redacted::new(self.endpoint.local_address().unwrap())],
            relay_url: None,
        }
    }
}

/// Two nodes and a connection between them: the dialler's end, then the accepter's.
async fn connected() -> (Node, Node, QuicConnection, QuicConnection) {
    let (a, b) = (Node::new(), Node::new());
    let ours = a.endpoint.dial(&b.address()).await.unwrap();
    let theirs = b.endpoint.accept().await.unwrap();
    (a, b, ours, theirs)
}

fn bytes(text: &'static str) -> Bytes {
    Bytes::from_static(text.as_bytes())
}

/// Everything a stream delivers until its clean end.
async fn read_all(recv: &mut impl RecvStream) -> Vec<u8> {
    let mut all = Vec::new();
    while let Some(chunk) = recv.read().await.unwrap() {
        all.extend_from_slice(&chunk);
    }
    all
}

#[tokio::test]
async fn each_end_learns_the_others_key_from_the_handshake() {
    let (a, b, ours, theirs) = connected().await;
    assert_eq!(ours.peer(), b.identity.public_key());
    assert_eq!(theirs.peer(), a.identity.public_key());
    assert_eq!(
        (ours.side(), theirs.side()),
        (Side::Dialled, Side::Accepted)
    );
    assert_eq!(ours.path().kind, PathKind::Direct);
    assert_eq!(a.endpoint.key(), a.identity.public_key());
}

#[tokio::test]
async fn an_address_answered_by_another_key_gives_no_connection() {
    let (a, b) = (Node::new(), Node::new());
    // The right address, the wrong key: as if someone else had taken the address over.
    let mut wrong = b.address();
    wrong.key = Identity::generate().unwrap().public_key();
    assert_eq!(
        a.endpoint.dial(&wrong).await.err(),
        Some(DialError::Refused)
    );
    // The one whose address it is still can be reached, by its own key.
    assert!(a.endpoint.dial(&b.address()).await.is_ok());
}

#[tokio::test]
async fn an_address_nobody_answers_is_unreachable_and_so_is_no_address_at_all() {
    let a = Node::new();
    let gone = {
        let b = Node::new();
        b.address()
    };
    assert_eq!(
        a.endpoint.dial(&gone).await.err(),
        Some(DialError::Unreachable)
    );
    let nowhere = Peer::by_key(Identity::generate().unwrap().public_key());
    assert_eq!(
        a.endpoint.dial(&nowhere).await.err(),
        Some(DialError::Unreachable)
    );
    let itself = a.address();
    assert_eq!(
        a.endpoint.dial(&itself).await.err(),
        Some(DialError::Unreachable)
    );
}

#[tokio::test]
async fn of_several_addresses_the_one_that_answers_is_used() {
    let (a, b) = (Node::new(), Node::new());
    let dead = {
        let gone = Node::new();
        gone.endpoint.local_address().unwrap()
    };
    let mut peer = b.address();
    peer.addresses.insert(0, Redacted::new(dead));
    let ours = a.endpoint.dial(&peer).await.unwrap();
    assert_eq!(ours.peer(), b.identity.public_key());
}

#[tokio::test]
async fn a_peer_that_speaks_another_protocol_is_refused() {
    let a = Node::new();
    let other = Node::with(QuicOptions {
        alpn: b"something-else/1".to_vec(),
        ..options()
    });
    assert_eq!(
        a.endpoint.dial(&other.address()).await.err(),
        Some(DialError::Refused)
    );
}

#[tokio::test]
async fn a_one_way_stream_delivers_in_order_and_then_ends() {
    let (_a, _b, ours, theirs) = connected().await;
    let mut send = ours.open_uni().await.unwrap();
    send.write(bytes("one ")).await.unwrap();
    send.write(bytes("two")).await.unwrap();
    send.finish();
    let mut recv = theirs.accept_uni().await.unwrap();
    assert_eq!(read_all(&mut recv).await, b"one two");
    assert_eq!(recv.read().await.unwrap(), None);
    assert_eq!(send.write(bytes("x")).await, Err(StreamError::Finished));
}

#[tokio::test]
async fn a_two_way_stream_carries_a_request_and_its_answer() {
    let (_a, _b, ours, theirs) = connected().await;
    let (mut ask, mut answer) = theirs.open_bidi().await.unwrap();
    ask.write(bytes("tracks?")).await.unwrap();
    let (mut reply, mut question) = ours.accept_bidi().await.unwrap();
    assert_eq!(question.read().await.unwrap(), Some(bytes("tracks?")));
    reply.write(bytes("ok")).await.unwrap();
    reply.finish();
    assert_eq!(read_all(&mut answer).await, b"ok");
}

#[tokio::test]
async fn a_megabyte_crosses_a_stream_whole_with_a_reader_that_starts_late() {
    let (_a, _b, ours, theirs) = connected().await;
    let sent: Vec<u8> = (0..1_000_000u32).map(|n| (n % 251) as u8).collect();
    let writer = {
        let sent = sent.clone();
        tokio::spawn(async move {
            let mut send = ours.open_uni().await.unwrap();
            for chunk in sent.chunks(10_000) {
                send.write(Bytes::copy_from_slice(chunk)).await.unwrap();
            }
            send.finish();
            // Keep the connection until the reader is done.
            ours.closed().await;
        })
    };
    let mut recv = theirs.accept_uni().await.unwrap();
    assert_eq!(read_all(&mut recv).await, sent);
    theirs.close(0, "done");
    writer.await.unwrap();
}

#[tokio::test]
async fn a_reset_reaches_the_reader_with_its_code_and_a_dropped_sender_is_a_reset() {
    let (_a, _b, ours, theirs) = connected().await;
    let mut send = ours.open_uni().await.unwrap();
    send.write(bytes("lost")).await.unwrap();
    send.reset(7);
    let mut recv = theirs.accept_uni().await.unwrap();
    // What was written before the reset may or may not arrive; the reset does.
    let ended = loop {
        match recv.read().await {
            Ok(Some(_)) => {}
            other => break other,
        }
    };
    assert_eq!(ended, Err(StreamError::Reset(7)));

    let mut send = ours.open_uni().await.unwrap();
    send.write(bytes("cut off")).await.unwrap();
    drop(send);
    let mut recv = theirs.accept_uni().await.unwrap();
    let ended = loop {
        match recv.read().await {
            Ok(Some(_)) => {}
            other => break other,
        }
    };
    assert_eq!(ended, Err(StreamError::Reset(0)));
}

#[tokio::test]
async fn a_stop_reaches_the_writer_with_its_code() {
    let (_a, _b, ours, theirs) = connected().await;
    let mut send = ours.open_uni().await.unwrap();
    send.write(bytes("first")).await.unwrap();
    let mut recv = theirs.accept_uni().await.unwrap();
    recv.stop(9);
    assert_eq!(recv.read().await, Err(StreamError::Finished));
    // The stop takes a moment to cross; the writer meets it on a write soon after.
    let met = loop {
        match send.write(bytes("more")).await {
            Ok(()) => tokio::task::yield_now().await,
            Err(error) => break error,
        }
    };
    assert_eq!(met, StreamError::Stopped(9));
}

#[tokio::test]
async fn a_datagram_arrives_whole_and_one_too_large_is_refused() {
    let (_a, _b, ours, theirs) = connected().await;
    ours.send_datagram(bytes("frame")).unwrap();
    assert_eq!(theirs.read_datagram().await.unwrap(), bytes("frame"));
    theirs.send_datagram(bytes("back")).unwrap();
    assert_eq!(ours.read_datagram().await.unwrap(), bytes("back"));

    let size = ours.max_datagram_size();
    // Room for an audio frame and its header, with plenty to spare.
    assert!(size >= 1000, "{size}");
    ours.send_datagram(Bytes::from(vec![1; size])).unwrap();
    assert_eq!(theirs.read_datagram().await.unwrap().len(), size);
    // The limit grows as the path is found to take larger packets, so "one byte over" is
    // not a fixed size; larger than any UDP packet is.
    assert_eq!(
        ours.send_datagram(Bytes::from(vec![1; 100_000])),
        Err(DatagramError::TooLarge)
    );
}

#[tokio::test]
async fn a_close_is_seen_by_both_ends_and_ends_what_was_waiting() {
    let (_a, _b, ours, theirs) = connected().await;
    let waiting = {
        let theirs = theirs.clone();
        tokio::spawn(async move { theirs.accept_uni().await.err() })
    };
    ours.close(3, "not in the plan");
    let seen = theirs.closed().await;
    assert_eq!(
        (seen.by, seen.code, seen.reason.as_str()),
        (ClosedBy::Peer, 3, "not in the plan")
    );
    let own = ours.closed().await;
    assert_eq!(
        (own.by, own.code, own.reason.as_str()),
        (ClosedBy::Us, 3, "not in the plan")
    );
    assert_eq!(waiting.await.unwrap().map(|c| c.code), Some(3));
    // The first close stands, and nothing more can be done.
    theirs.close(4, "too late");
    assert_eq!(theirs.closed().await.code, 3);
    assert!(ours.open_uni().await.is_err());
    assert!(matches!(
        ours.send_datagram(bytes("x")),
        Err(DatagramError::Closed(_))
    ));
}

#[tokio::test]
async fn closing_an_endpoint_closes_its_connections_and_ends_accept() {
    let (a, b, ours, _theirs) = connected().await;
    b.endpoint.close();
    assert_eq!(ours.closed().await.by, ClosedBy::Peer);
    assert!(b.endpoint.accept().await.is_none());
    // It answers no more dials. (Whether that shows as refused or as unreachable depends
    // on whether its socket is still there to say no.)
    assert!(a.endpoint.dial(&b.address()).await.is_err());
}

#[tokio::test]
async fn admission_runs_over_it_and_keeps_out_a_node_with_another_secret() {
    let key = |secret: u8| AdmissionKey::from_meeting_secret(&Secret::new([secret; 32])).unwrap();
    let (a, b, c) = (Node::new(), Node::new(), Node::new());
    let (to_b, to_a) = (b.address(), a.address());
    let a = Admitted::new(a.endpoint, key(7));
    let b = Admitted::new(b.endpoint, key(7));
    let c = Admitted::new(c.endpoint, key(8));

    let ours = a.dial(&to_b).await.unwrap();
    let theirs = b.accept().await.unwrap();
    assert_eq!(ours.peer(), b.key());
    assert_eq!(theirs.peer(), a.key());

    assert_eq!(c.dial(&to_a).await.err(), Some(DialError::Refused));
}
