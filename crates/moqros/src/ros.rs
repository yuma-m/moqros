//! ROS 2 integration built on [r2r](https://docs.rs/r2r).
//!
//! The bridges only register subscriptions/publishers on a node you own.
//! [`spawn_ros_to_moq`] and [`spawn_ros_to_moq_data`] need that node to be spun (e.g.
//! `node.spin_once(..)` in a blocking task) for messages to flow; [`spawn_moq_to_ros`]
//! only publishes, which r2r does without spinning.
//!
//! ```no_run
//! use moqros::ros::{QosProfile, r2r, spawn_ros_to_moq};
//!
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! let ctx = r2r::Context::create()?;
//! let mut node = r2r::Node::create(ctx, "camera_streamer", "")?;
//!
//! let publisher = moqros::Publisher::connect(&moqros::ClientConfig::new(
//!     "http://localhost:4443/anon".parse()?,
//! ))?;
//! let broadcast = publisher.create_image_broadcast("camera", moqros::EncoderSettings::default())?;
//! let bridge = spawn_ros_to_moq(&mut node, "/camera/image_raw", QosProfile::sensor_data(), broadcast)?;
//!
//! tokio::task::spawn_blocking(move || loop {
//!     node.spin_once(std::time::Duration::from_millis(50));
//! });
//! bridge.await??;
//! # Ok(()) }
//! ```
//!
//! Any other message type goes through a data broadcast:
//!
//! ```no_run
//! use moqros::ros::{QosProfile, spawn_ros_to_moq_data, wait_for_topic_type};
//!
//! # async fn run(mut node: moqros::ros::r2r::Node, publisher: moqros::Publisher) -> moqros::Result<()> {
//! let settings = moqros::DataSettings {
//!     format: moqros::DataFormat::Json,
//!     mode: moqros::DataMode::Snapshot,
//!     ros_type: Some(wait_for_topic_type(&node, "/imu").await?),
//! };
//! let broadcast = publisher.create_data_broadcast("imu", settings)?;
//! let bridge = spawn_ros_to_moq_data(&mut node, "/imu", QosProfile::sensor_data(), broadcast)?;
//! # Ok(()) }
//! ```
//!
//! [`spawn_moq_to_ros`] goes the other way for both kinds of broadcast.
//!
//! Data bridges handle message types at runtime through r2r's untyped API, so they
//! support the message types r2r generated bindings for when it was built.

use std::pin::Pin;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use futures::{Stream, StreamExt};
use r2r::builtin_interfaces::msg::Time;
use r2r::std_msgs::msg::Header;
use tokio::task::JoinHandle;

use crate::{
	DataFormat, DataPublisher, DataSettings, DataSubscriber, Error, Image, ImagePublisher, ImageSubscriber, Result,
	Subscriber, Subscription,
};

pub use r2r;
pub use r2r::QosProfile;

/// The ROS message type moqros encodes as video.
pub type RosImage = r2r::sensor_msgs::msg::Image;

/// Type name of [`RosImage`], as ROS graph queries report it.
pub const ROS_IMAGE_TYPE: &str = "sensor_msgs/msg/Image";

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

/// Forward a ROS topic of any message type into a MoQ data broadcast.
///
/// The broadcast's [`DataSettings`] pick the serialization and
/// must name the ROS type (see [`wait_for_topic_type`]). Messages are forwarded as
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
		let mut warned = false;
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
					if !warned {
						tracing::warn!(%err, "skipping message");
						warned = true;
					}
				}
			}
		}
		Ok(stats)
	}))
}

