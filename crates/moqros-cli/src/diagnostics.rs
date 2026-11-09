//! Bridge statistics on `/diagnostics` (`diagnostic_msgs/msg/DiagnosticArray`).

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use moqros::ros::{BridgeStats, QosProfile, now_stamp, r2r};
use serde_json::json;
use tokio::sync::watch;

/// Topic the diagnostics go to, as the ROS diagnostics tools expect.
pub const DIAGNOSTICS_TOPIC: &str = "/diagnostics";
const DIAGNOSTICS_TYPE: &str = "diagnostic_msgs/msg/DiagnosticArray";

/// A bridge that has received nothing for this long is reported as stale.
const STALE_AFTER: Duration = Duration::from_secs(5);

/// `diagnostic_msgs/msg/DiagnosticStatus` levels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
	Ok = 0,
	Warn = 1,
	Error = 2,
}

/// Publishes one `DiagnosticStatus` per bridge of the node.
pub struct Diagnostics {
	publisher: r2r::PublisherUntyped,
	hardware_id: String,
	bridges: BTreeMap<String, Entry>,
}

struct Entry {
	/// What the bridge does, e.g. `ROS -> MoQ video`, reported as a value.
	kind: String,
	state: State,
}

enum State {
	Running {
		stats: watch::Receiver<BridgeStats>,
		started: Instant,
		/// Counts at the previous report, to compute rates.
		previous: (Instant, BridgeStats),
	},
	Restarting {
		since: Instant,
	},
}

impl Diagnostics {
	/// Create the `/diagnostics` publisher on `node`. Fails if this build has no bindings for
	/// `diagnostic_msgs` (see `IDL_PACKAGE_FILTER`).
	pub fn new(node: &mut r2r::Node) -> anyhow::Result<Self> {
		let publisher = node.create_publisher_untyped(DIAGNOSTICS_TOPIC, DIAGNOSTICS_TYPE, QosProfile::default())?;
		Ok(Self {
			publisher,
			hardware_id: node.fully_qualified_name()?,
			bridges: BTreeMap::new(),
		})
	}

	/// Report on the bridge `name`, which has just started (again).
	pub fn running(&mut self, name: &str, kind: &str, stats: watch::Receiver<BridgeStats>) {
		let now = Instant::now();
		let previous = (now, *stats.borrow());
		self.bridges.insert(
			name.to_string(),
			Entry {
				kind: kind.to_string(),
				state: State::Running {
					stats,
					started: now,
					previous,
				},
			},
		);
	}

	/// Report the bridge `name` as stopped and waiting to restart.
	pub fn restarting(&mut self, name: &str) {
		if let Some(entry) = self.bridges.get_mut(name) {
			entry.state = State::Restarting { since: Instant::now() };
		}
	}

	/// Publish the state of every bridge.
	pub fn publish(&mut self) -> anyhow::Result<()> {
		let now = Instant::now();
		let status: Vec<_> = self
			.bridges
			.iter_mut()
			.map(|(name, entry)| entry.status(name, &self.hardware_id, now))
			.collect();
		let stamp = now_stamp();
		self.publisher.publish(json!({
			"header": { "stamp": { "sec": stamp.sec, "nanosec": stamp.nanosec }, "frame_id": "" },
			"status": status,
		}))?;
		Ok(())
	}
}

impl Entry {
	fn status(&mut self, name: &str, hardware_id: &str, now: Instant) -> serde_json::Value {
		let mut values = vec![("kind", self.kind.clone())];
		let (level, message) = match &mut self.state {
			State::Running {
				stats,
				started,
				previous,
			} => {
				let current = *stats.borrow();
				let report = Report::new(&current, previous, *started, now);
				*previous = (now, current);
				values.extend([
					("received", current.received.to_string()),
					("sent", current.sent.to_string()),
					("dropped", current.dropped.to_string()),
					("rate", format!("{:.1}", report.rate)),
				]);
				if let Some(age) = report.idle {
					values.push(("since_last_message", format!("{:.1}s", age.as_secs_f64())));
				}
				report.assess()
			}
			State::Restarting { since } => (
				Level::Error,
				format!("stopped {:.0}s ago, restarting", (now - *since).as_secs_f64()),
			),
		};
		json!({
			"level": level as u8,
			"name": format!("moqros: {name}"),
			"message": message,
			"hardware_id": hardware_id,
			"values": values
				.into_iter()
				.map(|(key, value)| json!({ "key": key, "value": value }))
				.collect::<Vec<_>>(),
		})
	}
}

