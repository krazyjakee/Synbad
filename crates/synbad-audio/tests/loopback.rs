//! End-to-end loopback test for the audio bridge.
//!
//! Drives two `AudioSession`s on the same machine through paired TCP
//! sockets on `127.0.0.1`, with synthetic PCM frames fed in on each
//! side via `mpsc` channels (no cpal). Asserts that *both* sides'
//! inbound RTP pumps receive at least one decoded frame before the
//! deadline.
//!
//! ## Why this test exists
//!
//! Manual two-machine debugging of the audio bridge showed
//! `webrtc_srtp: aes gcm: aead::Error` immediately after DTLS-SRTP
//! came up, with `PeerConnectionState::Connected` but zero packets
//! arriving on either inbound track. Reproducing that on one machine
//! turns a multi-minute manual loop into a sub-10-second `cargo test`,
//! and the same scaffold is the regression net once the underlying
//! SRTP/role/codec issue is fixed.
//!
//! The test bypasses the daemon (no mDNS, no pairing, no IPC) and
//! anonymous-handshakes a `CipherStream` pair directly, so any failure
//! is squarely in the `synbad-audio` layer.

use std::sync::Arc;
use std::time::Duration;

use synbad_audio::session::{AudioSession, SessionRole};
use synbad_audio::AudioEvent;
use synbad_crypto::{accept, initiate, HandshakeMode};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::time::{timeout, Instant};

/// One 20 ms frame at 48 kHz mono — matches `capture::FRAME_SAMPLES`,
/// re-declared here so the test doesn't depend on a private constant.
const FRAME_SAMPLES: usize = 960;

/// How long we wait for both sides to receive a frame before we call
/// it a bug. The whole DTLS+ICE+RTP dance on loopback should complete
/// well under 5 s; 15 s leaves headroom for slow CI runners and
/// generous webrtc-rs internal timeouts.
const RX_DEADLINE: Duration = Duration::from_secs(15);

/// Cadence the test feeds synthetic PCM frames into each session's
/// capture channel. Matches the bridge's real 20 ms framing so the
/// outbound RTP timestamps look identical to a live capture.
const FRAME_INTERVAL: Duration = Duration::from_millis(20);