/// The message type of `topic` in the ROS graph, or `None` if nothing publishes it yet.
///
/// Relative names resolve against the node's namespace; remappings aren't applied.
/// Fails if publishers disagree on the type.
pub fn topic_type(node: &r2r::Node, topic: &str) -> Result<Option<String>> {
	let namespace = node.namespace().map_err(ros_error)?;
	let node_name = node.fully_qualified_name().map_err(ros_error)?;
	let topic = resolve_topic(&namespace, &node_name, topic);
	let topics = node.get_topic_names_and_types().map_err(ros_error)?;
	match topics.get(&topic).map(Vec::as_slice) {
		None | Some([]) => Ok(None),
		Some([ros_type]) => Ok(Some(ros_type.clone())),
		Some(types) => Err(Error::Ros(format!("{topic} has several types: {}", types.join(", ")))),
	}
}

/// Wait until `topic` shows up in the ROS graph and return its message type (see [`topic_type`]).
pub async fn wait_for_topic_type(node: &r2r::Node, topic: &str) -> Result<String> {
	let mut logged = false;
	loop {
		if let Some(ros_type) = topic_type(node, topic)? {
			return Ok(ros_type);
		}
		if !logged {
			tracing::info!(topic, "waiting for the topic to appear to learn its type");
			logged = true;
		}
		tokio::time::sleep(Duration::from_millis(500)).await;
	}
}

/// Expand a relative or private (`~`) topic name the way rcl does, minus remapping.
fn resolve_topic(namespace: &str, node_name: &str, topic: &str) -> String {
	let join = |base: &str, rest: &str| format!("{}/{}", base.trim_end_matches('/'), rest.trim_start_matches('/'));
	if topic.starts_with('/') {
		topic.to_string()
	} else if let Some(rest) = topic.strip_prefix('~') {
		join(node_name, rest)
	} else {
		join(namespace, topic)
	}
}

/// Republish a MoQ broadcast onto a ROS topic, whatever it carries: video decoded into
/// `rgb8` images, or the messages of a data track.
///
/// Waits for the first subscription to learn what to publish, so this resolves once
/// `broadcast` is announced. A data track's ROS type comes from its catalog, or from
/// `ros_type` when given. The task resubscribes whenever the broadcast ends, so the
/// publisher side may restart freely, but fails if it comes back carrying something
/// else (video instead of data, or another format or message type).
///
/// Images are stamped with the local receive time, since MoQ timestamps are relative
/// to the start of the broadcast.
///
/// To wait for several broadcasts at once, use [`MoqToRos::subscribe`] and then
/// [`MoqToRos::spawn`]: only the latter needs the node.
pub async fn spawn_moq_to_ros(
	node: &mut r2r::Node,
	topic: &str,
	qos: QosProfile,
	frame_id: &str,
	subscriber: Subscriber,
	broadcast: &str,
	ros_type: Option<&str>,
) -> Result<JoinHandle<Result<()>>> {
	MoqToRos::subscribe(subscriber, broadcast)
		.await
		.spawn(node, topic, qos, frame_id, ros_type)
}

/// A MoQ broadcast that is subscribed to and ready to be republished on ROS.
pub struct MoqToRos {
	subscriber: Subscriber,
	broadcast: String,
	first: Subscription,
}

impl MoqToRos {
	/// Subscribe to `broadcast`, retrying until it is announced and readable.
	pub async fn subscribe(subscriber: Subscriber, broadcast: &str) -> Self {
		let first = subscribe(&subscriber, broadcast).await;
		Self {
			subscriber,
			broadcast: broadcast.to_string(),
			first,
		}
	}

	/// Broadcast name.
	pub fn broadcast(&self) -> &str {
		&self.broadcast
	}

	/// The data track's settings, or `None` if the broadcast carries video.
	pub fn data_settings(&self) -> Option<&crate::DataSettings> {
		match &self.first {
			Subscription::Images(_) => None,
			Subscription::Data(data) => Some(data.settings()),
		}
	}

