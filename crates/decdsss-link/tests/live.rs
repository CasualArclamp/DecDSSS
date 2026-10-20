// made by CasualArclamp
//! Live voice the whole way, in simulated time: a talker speaks for 25 s from the
//! moment the transmitter keys up, frames are built as the speech makes them (a
//! filler when none is ready), modulated a block at a time, sent through a noisy
//! channel, received, and played out. It checks that all the speech arrives, that
//! the transmission ends, and that the playout does not run dry once it has begun.

use decdsss_chan::Channel;
use decdsss_codec2::Mode as Codec;
use decdsss_core::Profile;
use decdsss_link::Assembler;
use decdsss_link::live::{FILLER, LiveEncoder};
use decdsss_phy::framing::{Header, PayloadType};
use decdsss_phy::rx::{Event, MultiReceiver};
use decdsss_phy::tx::LiveTx;

const RATE: f64 = 48_000.0;

fn speech(seconds: f64) -> Vec<i16> {
    let n = (seconds * 8000.0) as usize;
    let mut x = vec![0.0f64; n];
    let mut phase = 0.0;
    for (i, s) in x.iter_mut().enumerate() {
        phase += (105.0 + 25.0 * (i as f64 / 5000.0).sin()) / 8000.0;
        if phase >= 1.0 {
            phase -= 1.0;
            *s = 1.0;
        }
    }
    for (fc, bw) in [(700.0, 90.0), (1220.0, 110.0), (2600.0, 160.0)] {
        let r = (-std::f64::consts::PI * bw / 8000.0).exp();
        let (a1, a2) = (
            -2.0 * r * (std::f64::consts::TAU * fc / 8000.0).cos(),
            r * r,
        );
        let (mut y1, mut y2) = (0.0, 0.0);
        for s in x.iter_mut() {
            let y = *s - a1 * y1 - a2 * y2;
            y2 = y1;
            y1 = y;
            *s = y;
        }
    }
    let peak = x.iter().fold(0.0f64, |m, v| m.max(v.abs()));
    x.iter().map(|v| (v / peak * 8000.0) as i16).collect()
}

#[test]
fn live_speech_streams_and_plays_without_running_dry() {
    live_case(Profile::W2400, Codec::C450, PayloadType::Voice450, 25.0);
}

/// Codec2 1200 and 1600 live, on the fast modes DS1 and DS2: the modem carries 1.15 to 1.27
/// times the codec's rate, so the same must hold, with a block that is a quarter as long.
#[test]
fn live_speech_streams_on_the_fast_modes() {
    live_case(Profile::W2400, Codec::C1200, PayloadType::Voice1200, 25.0);
    // 1600 on 3 kHz runs at 1.155 times the codec's rate, so the backlog of the
    // start-up (5.3 s) takes 5.3 / 0.155 = 34 s to work off: talk for longer than
    // that to see the fillers.
    live_case(Profile::W3000, Codec::C1600, PayloadType::Voice1600, 50.0);
}

