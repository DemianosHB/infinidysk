//! Read-ahead PAR2 repair window, ported from the C# v1 fork
//! (`backend/Streams/MultiSegmentStream.cs`: `ComputeReadAheadWindowBytes`,
//! `SampleDeliveryRate`, and `RangeContext.ReadAheadRepairOptions`).
//!
//! When enabled (`repair.readahead.enabled` in v1), an active stream widens its prefetch
//! window to measured-bitrate × window ahead of the read head (capped by the in-flight
//! article RAM budget), so missing or corrupt articles are discovered — and reported for
//! PAR2 repair — before playback reaches them.

use std::time::{Duration, Instant};

/// Default read-ahead window, in seconds (v1 `repair.readahead.window-seconds`).
pub const DEFAULT_WINDOW_SECONDS: u32 = 45;
/// Allowed read-ahead window range, in seconds.
pub const WINDOW_SECONDS_RANGE: std::ops::RangeInclusive<u32> = 5..=600;
/// Shortest interval between two delivery-rate samples.
const MIN_SAMPLE_INTERVAL: Duration = Duration::from_millis(250);
/// Weight of a new instant rate in the moving average.
const EMA_ALPHA: f64 = 0.25;

/// The read-ahead window in bytes for a measured delivery rate: `rate × window`, capped by
/// `cap_bytes`. Zero when the rate is not positive yet, the window is zero, or the cap is
/// zero — a zero window never widens the stock prefetch ceiling.
/// The window is a float product; the casts only need f64 precision, and v1 truncates a
/// positive window to `long` the same way.
#[expect(clippy::cast_precision_loss)]
#[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
#[must_use]
pub fn window_bytes(rate_bytes_per_second: f64, window_seconds: u32, cap_bytes: u64) -> u64 {
    if !rate_bytes_per_second.is_finite()
        || rate_bytes_per_second <= 0.0
        || window_seconds == 0
        || cap_bytes == 0
    {
        return 0;
    }
    let window = rate_bytes_per_second * f64::from(window_seconds);
    if window >= cap_bytes as f64 {
        cap_bytes
    } else {
        window as u64
    }
}

/// The measured delivery rate of one stream: an exponential moving average over the bytes
/// consumed, sampled at most once per [`MIN_SAMPLE_INTERVAL`]. The first sample only
/// starts the clock; the rate stays zero until a second sample spans enough time.
#[derive(Debug, Default)]
pub struct DeliveryRate {
    last: Option<(Instant, u64)>,
    rate: f64,
}

impl DeliveryRate {
    /// Records `consumed_bytes` at `now` and returns the current rate estimate in bytes per
    /// second.
    pub fn sample(&mut self, consumed_bytes: u64, now: Instant) -> f64 {
        let Some((last_at, last_bytes)) = self.last.replace((now, consumed_bytes)) else {
            return 0.0;
        };
        let elapsed = now.saturating_duration_since(last_at);
        if elapsed < MIN_SAMPLE_INTERVAL {
            return self.rate;
        }
        #[expect(
            clippy::cast_precision_loss,
            reason = "a rate estimate needs no exact u64 precision"
        )]
        let instant = (consumed_bytes.saturating_sub(last_bytes)) as f64 / elapsed.as_secs_f64();
        self.rate = if self.rate <= 0.0 {
            instant
        } else {
            self.rate + EMA_ALPHA * (instant - self.rate)
        };
        self.rate
    }

    /// The current rate estimate in bytes per second; zero until two samples span enough
    /// time.
    #[must_use]
    pub const fn rate(&self) -> f64 {
        self.rate
    }

    /// Overrides the rate estimate (v1 `SetDeliveryRateForTests`). Tests only.
    #[cfg(test)]
    pub(crate) fn set_rate(&mut self, rate: f64) {
        self.rate = rate;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_bytes_matches_v1_expectations() {
        // Ported from ReadAheadRepairCeilingTests.ComputeReadAheadWindowBytes_MatchesExpectation.
        let cases: [(f64, u32, u64, u64); 5] = [
            (0.0, 45, 1_000_000, 0),
            (-5.0, 45, 1_000_000, 0),
            (1_000.0, 0, 1_000_000, 0),
            (500_000.0, 45, 1_000_000_000, 22_500_000),
            (1_000_000.0, 60, 10_000_000, 10_000_000),
        ];
        for (rate, window, cap, expected) in cases {
            assert_eq!(
                window_bytes(rate, window, cap),
                expected,
                "rate={rate} window={window} cap={cap}"
            );
        }
        assert_eq!(window_bytes(f64::NAN, 45, 1_000_000), 0);
        assert_eq!(window_bytes(1_000.0, 45, 0), 0);
    }

    #[test]
    fn delivery_rate_starts_at_zero_and_averages_new_instants() {
        let start = Instant::now();
        let mut rate = DeliveryRate::default();
        // First sample only starts the clock.
        assert_eq!(rate.sample(0, start), 0.0);
        assert_eq!(rate.rate(), 0.0);
        // 1 MB in 1 s: the first measured instant becomes the rate.
        assert_eq!(
            rate.sample(1_000_000, start + Duration::from_secs(1)),
            1_000_000.0
        );
        // Samples closer than 250 ms do not move the rate.
        assert_eq!(
            rate.sample(1_100_000, start + Duration::from_millis(1200)),
            1_000_000.0
        );
        // The 200 ms sample still advanced the clock (as v1's Exchange does), so the next
        // instant is 2 MB over 0.8 s = 2.5 MB/s; EMA: 1 + 0.25 * (2.5 - 1) = 1.375 MB/s.
        assert_eq!(
            rate.sample(3_100_000, start + Duration::from_secs(2)),
            1_375_000.0
        );
    }
}
