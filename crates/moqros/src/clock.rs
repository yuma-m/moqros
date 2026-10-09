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
			Some(ahead) => base + ahead.as_micros() as u64,
			None => base.saturating_sub((anchor - capture).as_micros() as u64),
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
}
