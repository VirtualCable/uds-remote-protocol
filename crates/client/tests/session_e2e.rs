//! Headless end-to-end test of the client session API (§15.3): an
//! in-process mini server (real handshake, UDP leg, encrypted channels)
//! drives a [`Session`], and the test consumes the event channel, the
//! shared framebuffers and the command channel exactly like a GUI consumer
//! would — without any GUI.

use std::time::{Duration, Instant};

use anyhow::Result;
use client::mic::MicMode;
use client::{ClientCommand, ClientConfig, ClientEvent, Session};
use codec::{RawEncoder, VideoEncoder, VideoFrame};
use protocol::fragment::Fragmenter;
use protocol::{ControlMessage, CursorMessage, InputMessage, Probe, ProbeAck, channel};
use transport::{DirectListener, EncryptedDatagram, EncryptedStream, UdpTransport, udp_offer};

const TICKET: [u8; protocol::TICKET_LENGTH] = [0x42; protocol::TICKET_LENGTH];
const KEY: [u8; 32] = [0x07; 32];

const WIDTH: u32 = 64;
const HEIGHT: u32 = 48;

/// Deterministic test pattern (one BGRA gradient frame).
fn pattern() -> Vec<u8> {
    let mut pixels = vec![0u8; (WIDTH * HEIGHT * 4) as usize];
    for (i, px) in pixels.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        // Deterministic fill; wrapping is intended (comparison only).
        #[allow(clippy::cast_possible_truncation)]
        {
            px[0] = i as u8;
        }
        px[1] = 0xFF;
    }
    pixels
}

/// Raw-encoded fragments of the test pattern.
fn pattern_fragments(frame_id: u32, capture_ts_ms: u32) -> Result<Vec<Vec<u8>>> {
    let pixels = pattern();
    let packet = RawEncoder::new().encode(&VideoFrame {
        width: WIDTH,
        height: HEIGHT,
        stride: (WIDTH * 4) as usize,
        data: &pixels,
    })?;
    Ok(Fragmenter::split(0, frame_id, capture_ts_ms, &packet.data))
}

/// Replies to one decoded control datagram when it is a probe (§12.1).
async fn answer_probe(
    dgram: &mut EncryptedDatagram<UdpTransport>,
    plain: &[u8],
    clock_start: Instant,
) -> Result<()> {
    if let Ok(probe) = Probe::decode(plain) {
        let micros = || u64::try_from(clock_start.elapsed().as_micros()).unwrap_or(u64::MAX);
        let ack = ProbeAck {
            t1: probe.t1,
            t2: micros(),
            t3: micros(),
        };
        dgram.send(channel::CONTROL, &ack.encode()).await?;
    }
    Ok(())
}

/// Mini server session over a real UDP leg: handshake, detection probes,
/// `DisplayInfo`, three display frames, cursor messages, then probe
/// answering and input watching until the client goes away.
async fn serve_udp(listener: DirectListener, got_input: flume::Sender<()>) -> Result<()> {
    let (raw, _peer) = listener.accept().await?;
    let mut stream = EncryptedStream::server_handshake(raw, &KEY, None).await?;
    let (pending, token) = udp_offer(&mut stream, "127.0.0.1:0", &TICKET, &KEY).await?;
    let clock_start = Instant::now();
    let (udp, first) = tokio::time::timeout(Duration::from_secs(3), pending.wait_first()).await??;
    let keys = crypt::derive_datagram_keys(&TICKET, &KEY)?;
    let mut dgram = EncryptedDatagram::new(udp, &keys.s2c, &keys.c2s, &token);

    // Answer the detection probe(s) that opened the leg (§4.2).
    let mut plain = vec![0u8; 2048];
    if let Some((channel::CONTROL, n)) = dgram.decrypt_received(&first, &mut plain)? {
        answer_probe(&mut dgram, &plain[..n], clock_start).await?;
    }

    // Announce the display, then send frames and cursor state (§8.4/§10).
    // DisplayInfo goes over the UDP leg like the real server's send_control;
    // cursor messages take the reliable stream.
    let info = ControlMessage::DisplayInfo {
        display_id: 0,
        width: WIDTH,
        height: HEIGHT,
        codec: codec::VideoCodecType::Raw.wire_id(),
    };
    dgram.send(channel::CONTROL, &info.encode()?).await?;
    let mut capture_ts = 1_000;
    for frame_id in 0..3u32 {
        for frag in pattern_fragments(frame_id, capture_ts)? {
            dgram.send(channel::DISPLAY, &frag).await?;
        }
        capture_ts += 33;
    }
    let shape = CursorMessage::Shape {
        width: 1,
        height: 1,
        hot_x: 0,
        hot_y: 0,
        pixels: vec![255, 0, 0, 255],
    };
    stream.send(channel::CURSOR, &shape.encode()).await?;
    stream
        .send(channel::CURSOR, &CursorMessage::Visibility { visible: true }.encode())
        .await?;

    // Serve probes and watch for input until the client disconnects.
    let mut sbuf = vec![0u8; 65_536];
    let mut dbuf = vec![0u8; 2048];
    loop {
        tokio::select! {
            res = stream.recv_into(&mut sbuf) => {
                let Ok((ch, _n)) = res else { return Ok(()) };
                if ch == channel::INPUT {
                    let _ = got_input.try_send(());
                }
            }
            res = dgram.recv_into(&mut dbuf) => {
                let Ok((ch, n)) = res else { continue };
                if ch == channel::CONTROL {
                    answer_probe(&mut dgram, &dbuf[..n], clock_start).await?;
                }
            }
        }
    }
}

