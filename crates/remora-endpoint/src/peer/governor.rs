//! Sans-IO encoder caps derived from outbound video loss and RTT.

use std::time::Duration;

const MAX_REQUESTED_KBPS: u32 = 500_000;
const MIN_INCREASE_KBPS: u32 = 500;
const FPS_LADDER: [u32; 13] = [144, 120, 90, 60, 45, 30, 24, 20, 15, 12, 10, 8, 5];

#[derive(Debug, Clone, PartialEq)]
pub struct GovernorConfig {
    pub floor_kbps: u32,
    pub auto_ceiling_kbps: u32,
    pub loss_congested_pct: f32,
    pub loss_clean_pct: f32,
    pub rtt_congested_ms: u32,
    pub rtt_clean_ms: u32,
    pub rtt_congested_margin_ms: u32,
    pub rtt_clean_margin_ms: u32,
    pub decrease_cooldown: Duration,
    pub increase_cooldown: Duration,
    pub probe_quiet_window: Duration,
    pub fps_adapt: bool,
}

impl Default for GovernorConfig {
    fn default() -> Self {
        Self {
            floor_kbps: 800,
            auto_ceiling_kbps: 8_000,
            loss_congested_pct: 8.0,
            loss_clean_pct: 1.0,
            rtt_congested_ms: 300,
            rtt_clean_ms: 150,
            rtt_congested_margin_ms: 250,
            rtt_clean_margin_ms: 100,
            decrease_cooldown: Duration::from_secs(3),
            increase_cooldown: Duration::from_secs(4),
            probe_quiet_window: Duration::from_secs(60),
            fps_adapt: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GovernorSample {
    pub now: Duration,
    pub fraction_lost_pct: f32,
    pub rtt_ms: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GovernorDecision {
    pub bitrate_kbps: u32,
    pub fps_cap: Option<u32>,
}

#[derive(Debug)]
pub struct VideoGovernor {
    config: GovernorConfig,
    ceiling_kbps: u32,
    nominal_fps: u32,
    current: Option<GovernorDecision>,
    aim_kbps: Option<u32>,
    min_rtt_ms: Option<u32>,
    last_congestion: Option<Duration>,
    last_decrease: Option<Duration>,
    last_change: Option<Duration>,
}

impl VideoGovernor {
    pub fn new(config: GovernorConfig) -> Self {
        Self {
            ceiling_kbps: config.auto_ceiling_kbps,
            config,
            nominal_fps: 30,
            current: None,
            aim_kbps: None,
            min_rtt_ms: None,
            last_congestion: None,
            last_decrease: None,
            last_change: None,
        }
    }

    pub fn set_requested_limits(&mut self, requested_kbps: Option<u32>, nominal_fps: u32) {
        self.ceiling_kbps = requested_kbps
            .filter(|rate| (1..=MAX_REQUESTED_KBPS).contains(rate))
            .unwrap_or(self.config.auto_ceiling_kbps);
        self.nominal_fps = nominal_fps;
    }

    pub fn on_sample(&mut self, sample: GovernorSample) -> Option<GovernorDecision> {
        let GovernorSample {
            now,
            fraction_lost_pct,
            rtt_ms,
        } = sample;
        if let Some(rtt_ms) = rtt_ms.filter(|&rtt| rtt != 0) {
            self.min_rtt_ms = Some(
                self.min_rtt_ms
                    .map_or(rtt_ms, |baseline| baseline.min(rtt_ms)),
            );
        }
        let baseline = self.min_rtt_ms.unwrap_or(0);
        let congested_rtt = self
            .config
            .rtt_congested_ms
            .max(baseline.saturating_add(self.config.rtt_congested_margin_ms));
        let clean_rtt = self
            .config
            .rtt_clean_ms
            .max(baseline.saturating_add(self.config.rtt_clean_margin_ms));
        let congested = fraction_lost_pct >= self.config.loss_congested_pct
            || rtt_ms.is_some_and(|rtt| rtt >= congested_rtt);
        let clean = fraction_lost_pct < self.config.loss_clean_pct
            && rtt_ms.is_none_or(|rtt| rtt < clean_rtt);
        let can_increase_or_step_fps =
            elapsed(now, self.last_change, self.config.increase_cooldown);
        let floor = self.config.floor_kbps.min(self.ceiling_kbps);
        let previous = self.current.unwrap_or(GovernorDecision {
            bitrate_kbps: self.ceiling_kbps,
            fps_cap: None,
        });
        let mut next = GovernorDecision {
            bitrate_kbps: previous.bitrate_kbps.clamp(floor, self.ceiling_kbps),
            fps_cap: previous.fps_cap.filter(|&fps| fps < self.nominal_fps),
        };

        if congested {
            self.last_congestion = Some(now);
            if elapsed(now, self.last_decrease, self.config.decrease_cooldown) {
                if next.bitrate_kbps > floor {
                    self.aim_kbps = Some((u64::from(next.bitrate_kbps) * 9 / 10) as u32);
                    next.bitrate_kbps = (next.bitrate_kbps / 2).max(floor);
                    self.last_decrease = Some(now);
                } else if self.config.fps_adapt && can_increase_or_step_fps {
                    let current_fps = next.fps_cap.unwrap_or(self.nominal_fps);
                    if let Some(lower) = FPS_LADDER.into_iter().find(|&fps| fps < current_fps) {
                        next.fps_cap = Some(lower);
                        self.last_decrease = Some(now);
                    }
                }
            }
        } else if can_increase_or_step_fps {
            let can_probe = elapsed(now, self.last_congestion, self.config.probe_quiet_window);
            if clean {
                let target = if can_probe {
                    self.ceiling_kbps
                } else {
                    self.aim_kbps
                        .unwrap_or(self.ceiling_kbps)
                        .clamp(floor, self.ceiling_kbps)
                };
                if next.bitrate_kbps < target {
                    let increase = (next.bitrate_kbps / 4).max(MIN_INCREASE_KBPS);
                    next.bitrate_kbps = next.bitrate_kbps.saturating_add(increase).min(target);
                    if can_probe {
                        self.aim_kbps = None;
                    }
                }
            }
            if self.config.fps_adapt
                && can_probe
                && let Some(current_fps) = next.fps_cap
            {
                next.fps_cap = FPS_LADDER
                    .into_iter()
                    .rev()
                    .find(|&fps| fps > current_fps)
                    .filter(|&fps| fps < self.nominal_fps);
            }
        }

        self.current = Some(next);
        if next == previous {
            return None;
        }
        self.last_change = Some(now);
        Some(next)
    }
}

fn elapsed(now: Duration, since: Option<Duration>, cooldown: Duration) -> bool {
    since.is_none_or(|since| now.saturating_sub(since) >= cooldown)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(
        governor: &mut VideoGovernor,
        secs: u64,
        loss_pct: f32,
        rtt_ms: Option<u32>,
    ) -> Option<GovernorDecision> {
        governor.on_sample(GovernorSample {
            now: Duration::from_secs(secs),
            fraction_lost_pct: loss_pct,
            rtt_ms,
        })
    }

    fn cap(bitrate_kbps: u32, fps_cap: Option<u32>) -> Option<GovernorDecision> {
        Some(GovernorDecision {
            bitrate_kbps,
            fps_cap,
        })
    }

    #[test]
    fn congestion_halves_bitrate_down_to_floor_and_respects_decrease_cooldown() {
        let mut governor = VideoGovernor::new(GovernorConfig::default());
        assert_eq!(sample(&mut governor, 0, 8.0, None), cap(4_000, None));
        assert_eq!(sample(&mut governor, 2, 8.0, None), None);
        assert_eq!(sample(&mut governor, 3, 8.0, None), cap(2_000, None));
        assert_eq!(sample(&mut governor, 6, 8.0, None), cap(1_000, None));
        assert_eq!(sample(&mut governor, 9, 8.0, None), cap(800, None));
        assert_eq!(sample(&mut governor, 13, 8.0, None), None);
        assert_eq!(sample(&mut governor, 99, 8.0, None), None);
    }

    #[test]
    fn long_path_recovers_at_its_baseline_and_cuts_only_on_rtt_spike() {
        let mut governor = VideoGovernor::new(GovernorConfig::default());
        assert_eq!(sample(&mut governor, 0, 0.0, Some(160)), None);
        assert_eq!(sample(&mut governor, 1, 9.0, Some(160)), cap(4_000, None));
        assert_eq!(sample(&mut governor, 4, 0.0, Some(160)), None);
        assert_eq!(sample(&mut governor, 5, 0.0, Some(160)), cap(5_000, None));
        assert_eq!(sample(&mut governor, 9, 0.0, Some(409)), None);
        assert_eq!(sample(&mut governor, 10, 0.0, Some(410)), cap(2_500, None));
    }

    #[test]
    fn hold_band_jitter_does_not_raise_or_cut_the_cap() {
        let mut governor = VideoGovernor::new(GovernorConfig::default());
        assert_eq!(sample(&mut governor, 0, 8.0, None), cap(4_000, None));
        assert_eq!(sample(&mut governor, 4, 4.0, Some(100)), None);
        assert_eq!(sample(&mut governor, 60, 4.0, Some(185)), None);
        assert_eq!(sample(&mut governor, 61, 0.9, Some(100)), cap(5_000, None));
    }

    #[test]
    fn recovery_stops_at_ninety_percent_until_quiet_probe() {
        let mut governor = VideoGovernor::new(GovernorConfig::default());
        assert_eq!(sample(&mut governor, 0, 8.0, None), cap(4_000, None));
        assert_eq!(sample(&mut governor, 4, 0.0, None), cap(5_000, None));
        assert_eq!(sample(&mut governor, 8, 0.0, None), cap(6_250, None));
        assert_eq!(sample(&mut governor, 12, 0.0, None), cap(7_200, None));
        assert_eq!(sample(&mut governor, 59, 0.0, None), None);
        assert_eq!(sample(&mut governor, 60, 0.0, None), cap(8_000, None));
    }

    #[test]
    fn fps_steps_only_at_floor_then_recovers_in_hold_band_after_quiet_window() {
        let mut governor = VideoGovernor::new(GovernorConfig {
            fps_adapt: true,
            ..GovernorConfig::default()
        });
        governor.set_requested_limits(Some(1_200), 50);
        assert_eq!(sample(&mut governor, 0, 8.0, None), cap(800, None));
        assert_eq!(sample(&mut governor, 3, 8.0, None), None);
        assert_eq!(sample(&mut governor, 4, 8.0, None), cap(800, Some(45)));
        assert_eq!(sample(&mut governor, 8, 8.0, None), cap(800, Some(30)));
        assert_eq!(sample(&mut governor, 67, 4.0, None), None);
        assert_eq!(sample(&mut governor, 68, 4.0, None), cap(800, Some(45)));
        assert_eq!(sample(&mut governor, 70, 8.0, None), None);
        assert_eq!(sample(&mut governor, 129, 4.0, None), None);
        assert_eq!(sample(&mut governor, 130, 4.0, None), cap(800, None));
    }

    #[test]
    fn fps_adaptation_is_disabled_by_default_even_at_the_floor() {
        let mut governor = VideoGovernor::new(GovernorConfig::default());
        governor.set_requested_limits(Some(800), 144);
        assert_eq!(sample(&mut governor, 0, 9.0, None), None);
        assert_eq!(sample(&mut governor, 4, 9.0, None), None);
        assert_eq!(sample(&mut governor, 64, 4.0, None), None);
    }

    #[test]
    fn requested_ceiling_validates_and_can_be_lower_than_the_floor() {
        let mut governor = VideoGovernor::new(GovernorConfig::default());
        governor.set_requested_limits(Some(500_000), 30);
        assert_eq!(sample(&mut governor, 0, 8.0, None), cap(250_000, None));
        governor.set_requested_limits(Some(500_001), 30);
        assert_eq!(sample(&mut governor, 1, 4.0, None), cap(8_000, None));
        governor.set_requested_limits(Some(500), 30);
        assert_eq!(sample(&mut governor, 2, 4.0, None), cap(500, None));
        governor.set_requested_limits(Some(0), 30);
        assert_eq!(sample(&mut governor, 3, 4.0, None), cap(800, None));
    }
}
