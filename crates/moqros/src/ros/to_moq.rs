//! ROS topics -> MoQ broadcasts.

use std::pin::Pin;
use std::sync::{Arc, Condvar, Mutex};

use bytes::Bytes;
use futures::{Stream, StreamExt};

#[cfg(feature = "compressed")]
use super::compressed_image_from_ros;
use super::meter::{RateMeter, WarnOnce};
use super::{BitrateControl, Bridge, QosProfile, RosImage, image_from_ros};
use crate::{DataFormat, DataPublisher, Error, ImagePublisher, Result};

/// The ROS topic a ROS -> MoQ bridge reads.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct RosSource {
	/// Topic name to subscribe to.
	pub topic: String,
	/// QoS of the ROS subscription.
	pub qos: QosProfile,
}

impl RosSource {
	/// Subscribe to `topic` with `qos`.
	pub fn new(topic: impl Into<String>, qos: QosProfile) -> Self {
		Self {
			topic: topic.into(),
			qos,
		}
	}
}

/// Forward a ROS image topic into a MoQ broadcast.
///
/// Encoding runs on a dedicated blocking thread. When it can't keep up, a
/// pending image is replaced by the newest one so latency stays bounded.
/// The returned [`Bridge`] resolves when the topic stream or the broadcast ends; its
/// [`bitrate_control`](Bridge::bitrate_control) changes the bitrate while it runs.
pub fn spawn_ros_to_moq(node: &mut r2r::Node, source: RosSource, broadcast: ImagePublisher) -> Result<Bridge> {
	spawn_image_bridge(node, source, broadcast, image_from_ros)
}

/// Forward a ROS `sensor_msgs/CompressedImage` topic (JPEG or PNG) into a MoQ broadcast,
/// decoding each image before encoding it as video (feature `compressed`).
///
/// Decoding runs on the encoding thread; otherwise this works like [`spawn_ros_to_moq`].
#[cfg(feature = "compressed")]
pub fn spawn_ros_to_moq_compressed(
	node: &mut r2r::Node,
	source: RosSource,
	broadcast: ImagePublisher,
) -> Result<Bridge> {
	spawn_image_bridge(node, source, broadcast, compressed_image_from_ros)
}

/// A ROS message carrying an image.
trait ImageMessage: r2r::WrappedTypesupport + Send + 'static {
	/// `header.frame_id`.
	fn frame_id(&self) -> &str;
}

impl ImageMessage for RosImage {
	fn frame_id(&self) -> &str {
		&self.header.frame_id
	}
}

#[cfg(feature = "compressed")]
impl ImageMessage for super::RosCompressedImage {
	fn frame_id(&self) -> &str {
		&self.header.frame_id
	}
}

/// Forward messages of type `M`, converted by `convert`, into a video broadcast.
fn spawn_image_bridge<M>(
	node: &mut r2r::Node,
	source: RosSource,
	mut broadcast: ImagePublisher,
	convert: fn(M) -> Result<crate::Image>,
) -> Result<Bridge>
where
	M: ImageMessage,
{
	let RosSource { topic, qos } = source;
	let mut stream = node.subscribe::<M>(&topic, qos)?;
	let (bitrate_control, mut bitrate) = BitrateControl::new(broadcast.settings().bitrate);

	let bridge = Bridge::spawn(|stats| async move {
		let slot = Arc::new(LatestSlot::<M>::default());
		let encoder = tokio::task::spawn_blocking({
			let slot = slot.clone();
			let stats = stats.clone();
			move || -> Result<()> {
				let mut warned = WarnOnce::default();
				while let Some(msg) = slot.take() {
					if bitrate.has_changed().unwrap_or(false) {
						broadcast.set_bitrate(*bitrate.borrow_and_update());
					}
					broadcast.set_frame_id(msg.frame_id())?;
					let image = match convert(msg) {
						Ok(image) => image,
						Err(err) => {
							stats.dropped();
							if warned.first() {
								tracing::warn!(%err, "skipping image");
							}
							continue;
						}
					};
					if broadcast.publish(&image)? {
						stats.sent();
					}
				}
				Ok(())
			}
		});

		// Closing on drop also releases the encoder thread when this task is cancelled
		// (e.g. at runtime shutdown); otherwise the runtime would wait on it forever.
		let slot = CloseOnDrop(slot);
		let mut meter = RateMeter::new("ROS -> MoQ");
		while let Some(msg) = stream.next().await {
			stats.received();
			meter.tick();
			if slot.0.put(msg) {
				stats.dropped();
				tracing::debug!(topic, "encoder busy, dropped an older image");
			}
			if encoder.is_finished() {
				break;
			}
		}
		drop(slot);
		encoder.await.expect("encoder thread panicked")
	});
	Ok(bridge.with_bitrate_control(bitrate_control))
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
/// they arrive. The returned [`Bridge`] resolves when the topic stream ends.
pub fn spawn_ros_to_moq_data(node: &mut r2r::Node, source: RosSource, mut broadcast: DataPublisher) -> Result<Bridge> {
	let RosSource { topic, qos } = source;
	let topic = topic.as_str();
	let settings = broadcast.settings();
	let ros_type = settings
		.ros_type
		.clone()
		.ok_or_else(|| Error::MissingRosType(broadcast.name().to_string()))?;
	let mut messages: Pin<Box<dyn Stream<Item = Result<Bytes>> + Send>> = match settings.format {
		DataFormat::Json => Box::pin(
			node.subscribe_untyped(topic, &ros_type, qos)?
				.map(|msg| Ok(serde_json::to_vec(&msg?)?.into())),
		),
		DataFormat::Cdr => Box::pin(node.subscribe_raw(topic, &ros_type, qos)?.map(|msg| Ok(msg.into()))),
	};

	Ok(Bridge::spawn(|stats| async move {
		let mut meter = RateMeter::new("ROS -> MoQ");
		let mut warned = WarnOnce::default();
		while let Some(msg) = messages.next().await {
			stats.received();
			meter.tick();
			match msg {
				Ok(payload) => {
					broadcast.publish(payload)?;
					stats.sent();
				}
				Err(err) => {
					stats.dropped();
					if warned.first() {
						tracing::warn!(%err, "skipping message");
					}
				}
			}
		}
		Ok(())
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
