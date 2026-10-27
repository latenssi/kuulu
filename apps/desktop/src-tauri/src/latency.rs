//! Mouth-to-ear latency estimate: the budget rows of docs/PLAN.md, with the
//! network and jitter buffer rows replaced by live measurements when
//! available. Device and driver latency is not included (measured in M6).

use serde::Serialize;
use std::time::Duration;

/// Budget network delay (one way) and jitter buffer target, docs/PLAN.md.
const BUDGET_NETWORK_MS: f32 = 11.0;
const FRAME_MS: f32 = 10.0;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Latency {
    pub total_ms: f32,
    pub stages: Vec<Stage>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Stage {
    pub name: &'static str,
    pub ms: f32,
    pub detail: String,
    /// From a live measurement rather than the budget.
    pub measured: bool,
}

/// `rtt`: round trip to the server; `jitter_target`: frames.
pub fn estimate(rtt: Option<Duration>, jitter_target: Option<usize>) -> Latency {
    let fixed = |name, ms, detail: &str| Stage {
        name,
        ms,
        detail: detail.into(),
        measured: false,
    };
    let network = match rtt {
        Some(rtt) => Stage {
            name: "Network",
            ms: rtt.as_secs_f32() * 500.0,
            detail: "Half the round trip, through the server".into(),
            measured: true,
        },
        None => fixed("Network", BUDGET_NETWORK_MS, "One way, through the server"),
    };
    let jitter = match jitter_target {
        Some(frames) => Stage {
            name: "Jitter buffer",
            ms: frames as f32 * FRAME_MS,
            detail: format!(
                "Adaptive, currently {frames} frame{}",
                if frames == 1 { "" } else { "s" }
            ),
            measured: true,
        },
        None => fixed("Jitter buffer", FRAME_MS, "Adaptive, target one frame"),
    };
    let stages = vec![
        fixed("Capture", 5.0, "240 samples at 48 kHz"),
        fixed(
            "Encode",
            7.5,
            "10 ms Opus frame (half overlaps capture) + 2.5 ms look-ahead",
        ),
        network,
        jitter,
        fixed("Playback", 10.0, "5 ms queue + 240-sample device period"),
    ];
    Latency {
        total_ms: stages.iter().map(|s| s.ms).sum(),
        stages,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_without_measurements() {
        let latency = estimate(None, None);
        assert_eq!(latency.total_ms, 43.5);
        assert!(latency.stages.iter().all(|s| !s.measured));
    }

    #[test]
    fn uses_half_the_rtt_and_the_jitter_target() {
        let latency = estimate(Some(Duration::from_millis(30)), Some(2));
        assert_eq!(latency.total_ms, 43.5 - 11.0 + 15.0 - 10.0 + 20.0);
        let jitter = &latency.stages[3];
        assert!(jitter.measured);
        assert_eq!(jitter.detail, "Adaptive, currently 2 frames");
    }
}
