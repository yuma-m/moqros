//! Timestamp normalization shared by the encoder and the publisher.

use std::time::Duration;

/// Backwards steps larger than this are a restarted capture clock (e.g. a looping
/// rosbag) rather than jitter, and the timeline continues from where it was.
const MAX_BACKWARDS_STEP: Duration = Duration::from_secs(1);

/// Maps capture timestamps onto a strictly increasing microsecond timeline
/// starting at zero, tolerating clock jumps and duplicate stamps.
#[derive(Debug, Default)]
pub(crate) struct StreamClock {
	/// A capture time and the stream time it maps to.
	anchor: Option<(Duration, u64)>,
	prev: Option<Duration>,
	last: Option<u64>,
}

impl StreamClock {
	pub(crate) fn micros(&mut self, capture: Duration) -> u64 {
		if let (Some(prev), Some(last)) = (self.prev, self.last)
			&& prev.saturating_sub(capture) > MAX_BACKWARDS_STEP
		{
			self.anchor = Some((capture, last + 1));
		}
		self.prev = Some(capture);

		let (anchor, base) = *self.anchor.get_or_insert((capture, 0));
		let mut micros = match capture.checked_sub(anchor) {
			Some(ahead) => base + saturating_micros(ahead),
			None => base.saturating_sub(saturating_micros(anchor - capture)),
		};
		if let Some(last) = self.last
			&& micros <= last
		{
			micros = last + 1;
		}
		self.last = Some(micros);
		micros
	}
}

impl StreamClock {
	/// The capture time, in µs, that stream time 0 maps to, so that a stream time `pts` was
	/// captured at `origin + pts`. Negative when the capture clock restarted below the stream
	/// time it had reached. `None` before the first timestamp. Changes when the timeline
	/// re-anchors after a backwards jump.
	pub(crate) fn origin_micros(&self) -> Option<i64> {
		let (capture, base) = self.anchor?;
		let capture = i64::try_from(capture.as_micros()).unwrap_or(i64::MAX);
		Some(capture.saturating_sub(i64::try_from(base).unwrap_or(i64::MAX)))
	}
}

/// The capture time of stream time `pts` on a timeline whose [origin](StreamClock::origin_micros)
/// is `origin`, or `None` if it would be before the capture clock's zero.
pub(crate) fn capture_time(origin: i64, pts: Duration) -> Option<Duration> {
	let micros = i128::from(origin) + i128::try_from(pts.as_micros()).ok()?;
	u64::try_from(micros).ok().map(Duration::from_micros)
}

/// `duration` in microseconds, saturating at `u64::MAX`.
fn saturating_micros(duration: Duration) -> u64 {
	u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn stream_clock_is_monotonic() {
		let mut clock = StreamClock::default();
		assert_eq!(clock.micros(Duration::from_secs(100)), 0);
		assert_eq!(clock.micros(Duration::from_millis(100_033)), 33_000);
		assert_eq!(clock.micros(Duration::from_millis(100_033)), 33_001);
		assert_eq!(clock.micros(Duration::from_secs(99)), 33_002);
	}

	#[test]
	fn stream_clock_absorbs_small_backwards_steps() {
		let mut clock = StreamClock::default();
		assert_eq!(clock.micros(Duration::from_millis(10_000)), 0);
		assert_eq!(clock.micros(Duration::from_millis(10_033)), 33_000);
		assert_eq!(clock.micros(Duration::from_millis(10_032)), 33_001);
		// Jitter doesn't shift the timeline.
		assert_eq!(clock.micros(Duration::from_millis(10_066)), 66_000);
	}

	#[test]
	fn stream_clock_continues_after_a_backwards_jump() {
		let mut clock = StreamClock::default();
		assert_eq!(clock.micros(Duration::from_secs(10)), 0);
		assert_eq!(clock.micros(Duration::from_secs(5)), 1);
		assert_eq!(clock.micros(Duration::from_secs(5)), 2);
		// Time keeps its pace from the jump instead of collapsing onto `last + 1`.
		assert_eq!(clock.micros(Duration::from_secs(6)), 1_000_001);
		assert_eq!(clock.micros(Duration::from_secs(11)), 6_000_001);
	}

	#[test]
	fn origin_follows_the_anchor() {
		let mut clock = StreamClock::default();
		assert_eq!(clock.origin_micros(), None);
		clock.micros(Duration::from_secs(10));
		assert_eq!(clock.origin_micros(), Some(10_000_000));
		clock.micros(Duration::from_millis(10_033));
		assert_eq!(clock.origin_micros(), Some(10_000_000));
		// A restarted capture clock moves the origin, below zero here.
		let pts = clock.micros(Duration::from_millis(10));
		assert_eq!(pts, 33_001);
		assert_eq!(clock.origin_micros(), Some(10_000 - 33_001));
		let origin = clock.origin_micros().unwrap();
		assert_eq!(
			capture_time(origin, Duration::from_micros(pts)),
			Some(Duration::from_millis(10))
		);
		assert_eq!(capture_time(origin, Duration::ZERO), None);
	}
}