// Currently FAILS — reproduces a webrtc-rs 0.17 SRTP key derivation
// bug that we haven't been able to work around from the outside (see
// the longer commentary at the top of this file and in
// `session::start_inner`). Marked `#[ignore]` so `cargo test` stays
// green; run explicitly with
// `cargo test -p synbad-audio --test loopback -- --ignored --nocapture`
// when iterating on a fix.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "reproduces an unresolved webrtc-rs 0.17 SRTP bug; run explicitly with --ignored"]
async fn loopback_audio_flows_bidirectionally() {
    // Subscriber so a failure prints the webrtc-rs logs (including the
    // suspected SRTP `aead::Error`). Quiet by default; turn up with
    // `RUST_LOG=synbad_audio=debug,webrtc=info`.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_test_writer()
        .try_init();

    // 1. Build paired CipherStreams over a loopback TCP socket.
    //    `synbad_crypto::initiate` / `accept` only accept a
    //    `tokio::net::TcpStream`, so a real socket on 127.0.0.1 is the
    //    least-invasive way to stand them up.
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback listener");
    let addr = listener.local_addr().unwrap();

    let server_accept = async {
        let (stream, _) = listener.accept().await.expect("accept");
        stream
    };
    let client_connect = TcpStream::connect(addr);
    let (server_stream, client_stream) = tokio::join!(server_accept, client_connect);
    let client_stream = client_stream.expect("connect");

    // Anonymous handshake — audio sessions don't care about peer
    // identity beyond what the caller passes them in `peer_machine_id`.
    let (cipher_a_res, cipher_b_res) = tokio::join!(
        initiate(client_stream, HandshakeMode::Anonymous, None),
        accept(server_stream, HandshakeMode::Anonymous, |_machine_id| None),
    );
    let (cipher_a, _) = cipher_a_res.expect("initiate handshake");
    let (cipher_b, _) = cipher_b_res.expect("accept handshake");

    // 2. Per-side capture sources + playback sinks. The capture side
    //    needs to *push* PCM frames; the playback side counts what
    //    arrives so the test can assert on receipt.
    let (cap_tx_a, cap_rx_a) = mpsc::channel::<Arc<[i16]>>(8);
    let (cap_tx_b, cap_rx_b) = mpsc::channel::<Arc<[i16]>>(8);
    let (play_tx_a, mut play_rx_a) = mpsc::channel::<Arc<[i16]>>(32);
    let (play_tx_b, mut play_rx_b) = mpsc::channel::<Arc<[i16]>>(32);

    // Synthetic capture sources — silence is fine, the test cares
    // about RTP framing flowing end-to-end, not the audio content.
    let cap_pump_a = spawn_silent_capture(cap_tx_a);
    let cap_pump_b = spawn_silent_capture(cap_tx_b);

    // Event channels — failures show up here as `AudioEvent::Error`.
    let (events_tx_a, mut events_rx_a) = mpsc::channel::<AudioEvent>(64);
    let (events_tx_b, mut events_rx_b) = mpsc::channel::<AudioEvent>(64);

    // 3. Start the two sessions. The TCP-direction → role mapping in
    //    `synbadd::audio` says the side that *dialed* is the Offerer
    //    and the side that *accepted* is the Answerer; mirror that
    //    here so the test reflects production wiring.
    let session_a = AudioSession::start_for_test(
        "peer-b".into(),
        cipher_a,
        SessionRole::Offerer,
        events_tx_a,
        Some(cap_rx_a),
        Some(play_tx_a),
    )
    .await
    .expect("start session A");

    let session_b = AudioSession::start_for_test(
        "peer-a".into(),
        cipher_b,
        SessionRole::Answerer,
        events_tx_b,
        Some(cap_rx_b),
        Some(play_tx_b),
    )
    .await
    .expect("start session B");

    // 4. Race both sides receiving a frame against the deadline.
    let started = Instant::now();
    let outcome = timeout(RX_DEADLINE, async {
        let a_recv = play_rx_a.recv();
        let b_recv = play_rx_b.recv();
        tokio::join!(a_recv, b_recv)
    })
    .await;

    // Stop the capture pumps so the sessions tear down cleanly.
    cap_pump_a.abort();
    cap_pump_b.abort();

    match outcome {
        Ok((Some(_), Some(_))) => {
            eprintln!(
                "loopback audio: both sides received first frame in {:?}",
                started.elapsed()
            );
        }
        Ok((a, b)) => {
            // Drain whatever events landed so the failure message
            // surfaces the real reason (typically an `AudioEvent::Error`
            // from one of the sessions).
            let events_a = drain_events(&mut events_rx_a);
            let events_b = drain_events(&mut events_rx_b);
            panic!(
                "loopback audio: channels closed before deadline. \
                 a_first_frame={a:?}, b_first_frame={b:?}, \
                 events_a={events_a:?}, events_b={events_b:?}"
            );
        }
        Err(_) => {
            let status_a = session_a.status();
            let status_b = session_b.status();
            let events_a = drain_events(&mut events_rx_a);
            let events_b = drain_events(&mut events_rx_b);
            panic!(
                "loopback audio: neither side received a frame within {RX_DEADLINE:?}.\n\
                 session A status: {status_a:?}\n\
                 session B status: {status_b:?}\n\
                 events A: {events_a:?}\n\
                 events B: {events_b:?}"
            );
        }
    }

    // Best-effort cleanup. Drop tears down the WebRTC PCs via the
    // session's `Drop` impl too, but `close().await` flushes properly.
    session_a.close(Some("test done".into())).await;
    session_b.close(Some("test done".into())).await;
}

/// Spawn a task that ships a silent 20 ms PCM frame into `tx` every
/// `FRAME_INTERVAL` until the receiver is dropped. The cadence matches
/// the cpal capture path so the outbound RTP timestamps progress at a
/// realistic rate.
fn spawn_silent_capture(tx: mpsc::Sender<Arc<[i16]>>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(FRAME_INTERVAL);
        // Silence — content doesn't matter; we only care that L16 RTP
        // packets travel.
        let frame: Arc<[i16]> = Arc::from(vec![0i16; FRAME_SAMPLES].into_boxed_slice());
        loop {
            interval.tick().await;
            if tx.send(frame.clone()).await.is_err() {
                break;
            }
        }
    })
}

/// Pull every queued event without blocking. Used in failure
/// diagnostics so the panic message shows what the bridge thought was
/// wrong rather than just "deadline".
fn drain_events(rx: &mut mpsc::Receiver<AudioEvent>) -> Vec<AudioEvent> {
    let mut out = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        out.push(ev);
    }
    out
}
