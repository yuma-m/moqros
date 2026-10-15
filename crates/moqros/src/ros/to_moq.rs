//! ROS topics -> MoQ broadcasts.

use std::pin::Pin;
use std::sync::{Arc, Condvar, Mutex};

use bytes::Bytes;
use futures::{Stream, StreamExt};
use tokio::task::JoinHandle;

use super::meter::{RateMeter, WarnOnce};
use super::{BridgeStats, QosProfile, RosImage, image_from_ros, json_error, ros_error};
use crate::{DataFormat, DataPublisher, Error, ImagePublisher, Result};

/// Forward a ROS image topic into a MoQ broadcast.
///
/// Encoding runs on a dedicated blocking thread. When it can't keep up, a
/// pending image is replaced by the newest one so latency stays bounded.
/// The returned task resolves when the topic stream or the broadcast ends.
pub fn spawn_ros_to_moq(
	node: &mut r2r::Node,
	topic: &str,
	qos: QosProfile,
	mut broadcast: ImagePublisher,
) -> Result<JoinHandle<Result<BridgeStats>>> {
	let mut stream = node.subscribe::<RosImage>(topic, qos).map_err(ros_error)?;
	let topic = topic.to_string();

	let slot = Arc::new(LatestSlot::default());

	let encoder = tokio::task::spawn_blocking({
		let slot = slot.clone();
		move || -> Result<u64> {
			let mut sent = 0;
			let mut warned = WarnOnce::default();
			while let Some(msg) = slot.take() {
				let image = match image_from_ros(msg) {
					Ok(image) => image,
					Err(err) => {
						if warned.first() {
							tracing::warn!(%err, "skipping image");
						}
						continue;
					}
				};
				if broadcast.publish(&image)? {
					sent += 1;
				}
			}
			Ok(sent)
		}
	});

	Ok(tokio::spawn(async move {
		// Closing on drop also releases the encoder thread when this task is cancelled
		// (e.g. at runtime shutdown); otherwise the runtime would wait on it forever.
		let slot = CloseOnDrop(slot);
		let mut stats = BridgeStats::default();
		let mut meter = RateMeter::new("ROS -> MoQ");
		while let Some(msg) = stream.next().await {
			stats.received += 1;
			meter.tick();
			if slot.0.put(msg) {
				stats.dropped += 1;
				tracing::debug!(topic, "encoder busy, dropped an older image");
			}
			if encoder.is_finished() {
				break;
			}
		}
		drop(slot);
		stats.sent = encoder.await.expect("encoder thread panicked")?;
		Ok(stats)
	}))
}

/// A single-element mailbox where a new value replaces the pending one.
#[derive(Default)]
struct LatestSlot<T> {
	state: Mutex<(Option<T>, bool)>,
	ready: Condvar,
}

impl<T> LatestSlot<T> {
	/// Store `value`, returning true if it replaced one that was never taken.
	fn put(&self, value: T) -> bool {
		let mut state = self.state.lock().unwrap();
		let replaced = state.0.replace(value).is_some();
		self.ready.notify_one();
		replaced
	}

	/// Block until a value is available; `None` once closed and drained.
	fn take(&self) -> Option<T> {
		let mut state = self.state.lock().unwrap();
		loop {
			if let Some(value) = state.0.take() {
				return Some(value);
			}
			if state.1 {
				return None;
			}
			state = self.ready.wait(state).unwrap();
		}
	}

	fn close(&self) {
		self.state.lock().unwrap().1 = true;
		self.ready.notify_all();
	}
}

/// Closes the slot when dropped.
struct CloseOnDrop<T>(Arc<LatestSlot<T>>);

impl<T> Drop for CloseOnDrop<T> {
	fn drop(&mut self) {
		self.0.close();
	}
}

/// Forward a ROS topic of any message type into a MoQ data broadcast.
///
/// The broadcast's [`DataSettings`](crate::DataSettings) pick the serialization and
/// must name the ROS type (see [`wait_for_topic_type`](super::wait_for_topic_type)). Messages are forwarded as
/// they arrive. The returned task resolves when the topic stream ends.
pub fn spawn_ros_to_moq_data(
	node: &mut r2r::Node,
	topic: &str,
	qos: QosProfile,
	mut broadcast: DataPublisher,
) -> Result<JoinHandle<Result<BridgeStats>>> {
	let settings = broadcast.settings();
	let ros_type = settings
		.ros_type
		.clone()
		.ok_or_else(|| Error::MissingRosType(broadcast.name().to_string()))?;
	let mut messages: Pin<Box<dyn Stream<Item = Result<Bytes>> + Send>> = match settings.format {
		DataFormat::Json => Box::pin(
			node.subscribe_untyped(topic, &ros_type, qos)
				.map_err(ros_error)?
				.map(|msg| Ok(serde_json::to_vec(&msg.map_err(ros_error)?).map_err(json_error)?.into())),
		),
		DataFormat::Cdr => Box::pin(
			node.subscribe_raw(topic, &ros_type, qos)
				.map_err(ros_error)?
				.map(|msg| Ok(msg.into())),
		),
	};

	Ok(tokio::spawn(async move {
		let mut stats = BridgeStats::default();
		let mut meter = RateMeter::new("ROS -> MoQ");
		let mut warned = WarnOnce::default();
		while let Some(msg) = messages.next().await {
			stats.received += 1;
			meter.tick();
			match msg {
				Ok(payload) => {
					broadcast.publish(payload)?;
					stats.sent += 1;
				}
				Err(err) => {
					stats.dropped += 1;
					if warned.first() {
						tracing::warn!(%err, "skipping message");
					}
				}
			}
		}
		Ok(stats)
	}))
}

#[cfg(test)]
mod tests {
	use std::thread;
	use std::time::Duration;

	use super::*;

	#[test]
	fn latest_slot_keeps_only_the_newest_value() {
		let slot = LatestSlot::default();
		assert!(!slot.put(1));
		assert!(slot.put(2));
		assert_eq!(slot.take(), Some(2));
		assert!(!slot.put(3));
		slot.close();
		// Closing still hands out the pending value first.
		assert_eq!(slot.take(), Some(3));
		assert_eq!(slot.take(), None);
	}

	#[test]
	fn latest_slot_take_waits_for_put_or_close() {
		let slot = Arc::new(LatestSlot::default());
		let taker = thread::spawn({
			let slot = slot.clone();
			move || (slot.take(), slot.take())
		});
		thread::sleep(Duration::from_millis(50));
		slot.put(7);
		thread::sleep(Duration::from_millis(50));
		drop(CloseOnDrop(slot));
		assert_eq!(taker.join().unwrap(), (Some(7), None));
	}
}