fn live_case(profile: Profile, codec: Codec, ptype: PayloadType, talk_s: f64) {
    println!("live case: {profile} Codec2 {codec}");
    let mode = decdsss_link::voice::voice_mode(profile, codec).unwrap();
    let depth = 2;
    let header = Header {
        profile,
        mode,
        depth,
        ptype,
        call: "VK2ABC".into(),
        session: 7,
        frame0: 0,
        nblocks: 4095,
        total_frames: 0,
    };
    let mut tx = LiveTx::new(header, RATE, 0.2).unwrap();
    let (lead_s, block_s) = (tx.lead_seconds(), tx.block_seconds());
    let frame_bytes = tx.layout().frame_bytes();
    let mut enc = LiveEncoder::new(codec, frame_bytes).unwrap();
    let input = speech(talk_s);
    let frame_s = enc.frame_seconds();

    // The talker's frames: each is ready when its speech has been spoken.
    let mut ready: Vec<(f64, Vec<u8>)> = Vec::new();
    let mut spoken = 0.0;
    for piece in input.chunks(800) {
        spoken += 800.0 / 8000.0;
        for f in enc.push(piece) {
            ready.push((spoken, f));
        }
    }
    let spoken_end = spoken;
    let mut last = enc.finish();
    ready.extend(last.drain(..).map(|f| (spoken_end, f)));
    let speech_frames = ready.iter().filter(|(_, f)| f[0] < 0xFE).count();

    // The transmitter: lead first, then a block whenever one is due, built a second
    // ahead of its air time from the frames ready by then (fillers for the rest).
    let mut audio = tx.lead();
    let mut at = tx.lead_seconds();
    let mut next = 0;
    let mut sent_fillers = 0;
    let mut ends_sent = false;
    while !ends_sent {
        let build = at - 1.0;
        let mut frames = Vec::new();
        for _ in 0..depth {
            if next < ready.len() && ready[next].0 <= build {
                ends_sent |= ready[next].1[0] == 0xFE;
                frames.push(ready[next].1.clone());
                next += 1;
            } else {
                frames.push(enc.filler());
                sent_fillers += 1;
            }
        }
        audio.extend(tx.block(&frames));
        at += tx.block_seconds();
    }
    // The talker lets go: the postamble, then the filter's tail.
    audio.extend(tx.eot());
    audio.extend(tx.finish());
    let signal_end_s = audio.len() as f64 / RATE;
    assert!(next == ready.len(), "every frame went out");

    let channel = Channel {
        snr_db: Some(12.0),
        offset_hz: 5.0,
        clock_ppm: 8.0,
        seed: 17,
        ..Channel::clean(0.2, profile.bandwidth_hz())
    };
    let mut heard = channel.apply(&audio, RATE);
    // Silence after the talker lets go, as on the air: the receiver sees the
    // signal end and the live stream is closed by the end frame or the lost run.
    heard.extend(vec![0.0; (30.0 * RATE) as usize]);

    let mut rx = MultiReceiver::new(&[profile], RATE);
    let mut asm = Assembler::new();
    // Playout: when each chunk of speech arrived (seconds of signal) and how long it is.
    let mut arrivals: Vec<(f64, f64)> = Vec::new();
    let mut ended = false;
    let mut segment_closed = false;
    let mut eot_at = None;
    let mut fed = 0usize;
    for chunk in heard.chunks(4800) {
        fed += chunk.len();
        for tagged in rx.push(chunk) {
            match tagged.event {
                Event::Block { header, result, .. } => {
                    assert_eq!(header.total_frames, 0);
                    asm.push(&header, &result);
                    for c in asm.take_live() {
                        arrivals.push((fed as f64 / RATE, c.pcm.len() as f64 / 8000.0));
                        ended |= c.ended;
                    }
                }
                Event::SegmentEnd { .. } => segment_closed = true,
                Event::EndOfTransmission { .. } => eot_at = Some(fed as f64 / RATE),
                _ => {}
            }
        }
    }
    assert!(ended, "the end frame arrived");
    // The postamble closes the stream within moments of the signal's end, not after
    // the six blocks (18 s) with nothing in them that the receiver otherwise waits.
    let eot_at = eot_at.expect("the postamble was heard");
    assert!(
        eot_at - signal_end_s < 3.0,
        "heard {:.1} s after the signal ended",
        eot_at - signal_end_s
    );
    assert!(
        segment_closed,
        "the receiver closed the channel after the end"
    );
    let (pcm, done) = asm.live_speech("VK2ABC", 7).unwrap();
    assert!(done);
    // All the speech, in time (the last frame padded with silence at most).
    let want = speech_frames as f64 * frame_s;
    assert!(
        (pcm.len() as f64 / 8000.0 - want).abs() < 1e-6,
        "{} s of speech against {want}",
        pcm.len() as f64 / 8000.0
    );
    assert!(want >= talk_s && want < talk_s + frame_s);

    // The playout starts a second after the first speech arrives and runs at the
    // speech's own rate: every chunk must be in before it is wanted.
    let start = arrivals[0].0 + 1.0;
    let mut wanted = 0.0;
    let mut worst = f64::MAX;
    for (arrived, len) in &arrivals {
        worst = worst.min(start + wanted - arrived);
        wanted += len;
    }
    println!(
        "first speech heard at {:.1} s of signal (lead {:.1} s), playout slack at worst {:.2} s,          {} fillers, {} speech frames",
        start, lead_s, worst, sent_fillers, speech_frames
    );
    assert!(worst > 0.0, "the playout ran dry by {:.2} s", -worst);
    // The speech reaches the listener about the lead time after it was spoken: the
    // first sound is heard at lead + a block + the playout delay.
    let first_heard = start;
    assert!(
        first_heard < lead_s + 2.0 * block_s + 2.0,
        "first speech after {first_heard:.1} s"
    );
    // Fillers were sent once the backlog was gone.
    assert!(
        sent_fillers > 0,
        "the transmitter never had to send a filler"
    );
    assert_eq!(FILLER, 0xFF);
}