/// Mini server session on the reliable stream only: the UDP leg is offered
/// but probes are never answered, so the client falls back to TCP (§4.1)
/// and display frames flow over the stream.
async fn serve_tcp_fallback(listener: DirectListener) -> Result<()> {
    let (raw, _peer) = listener.accept().await?;
    let mut stream = EncryptedStream::server_handshake(raw, &KEY, None).await?;
    // Offered (the client probes it) but intentionally never answered.
    let (_pending, _token) = udp_offer(&mut stream, "127.0.0.1:0", &TICKET, &KEY).await?;
    let info = ControlMessage::DisplayInfo {
        display_id: 0,
        width: WIDTH,
        height: HEIGHT,
        codec: codec::VideoCodecType::Raw.wire_id(),
    };
    stream.send(channel::CONTROL, &info.encode()?).await?;
    for frame_id in 0..2u32 {
        for frag in pattern_fragments(frame_id, 1_000 + frame_id * 33)? {
            stream.send(channel::DISPLAY, &frag).await?;
        }
    }
    // Hold the session open until the client hangs up.
    let mut buf = vec![0u8; 65_536];
    loop {
        if stream.recv_into(&mut buf).await.is_err() {
            return Ok(());
        }
    }
}

/// Collects events until `done` says enough, with an overall deadline.
macro_rules! collect {
    ($rx:expr, $done:expr) => {{
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(left > Duration::ZERO, "timed out waiting for events");
            let event = tokio::time::timeout(left, $rx.recv_async())
                .await
                .expect("event within deadline")
                .expect("event channel alive");
            if $done(event) {
                break;
            }
        }
    }};
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_delivers_frames_cursor_and_input_end_to_end() -> Result<()> {
    let listener = DirectListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let (input_tx, input_rx) = flume::bounded(8);
    let server = tokio::spawn(serve_udp(listener, input_tx));

    let (events_tx, events_rx) = flume::bounded(client::EVENT_CHANNEL_CAPACITY);
    let config = ClientConfig {
        addr,
        ticket: TICKET,
        key: KEY,
        audio: false,
        audio_dump: None,
        mic: MicMode::Off,
        privacy_screen: false,
        smartcard: false,
    };
    let (session, commands, displays) = Session::new(config, events_tx);
    let session_task = tokio::spawn(session.run());

    // Session events arrive in order: connected, announced, frames, cursor.
    let (mut connected, mut announced, mut dirty, mut shape, mut visible) =
        (false, false, 0u32, false, false);
    collect!(&events_rx, |event| {
        match event {
            ClientEvent::Connected { udp } => {
                assert!(udp, "loopback UDP detection must succeed");
                connected = true;
            }
            ClientEvent::DisplayInfo {
                display_id,
                width,
                height,
                codec,
            } => {
                assert_eq!((display_id, width, height), (0, WIDTH, HEIGHT));
                assert_eq!(codec, codec::VideoCodecType::Raw);
                announced = true;
            }
            ClientEvent::FrameDirty { display_id, rects } => {
                assert_eq!(display_id, 0);
                dirty += 1;
                // No dirty metadata on the wire yet (§8): full-frame rects.
                assert_eq!(
                    rects,
                    vec![client::Rect {
                        x: 0,
                        y: 0,
                        w: WIDTH,
                        h: HEIGHT
                    }]
                );
            }
            ClientEvent::CursorShape { .. } => shape = true,
            ClientEvent::CursorVisibility { visible: v } => visible = v,
            // Periodic telemetry; not part of what this test asserts.
            ClientEvent::Stats(_) => {}
            other => panic!("unexpected event: {other:?}"),
        }
        connected && announced && dirty >= 3 && shape && visible
    });

    // The shared framebuffer holds the last decoded frame (§15.3).
    let display = displays.get(0).expect("display 0 framebuffer");
    {
        let guard = display.read().expect("framebuffer lock");
        assert_eq!((guard.width(), guard.height()), (WIDTH, HEIGHT));
        assert_eq!(guard.pixels(), &pattern()[..], "pixel-perfect delivery");
    }

    // A command flows back to the server on the input channel.
    commands.send(ClientCommand::Input(InputMessage::PointerMove {
        display_id: 0,
        x: 10,
        y: 20,
    }))?;
    tokio::time::timeout(Duration::from_secs(2), input_rx.recv_async())
        .await
        .expect("input within 2 s")
        .expect("input channel alive");

    // Close ends the session: Disconnected is the last event and run()
    // returns cleanly.
    commands.send(ClientCommand::Close)?;
    collect!(&events_rx, |event| matches!(event, ClientEvent::Disconnected));
    session_task.await.expect("session task")?;
    server.abort();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_falls_back_to_tcp_when_udp_is_silent() -> Result<()> {
    let listener = DirectListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let server = tokio::spawn(serve_tcp_fallback(listener));

    let (events_tx, events_rx) = flume::bounded(client::EVENT_CHANNEL_CAPACITY);
    let config = ClientConfig {
        addr,
        ticket: TICKET,
        key: KEY,
        audio: false,
        audio_dump: None,
        mic: MicMode::Off,
        privacy_screen: false,
        smartcard: false,
    };
    let (session, commands, displays) = Session::new(config, events_tx);
    let session_task = tokio::spawn(session.run());

    let (mut connected, mut dirty) = (false, 0u32);
    collect!(&events_rx, |event| {
        match event {
            ClientEvent::Connected { udp } => {
                assert!(!udp, "no probe answers: TCP fallback expected");
                connected = true;
            }
            ClientEvent::FrameDirty { .. } => dirty += 1,
            ClientEvent::DisplayInfo { .. } => {}
            other => panic!("unexpected event: {other:?}"),
        }
        connected && dirty >= 2
    });
    drop(commands);
    drop(displays);
    collect!(&events_rx, |event| matches!(event, ClientEvent::Disconnected));
    session_task.await.expect("session task")?;
    server.abort();
    Ok(())
}
