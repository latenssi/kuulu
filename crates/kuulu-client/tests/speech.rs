//! M3 acceptance (docs/PLAN.md): two headless voice clients talk through a
//! server over the home network profile, in real time, on synthetic speech.

use kuulu_audio::quality::{compare, speech};
use kuulu_audio::{FRAME_SAMPLES, SAMPLE_RATE};
use kuulu_client::voice::Voice;
use kuulu_client::{Client, netem};
use kuulu_server::{Identity, Server};
use std::sync::Arc;
use std::time::{Duration, Instant};

const SECONDS: usize = 10;
/// Opus restricted low-delay look-ahead in samples.
const CODEC_DELAY: usize = 120;

#[tokio::test(flavor = "multi_thread")]
async fn speech_stays_intelligible_over_home_network() {
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

    let source = speech(SECONDS * SAMPLE_RATE as usize);
    let input = source.clone();
    // The audio thread: one capture and one playback frame per 10 ms.
    let (output, stats, max_target, max_depth) = tokio::task::spawn_blocking(move || {
        let start = Instant::now();
        let mut output = Vec::with_capacity(input.len());
        let mut out = [0.0; FRAME_SAMPLES];
        let (mut max_target, mut max_depth) = (0, 0);
        for (i, frame) in input.as_chunks().0.iter().enumerate() {
            let due = start + Duration::from_millis(10) * i as u32;
            std::thread::sleep(due.saturating_duration_since(Instant::now()));
            talker.capture(frame);
            listener.play(&mut out);
            output.extend_from_slice(&out);
            let s = listener.stats();
            max_target = max_target.max(s.jitter_target);
            max_depth = max_depth.max(s.jitter_depth);
        }
        (output, listener.stats(), max_target, max_depth)
    })
    .await
    .unwrap();

    let quality = compare(&source, &output, CODEC_DELAY);
    let played = stats.decoded + stats.concealed;
    let concealed = stats.concealed as f64 / played as f64;
    println!(
        "jitter target max {max_target}, depth max {max_depth} frames; \
         concealed {concealed:.4} of {played} frames; \
         SNR {:.1} dB, poor frames {:.4} of {} voiced",
        quality.snr_db, quality.poor, quality.voiced_frames
    );
    assert!(max_target <= 2, "jitter target {max_target}");
    assert!(max_depth <= 2, "jitter depth {max_depth}");
    assert!(played as usize >= SECONDS * 95, "{stats:?}");
    assert!(concealed < 0.03, "{stats:?}");
    assert!(quality.snr_db > 10.0 && quality.poor < 0.05, "{quality:?}");
}
