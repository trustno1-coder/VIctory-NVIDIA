//! Pair pacing on the SDK presenter thread, including below 10 rendered FPS.
use std::time::{Duration, Instant};

#[derive(Default)]
pub(super) struct Pacer {
    previous_generated: Option<Instant>,
    pair: Option<(u64, Instant)>,
    period: Option<Duration>,
}

impl Pacer {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Called once per actual SDK presentation callback. A real frame without
    /// its generated partner (off/loading) is never delayed.
    pub fn deadline(&mut self, generated: bool, frame: u64, now: Instant) -> Option<Instant> {
        if generated {
            if let Some(previous) = self.previous_generated {
                let sample = now.saturating_duration_since(previous);
                // A loading pause is not the cadence to reproduce for subsequent pairs.
                if sample > Duration::from_millis(500) {
                    self.period = None;
                } else if sample >= Duration::from_millis(4) {
                    self.period = Some(match self.period {
                        Some(period) => period.mul_f64(0.75) + sample.mul_f64(0.25),
                        None => sample,
                    });
                }
            }
            self.previous_generated = Some(now);
            let half = self.period.unwrap_or(Duration::from_millis(16)) / 2;
            self.pair = Some((frame, now + half.min(Duration::from_millis(125))));
            None
        } else {
            match self.pair.take() {
                Some((id, deadline)) if id == frame => Some(deadline),
                _ => {
                    self.reset();
                    None
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spaces_slow_pairs_without_ten_frame_warmup_or_100ms_reset() {
        for period_ms in [80, 100, 125, 200] {
            let mut pacer = Pacer::default();
            let start = Instant::now();
            for frame in 0..20 {
                let generated = start + Duration::from_millis(period_ms * frame);
                assert!(pacer.deadline(true, frame, generated).is_none());
                let real = pacer.deadline(false, frame, generated).unwrap();
                if frame > 0 {
                    assert_eq!(real - generated, Duration::from_millis(period_ms) / 2);
                }
            }
        }
    }

    #[test]
    fn no_delay_without_partner_and_no_stale_delay_after_pause() {
        let mut pacer = Pacer::default();
        let start = Instant::now();
        assert!(pacer.deadline(false, 1, start).is_none());
        pacer.deadline(true, 2, start);
        assert!(pacer.deadline(false, 3, start).is_none());
        pacer.deadline(true, 4, start);
        let later = start + Duration::from_secs(3);
        pacer.deadline(true, 5, later);
        assert_eq!(
            pacer.deadline(false, 5, later).unwrap() - later,
            Duration::from_millis(8)
        );
    }
}