	/// Create the ROS publisher on `topic` and start republishing; see [`spawn_moq_to_ros`].
	pub fn spawn(
		self,
		node: &mut r2r::Node,
		topic: &str,
		qos: QosProfile,
		frame_id: &str,
		ros_type: Option<&str>,
	) -> Result<JoinHandle<Result<()>>> {
		let output = match &self.first {
			Subscription::Images(_) => RosOutput::Images {
				publisher: node.create_publisher::<RosImage>(topic, qos).map_err(ros_error)?,
				frame_id: frame_id.to_string(),
			},
			Subscription::Data(data) => {
				let settings = data.settings();
				let ros_type = ros_type
					.map(String::from)
					.or_else(|| settings.ros_type.clone())
					.ok_or_else(|| Error::MissingRosType(self.broadcast.clone()))?;
				RosOutput::Data {
					publisher: node
						.create_publisher_untyped(topic, &ros_type, qos)
						.map_err(ros_error)?,
					catalog: settings.clone(),
				}
			}
		};
		Ok(tokio::spawn(republish(
			self.subscriber,
			self.broadcast,
			self.first,
			output,
		)))
	}
}

/// Subscribe to `broadcast`, retrying until it is announced and readable.
async fn subscribe(subscriber: &Subscriber, broadcast: &str) -> Subscription {
	loop {
		match subscriber.subscribe(broadcast).await {
			Ok(subscription) => return subscription,
			Err(err) => {
				tracing::warn!(%err, broadcast, "subscribe failed, retrying");
				tokio::time::sleep(Duration::from_secs(1)).await;
			}
		}
	}
}

/// Where [`republish`] sends what it receives.
///
/// r2r publishers are Send but not Sync, so this is only ever borrowed mutably:
/// a shared borrow held across an await would make the task non-Send.
enum RosOutput {
	Images {
		publisher: r2r::Publisher<RosImage>,
		frame_id: String,
	},
	Data {
		publisher: r2r::PublisherUntyped,
		/// What the first catalog announced; the publisher's type may override its `ros_type`.
		catalog: DataSettings,
	},
}

/// Feed `output` from `broadcast` forever, starting with `first` and resubscribing
/// whenever the broadcast ends.
async fn republish(
	subscriber: Subscriber,
	broadcast: String,
	first: Subscription,
	mut output: RosOutput,
) -> Result<()> {
	let mut first = Some(first);
	loop {
		let subscription = match first.take() {
			Some(subscription) => subscription,
			None => subscribe(&subscriber, &broadcast).await,
		};

		let mut count = 0u64;
		let ended = match (subscription, &mut output) {
			(Subscription::Images(images), RosOutput::Images { publisher, frame_id }) => {
				republish_images(images, publisher, frame_id, &mut count).await?
			}
			(Subscription::Data(data), RosOutput::Data { publisher, catalog }) => {
				if let Some(changed) = data_changed(catalog, data.settings()) {
					return Err(Error::Ros(format!("broadcast {broadcast} changed {changed}")));
				}
				republish_data(data, publisher, catalog.format, &mut count).await
			}
			(Subscription::Images(_), RosOutput::Data { .. }) => {
				return Err(Error::Ros(format!("broadcast {broadcast} changed from data to video")));
			}
			(Subscription::Data(_), RosOutput::Images { .. }) => {
				return Err(Error::NoVideo(broadcast));
			}
		};

		match ended {
			None => tracing::info!(broadcast, count, "broadcast ended, waiting for it to return"),
			Some(err) => tracing::warn!(%err, broadcast, count, "subscription failed, resubscribing"),
		}
	}
}

/// How a resubscribed data track differs from the `first` one in ways the topic can't
/// follow, if it does. The mode doesn't matter for republishing.
fn data_changed(first: &DataSettings, now: &DataSettings) -> Option<String> {
	let name = |settings: &DataSettings| {
		format!(
			"{} {}",
			settings.format,
			settings.ros_type.as_deref().unwrap_or("(untyped)")
		)
	};
	(first.format != now.format || first.ros_type != now.ros_type)
		.then(|| format!("from {} to {}", name(first), name(now)))
}

