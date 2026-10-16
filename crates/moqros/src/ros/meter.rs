//! Logging helpers shared by the bridges.

use std::time::{Duration, Instant};

/// Lets only the first of a series of warnings through, so a stream of bad messages
/// doesn't flood the log.
#[derive(Default)]
pub(super) struct WarnOnce(bool);

impl WarnOnce {
	/// Whether this is the first warning.
	pub(super) fn first(&mut self) -> bool {
		!std::mem::replace(&mut self.0, true)
	}
}

/// Logs the message rate of a bridge every few seconds.
pub(super) struct RateMeter {
	label: &'static str,
	count: u64,
	since: Instant,
}

impl RateMeter {
	const PERIOD: Duration = Duration::from_secs(10);

	pub(super) fn new(label: &'static str) -> Self {
		Self {
			label,
			count: 0,
			since: Instant::now(),
		}
	}

	pub(super) fn tick(&mut self) {
		self.count += 1;
		let elapsed = self.since.elapsed();
		if elapsed >= Self::PERIOD {
			let rate = self.count as f64 / elapsed.as_secs_f64();
			tracing::info!(bridge = self.label, rate = format!("{rate:.1}/s"), "message rate");
			self.count = 0;
			self.since = Instant::now();
		}
	}
}