/// What a running bridge did since the previous report.
#[derive(Debug, PartialEq)]
struct Report {
	/// Messages received per second.
	rate: f64,
	/// Messages received and dropped in the period.
	received: u64,
	dropped: u64,
	/// How long nothing has arrived, since the last message or else since the bridge started.
	idle: Option<Duration>,
}

impl Report {
	#[expect(clippy::cast_precision_loss, reason = "message counts are far below 2^52")]
	fn new(current: &BridgeStats, (since, previous): &(Instant, BridgeStats), started: Instant, now: Instant) -> Self {
		let received = current.received.saturating_sub(previous.received);
		let elapsed = now.saturating_duration_since(*since).as_secs_f64();
		Self {
			rate: if elapsed > 0.0 { received as f64 / elapsed } else { 0.0 },
			received,
			dropped: current.dropped.saturating_sub(previous.dropped),
			idle: Some(now.saturating_duration_since(current.last_received.unwrap_or(started))),
		}
	}

	fn assess(&self) -> (Level, String) {
		match self.idle {
			Some(idle) if idle >= STALE_AFTER => {
				return (Level::Warn, format!("no message for {:.0}s", idle.as_secs_f64()));
			}
			_ => {}
		}
		if self.received > 0 && self.dropped * 2 > self.received {
			return (
				Level::Warn,
				format!("dropped {} of {} messages", self.dropped, self.received),
			);
		}
		(Level::Ok, "ok".to_string())
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn report(received: u64, dropped: u64, idle_secs: u64) -> Report {
		Report {
			rate: 0.0,
			received,
			dropped,
			idle: Some(Duration::from_secs(idle_secs)),
		}
	}

	#[test]
	fn assesses_bridges() {
		assert_eq!(report(30, 0, 0).assess().0, Level::Ok);
		assert_eq!(report(30, 15, 0).assess().0, Level::Ok);
		assert_eq!(
			report(30, 16, 0).assess(),
			(Level::Warn, "dropped 16 of 30 messages".into())
		);
		assert_eq!(report(0, 0, 4).assess().0, Level::Ok);
		assert_eq!(report(0, 0, 5).assess(), (Level::Warn, "no message for 5s".into()));
	}

	#[test]
	fn computes_rates_since_the_previous_report() {
		let started = Instant::now();
		let previous = BridgeStats {
			received: 10,
			dropped: 1,
			..Default::default()
		};
		let current = BridgeStats {
			received: 30,
			dropped: 3,
			last_received: Some(started + Duration::from_secs(2)),
			..Default::default()
		};
		let report = Report::new(
			&current,
			&(started, previous),
			started,
			started + Duration::from_secs(2),
		);
		assert_eq!(
			report,
			Report {
				rate: 10.0,
				received: 20,
				dropped: 2,
				idle: Some(Duration::ZERO),
			}
		);
		// A bridge that never received anything is idle since it started.
		let report = Report::new(
			&BridgeStats::default(),
			&(started, BridgeStats::default()),
			started,
			started + Duration::from_secs(6),
		);
		assert_eq!(report.idle, Some(Duration::from_secs(6)));
	}

	/// The message matches `diagnostic_msgs/msg/DiagnosticArray`, so ROS accepts it.
	#[test]
	fn publishes_diagnostic_arrays() {
		let ctx = r2r::Context::create().unwrap();
		let mut node = r2r::Node::create(ctx, "moqros_diagnostics_test", "").unwrap();
		let mut diagnostics = Diagnostics::new(&mut node).expect("diagnostic_msgs bindings (IDL_PACKAGE_FILTER)");
		let (sender, stats) = watch::channel(BridgeStats::default());
		diagnostics.running("/camera", "ROS -> MoQ video", stats);
		sender.send_modify(|stats| stats.received = 3);
		diagnostics.running("imu", "MoQ -> ROS data", watch::channel(BridgeStats::default()).1);
		diagnostics.restarting("imu");
		diagnostics.publish().unwrap();
	}
}
