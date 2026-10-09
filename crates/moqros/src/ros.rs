//! ROS 2 integration built on [r2r](https://docs.rs/r2r).
//!
//! The bridges only register subscriptions/publishers on a node you own.
//! [`spawn_ros_to_moq`] needs that node to be spun (e.g. `node.spin_once(..)` in a
//! blocking task) for messages to flow; [`spawn_moq_to_ros`] only publishes, which
//! r2r does without spinning.

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::StreamExt;
use r2r::builtin_interfaces::msg::Time;
use r2r::std_msgs::msg::Header;
use tokio::task::JoinHandle;

use crate::{Error, Image, ImagePublisher, Result, Subscriber};

pub use r2r;
pub use r2r::QosProfile;

/// The ROS message type handled by moqros.
pub type RosImage = r2r::sensor_msgs::msg::Image;

/// Convert a `sensor_msgs/Image` into an [`Image`] without copying the pixel buffer.
pub fn image_from_ros(msg: RosImage) -> Result<Image> {
	if msg.is_bigendian != 0 && msg.encoding.contains("16") {
		return Err(Error::UnsupportedEncoding(format!("{} (big endian)", msg.encoding)));
	}
	let format = msg.encoding.parse()?;
	let stamp = &msg.header.stamp;
	let timestamp = Duration::new(stamp.sec.max(0) as u64, stamp.nanosec);
	Image::with_step(msg.width, msg.height, format, msg.step, msg.data, timestamp)
}

/// Convert an [`Image`] into a `sensor_msgs/Image` with the given header.
pub fn image_to_ros(image: &Image, header: Header) -> RosImage {
	RosImage {
		header,
		height: image.height,
		width: image.width,
		encoding: image.format.as_ros_encoding().to_string(),
		is_bigendian: 0,
		step: image.step,
		data: image.data.to_vec(),
	}
}

/// A `builtin_interfaces/Time` for the current wall clock.
pub fn now_stamp() -> Time {
	let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
	Time {
		sec: now.as_secs() as i32,
		nanosec: now.subsec_nanos(),
	}
}

/// Statistics returned when a bridge stops.
#[derive(Debug, Default, Clone, Copy)]
pub struct BridgeStats {
	pub received: u64,
	pub sent: u64,
	pub dropped: u64,
}

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
			let mut warned = false;
			while let Some(msg) = slot.take() {
				let image = match image_from_ros(msg) {
					Ok(image) => image,
					Err(err) => {
						if !warned {
							tracing::warn!(%err, "skipping image");
							warned = true;
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

/// Republish a decoded MoQ image broadcast onto a ROS topic as `rgb8`.
///
/// Waits for `broadcast` to be announced and resubscribes whenever it ends, so the
/// publisher side may restart freely. The header stamp is the local receive time,
/// since MoQ timestamps are relative to the start of the broadcast.
pub fn spawn_moq_to_ros(
	node: &mut r2r::Node,
	topic: &str,
	qos: QosProfile,
	frame_id: &str,
	subscriber: Subscriber,
	broadcast: &str,
) -> Result<JoinHandle<Result<()>>> {
	let publisher = node.create_publisher::<RosImage>(topic, qos).map_err(ros_error)?;
	let frame_id = frame_id.to_string();
	let broadcast = broadcast.to_string();

	Ok(tokio::spawn(async move {
		loop {
			let mut images = match subscriber.subscribe_images(&broadcast).await {
				Ok(images) => images,
				Err(err) => {
					tracing::warn!(%err, broadcast, "subscribe failed, retrying");
					tokio::time::sleep(Duration::from_secs(1)).await;
					continue;
				}
			};

			// r2r publishers are Send but not Sync, so never hold `&publisher` across an await.
			let mut count = 0u64;
			let mut meter = RateMeter::new("MoQ -> ROS");
			let ended = loop {
				match images.next_image().await {
					Ok(Some(image)) => {
						let header = Header {
							stamp: now_stamp(),
							frame_id: frame_id.clone(),
						};
						publisher.publish(&image_to_ros(&image, header)).map_err(ros_error)?;
						count += 1;
						meter.tick();
					}
					Ok(None) => break None,
					Err(err) => break Some(err),
				}
			};

			match ended {
				None => tracing::info!(broadcast, count, "broadcast ended, waiting for it to return"),
				Some(err) => tracing::warn!(%err, broadcast, count, "subscription failed, resubscribing"),
			}
		}
	}))
}

/// Logs the message rate of a bridge every few seconds.
struct RateMeter {
	label: &'static str,
	count: u64,
	since: std::time::Instant,
}

impl RateMeter {
	const PERIOD: Duration = Duration::from_secs(10);

	fn new(label: &'static str) -> Self {
		Self {
			label,
			count: 0,
			since: std::time::Instant::now(),
		}
	}

	fn tick(&mut self) {
		self.count += 1;
		let elapsed = self.since.elapsed();
		if elapsed >= Self::PERIOD {
			let fps = self.count as f64 / elapsed.as_secs_f64();
			tracing::info!(bridge = self.label, fps = format!("{fps:.1}"), "image rate");
			self.count = 0;
			self.since = std::time::Instant::now();
		}
	}
}

fn ros_error(err: r2r::Error) -> Error {
	Error::Ros(err.to_string())
}

#[cfg(test)]
mod tests {
	use std::thread;

	use super::*;
	use crate::PixelFormat;

	fn ros_image(encoding: &str, width: u32, height: u32, step: u32) -> RosImage {
		RosImage {
			header: Header {
				stamp: Time { sec: 12, nanosec: 34 },
				frame_id: "camera".into(),
			},
			height,
			width,
			encoding: encoding.into(),
			is_bigendian: 0,
			step,
			data: (0..step * height).map(|i| i as u8).collect(),
		}
	}

	#[test]
	fn converts_ros_images() {
		let image = image_from_ros(ros_image("bgr8", 2, 2, 8)).unwrap();
		assert_eq!(
			(image.width, image.height, image.format, image.step),
			(2, 2, PixelFormat::Bgr8, 8)
		);
		assert_eq!(image.timestamp, Duration::new(12, 34));
		assert_eq!(image.data.len(), 16);

		let header = Header {
			stamp: Time { sec: 1, nanosec: 2 },
			frame_id: "out".into(),
		};
		let back = image_to_ros(&image, header.clone());
		assert_eq!(back.header, header);
		assert_eq!(back.encoding, "bgr8");
		assert_eq!((back.width, back.height, back.step, back.is_bigendian), (2, 2, 8, 0));
		assert_eq!(back.data, ros_image("bgr8", 2, 2, 8).data);
	}

	#[test]
	fn clamps_negative_stamps() {
		let mut msg = ros_image("mono8", 2, 2, 2);
		msg.header.stamp.sec = -5;
		assert_eq!(image_from_ros(msg).unwrap().timestamp, Duration::new(0, 34));
	}

	#[test]
	fn rejects_unsupported_ros_images() {
		assert!(matches!(
			image_from_ros(ros_image("16UC1", 2, 2, 4)),
			Err(Error::UnsupportedEncoding(_))
		));
		let mut big_endian = ros_image("mono16", 2, 2, 4);
		big_endian.is_bigendian = 1;
		assert!(matches!(image_from_ros(big_endian), Err(Error::UnsupportedEncoding(e)) if e.contains("big endian")));
		assert!(matches!(
			image_from_ros(ros_image("rgb8", 4, 2, 8)),
			Err(Error::InvalidImage(_))
		));
	}

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
