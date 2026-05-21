//! Phase 1 validation: does **str0m** drive bidirectional audio
//! between two `Rtc` instances in the same process, or does it hit
//! the same SRTP key-derivation symptom we see on webrtc-rs 0.17?
//!
//! This is a deliberately small smoke test — not a port of synbad-audio
//! to str0m. It exists so we can answer "is str0m the right runway?"
//! before committing days to the full rewrite. The pattern mirrors
//! str0m's own `tests/bidirectional.rs`, simplified to drop netem and
//! the `TestRtc` wrapper.
//!
//! If this passes, str0m is validated and we promote it from
//! dev-dependency to runtime dependency and rewrite session.rs against
//! its sans-I/O driver. If it fails with the same SRTP symptom, the
//! issue is more fundamental than the library choice and we regroup.

use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

use str0m::format::Codec;
use str0m::media::{Direction, MediaKind};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, Input, Output, Rtc, RtcError};

#[test]
fn str0m_bidirectional_audio_validates() -> Result<(), RtcError> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_test_writer()
        .try_init();

    let mut l = TestPeer::new((Ipv4Addr::new(1, 1, 1, 1), 1000).into());
    let mut r = TestPeer::new((Ipv4Addr::new(2, 2, 2, 2), 2000).into());

    // SDP exchange — L offers a sendrecv audio m-line, R accepts.
    let mid = {
        let mut change = l.rtc.sdp_api();
        let mid = change.add_media(MediaKind::Audio, Direction::SendRecv, None, None, None);
        let (offer, pending) = change.apply().unwrap();

        let answer = r.rtc.sdp_api().accept_offer(offer)?;
        l.rtc.sdp_api().accept_answer(pending, answer)?;
        mid
    };

    // Drive both sides until they reach the connected ICE/DTLS state.
    let connect_deadline = Instant::now() + Duration::from_secs(10);
    while !l.rtc.is_connected() || !r.rtc.is_connected() {
        progress(&mut l, &mut r)?;
        if Instant::now() > connect_deadline {
            panic!(
                "str0m peers did not reach Connected within 10s \
                 (l connected={}, r connected={})",
                l.rtc.is_connected(),
                r.rtc.is_connected(),
            );
        }
    }

    // Both peers now have an Opus payload-type entry on `mid`.
    let pt = l
        .rtc
        .codec_config()
        .iter()
        .find(|p| p.spec().codec == Codec::Opus)
        .expect("Opus codec configured")
        .pt();

    // Send 1 second worth of synthetic audio frames in each direction
    // and count what arrives. The threshold is intentionally modest —
    // if the bidirectional path were broken the count would be zero,
    // not 90.
    let send_deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < send_deadline {
        let l_now = Instant::now();
        let l_time = l_now.duration_since(l.start);
        l.rtc
            .writer(mid)
            .expect("writer L")
            .write(pt, l_now, l_time.into(), vec![1u8; 80])?;
        progress(&mut l, &mut r)?;

        let r_now = Instant::now();
        let r_time = r_now.duration_since(r.start);
        r.rtc
            .writer(mid)
            .expect("writer R")
            .write(pt, r_now, r_time.into(), vec![2u8; 80])?;
        progress(&mut l, &mut r)?;
    }

    let media_at_l = l.media_count();
    let media_at_r = r.media_count();
    println!("str0m loopback: L received {media_at_l} media, R received {media_at_r}");

    assert!(
        media_at_l > 50,
        "L should have received >50 MediaData events, got {media_at_l}"
    );
    assert!(
        media_at_r > 50,
        "R should have received >50 MediaData events, got {media_at_r}"
    );
    Ok(())
}

struct TestPeer {
    rtc: Rtc,
    addr: std::net::SocketAddr,
    start: Instant,
    /// Packets emitted by `rtc` and not yet handed to the peer.
    outbox: Vec<(Vec<u8>, std::net::SocketAddr)>,
    /// Events seen via `Output::Event`.
    events: Vec<Event>,
}

impl TestPeer {
    fn new(addr: std::net::SocketAddr) -> Self {
        let start = Instant::now();
        let mut rtc = Rtc::new(start);
        let candidate = Candidate::host(addr, Protocol::Udp).expect("host candidate");
        rtc.add_local_candidate(candidate);
        Self {
            rtc,
            addr,
            start,
            outbox: Vec::new(),
            events: Vec::new(),
        }
    }

    fn media_count(&self) -> usize {
        self.events
            .iter()
            .filter(|e| matches!(e, Event::MediaData(_)))
            .count()
    }
}

/// Pump one round of output between two peers. Drains each side's
/// `poll_output` until it returns `Timeout`, queues outbound packets,
/// then delivers everything that was queued via `handle_input`.
fn progress(l: &mut TestPeer, r: &mut TestPeer) -> Result<(), RtcError> {
    drain_outputs(l)?;
    drain_outputs(r)?;
    deliver(l, r)?;
    deliver(r, l)?;
    let now = Instant::now();
    l.rtc.handle_input(Input::Timeout(now))?;
    r.rtc.handle_input(Input::Timeout(now))?;
    Ok(())
}

fn drain_outputs(peer: &mut TestPeer) -> Result<(), RtcError> {
    loop {
        match peer.rtc.poll_output()? {
            Output::Transmit(t) => {
                peer.outbox.push((t.contents.into(), t.destination));
            }
            Output::Event(e) => {
                peer.events.push(e);
            }
            Output::Timeout(_) => return Ok(()),
        }
    }
}

/// Hand every queued packet from `from` to `to` via
/// `Input::Receive` — proxies the loopback UDP path without an actual
/// socket. The `destination` on the outbound transmit is the address
/// the peer told str0m to use; we ignore it and just deliver to `to`.
fn deliver(from: &mut TestPeer, to: &mut TestPeer) -> Result<(), RtcError> {
    let mut drained = std::mem::take(&mut from.outbox);
    for (contents, _dest) in drained.drain(..) {
        let now = Instant::now();
        let Ok(view) = contents.as_slice().try_into() else {
            continue;
        };
        to.rtc.handle_input(Input::Receive(
            now,
            Receive {
                proto: Protocol::Udp,
                source: from.addr,
                destination: to.addr,
                contents: view,
            },
        ))?;
    }
    Ok(())
}
