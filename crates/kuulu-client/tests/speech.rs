//! M3 acceptance (docs/PLAN.md): two headless voice clients talk through a
//! server over the home network profile, in real time, on synthetic speech.

use kuulu_audio::quality::{Quality, compare, speech};
use kuulu_audio::{FRAME_SAMPLES, SAMPLE_RATE};
use kuulu_client::voice::{Stats, Voice};
use kuulu_client::{Client, netem};
use kuulu_server::{Identity, Server};
use std::sync::Arc;
use std::time::{Duration, Instant};

const SECONDS: usize = 10;
/// Opus restricted low-delay look-ahead in samples.
const CODEC_DELAY: usize = 120;
/// A tick this late means the test runner stalled (shared CI machines do,
/// for 100 ms at times). Catching up in a burst would look like network
/// jitter, so the loop's clock moves on instead, as if the stall never was.
const STALL: Duration = Duration::from_millis(10);

#[derive(Debug)]
struct Run {
    stats: Stats,
    quality: Quality,
    /// Test runner stalls skipped over, and the longest.
    stalls: usize,
    max_stall: Duration,
    max_target: usize,
    /// Jitter buffer depth after each frame played.
    mean_depth: f64,
    p95_depth: usize,
}

#[tokio::test(flavor = "multi_thread")]
async fn speech_stays_intelligible_over_home_network() {
    let run = talk().await;
    let Run { stats, quality, .. } = run;
    let played = stats.decoded + stats.concealed;
    let concealed = stats.concealed as f64 / played as f64;
    println!(
        "jitter target max {}, depth mean {:.2} / p95 {} frames; concealed {concealed:.4} \
         of {played} frames; SNR {:.1} dB, poor {:.4} of {} voiced frames; \
         runner stalls {} (longest {:?})",
        run.max_target,
        run.mean_depth,
        run.p95_depth,
        quality.snr_db,
        quality.poor,
        quality.voiced_frames,
        run.stalls,
        run.max_stall
    );
    // PLAN.md: the jitter buffer stays at most two frames. (Two hops of
    // ±5 ms make ±10 ms end to end, so the target can occasionally need 3.)
    assert!(run.max_target <= 2 && run.p95_depth <= 2, "{run:?}");
    assert!(played as usize >= SECONDS * 95, "{stats:?}");
    assert!(concealed < 0.03, "{stats:?}");
    assert!(quality.snr_db > 10.0 && quality.poor < 0.05, "{quality:?}");
}

/// One run: server, a talker and a listener behind their own netem proxies.
async fn talk() -> Run {
    let identity = Identity::generate();
    let server = Arc::new(Server::bind("127.0.0.1:0".parse().unwrap(), &identity).unwrap());
    let addr = server.local_addr().unwrap();
    let run = server.clone();
    tokio::spawn(async move { run.run().await });
    let fp = identity.fingerprint();
    let local = "127.0.0.1:0".parse().unwrap();
    let profile = netem::Profile::HOME;
    let via_a = netem::spawn(local, addr, profile, 1).await.unwrap();
    let via_b = netem::spawn(local, addr, profile, 2).await.unwrap();
    let mut talker = Voice::start(Arc::new(Client::connect(via_a, &fp).await.unwrap())).unwrap();
    let mut listener = Voice::start(Arc::new(Client::connect(via_b, &fp).await.unwrap())).unwrap();

    // The audio thread: one capture and one playback frame per 10 ms.
    tokio::task::spawn_blocking(move || {
        let input = speech(SECONDS * SAMPLE_RATE as usize);
        let start = Instant::now();
        let mut output = Vec::with_capacity(input.len());
        let mut out = [0.0; FRAME_SAMPLES];
        let (mut stalls, mut max_stall, mut skipped) = (0, Duration::ZERO, Duration::ZERO);
        let (mut max_target, mut depths) = (0, Vec::new());
        for (i, frame) in input.as_chunks().0.iter().enumerate() {
            let due = start + skipped + Duration::from_millis(10) * i as u32;
            // Not `sleep`: on macOS CI it overshoots by 10 ms or more on most
            // ticks, which the listener would hear as jitter.
            while Instant::now() < due {
                std::thread::yield_now();
            }
            let late = Instant::now() - due;
            if late > STALL {
                stalls += 1;
                max_stall = max_stall.max(late);
                skipped += late;
            }
            talker.capture(frame);
            listener.play(&mut out);
            output.extend_from_slice(&out);
            let s = listener.stats();
            max_target = max_target.max(s.jitter_target);
            depths.push(s.jitter_depth);
        }
        let mean_depth = depths.iter().sum::<usize>() as f64 / depths.len() as f64;
        depths.sort();
        Run {
            stats: listener.stats(),
            quality: compare(&input, &output, CODEC_DELAY),
            stalls,
            max_stall,
            max_target,
            mean_depth,
            p95_depth: depths[depths.len() * 95 / 100],
        }
    })
    .await
    .unwrap()
}