/// Publish decoded images until the subscription ends, returning why it did.
/// Fails only if ROS rejects a message.
async fn republish_images(
	mut images: ImageSubscriber,
	publisher: &mut r2r::Publisher<RosImage>,
	frame_id: &str,
	count: &mut u64,
) -> Result<Option<Error>> {
	let mut meter = RateMeter::new("MoQ -> ROS");
	loop {
		match images.next_image().await {
			Ok(Some(image)) => {
				let header = Header {
					stamp: now_stamp(),
					frame_id: frame_id.to_string(),
				};
				publisher.publish(&image_to_ros(&image, header)).map_err(ros_error)?;
				*count += 1;
				meter.tick();
			}
			Ok(None) => return Ok(None),
			Err(err) => return Ok(Some(err)),
		}
	}
}

/// Publish data messages until the subscription ends, returning why it did.
///
/// Messages ROS rejects are skipped: a malformed payload says nothing about the next.
async fn republish_data(
	mut data: DataSubscriber,
	publisher: &mut r2r::PublisherUntyped,
	format: DataFormat,
	count: &mut u64,
) -> Option<Error> {
	let mut meter = RateMeter::new("MoQ -> ROS");
	let mut warned = false;
	loop {
		let message = match data.next_message().await {
			Ok(Some(message)) => message,
			Ok(None) => return None,
			Err(err) => return Some(err),
		};
		let published = match format {
			DataFormat::Json => serde_json::from_slice(&message.payload)
				.map_err(json_error)
				.and_then(|value| publisher.publish(value).map_err(ros_error)),
			DataFormat::Cdr => publisher.publish_raw(&message.payload).map_err(ros_error),
		};
		match published {
			Ok(()) => {
				*count += 1;
				meter.tick();
			}
			Err(err) if !warned => {
				tracing::warn!(%err, "skipping message");
				warned = true;
			}
			Err(_) => {}
		}
	}
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
			let rate = self.count as f64 / elapsed.as_secs_f64();
			tracing::info!(bridge = self.label, rate = format!("{rate:.1}/s"), "message rate");
			self.count = 0;
			self.since = std::time::Instant::now();
		}
	}
}

fn ros_error(err: r2r::Error) -> Error {
	Error::Ros(err.to_string())
}

fn json_error(err: serde_json::Error) -> Error {
	Error::Json(err.to_string())
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
	fn detects_data_track_changes() {
		let settings = |format, mode, ros_type: &str| DataSettings {
			format,
			mode,
			ros_type: Some(ros_type.into()),
		};
		let first = settings(DataFormat::Json, crate::DataMode::Snapshot, "std_msgs/msg/Header");
		assert_eq!(data_changed(&first, &first), None);
		// The mode doesn't change what the topic publishes.
		let stream = settings(DataFormat::Json, crate::DataMode::Stream, "std_msgs/msg/Header");
		assert_eq!(data_changed(&first, &stream), None);
		let cdr = settings(DataFormat::Cdr, crate::DataMode::Snapshot, "std_msgs/msg/Header");
		assert_eq!(
			data_changed(&first, &cdr).as_deref(),
			Some("from json std_msgs/msg/Header to cdr std_msgs/msg/Header")
		);
		let string = settings(DataFormat::Json, crate::DataMode::Snapshot, "std_msgs/msg/String");
		assert!(data_changed(&first, &string).is_some());
	}

	#[test]
	fn resolves_topic_names() {
		for (namespace, topic, resolved) in [
			("/", "/imu", "/imu"),
			("/", "imu", "/imu"),
			("/robot", "imu", "/robot/imu"),
			("/robot", "/imu", "/imu"),
			("/robot", "~/state", "/robot/moqros_pub/state"),
		] {
			let node = format!("{}/moqros_pub", namespace.trim_end_matches('/'));
			assert_eq!(resolve_topic(namespace, &node, topic), resolved, "{namespace} {topic}");
		}
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
