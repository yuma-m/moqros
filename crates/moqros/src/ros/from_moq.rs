//! MoQ broadcasts -> ROS topics.

use std::time::Duration;

use r2r::std_msgs::msg::Header;
use tokio::task::JoinHandle;

use super::meter::{RateMeter, WarnOnce};
use super::{QosProfile, RosImage, image_to_ros, now_stamp};
use crate::{DataFormat, DataSettings, DataSubscriber, Error, ImageSubscriber, Result, Subscriber, Subscription};

/// Republish a MoQ broadcast onto a ROS topic, whatever it carries: video decoded into
/// `rgb8` images, or the messages of a data track.
///
/// Waits for the first subscription to learn what to publish, so this resolves once
/// `broadcast` is announced. A data track's ROS type comes from its catalog, or from
/// [`RosTarget::ros_type`] when given. The task resubscribes whenever the broadcast ends, so the
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
	subscriber: Subscriber,
	broadcast: &str,
	target: RosTarget,
) -> Result<JoinHandle<Result<()>>> {
	MoqToRos::subscribe(subscriber, broadcast).await.spawn(node, target)
}

/// The ROS topic [`spawn_moq_to_ros`] republishes a broadcast on.
#[derive(Debug, Clone)]
pub struct RosTarget {
	/// Topic name to publish on.
	pub topic: String,
	/// QoS of the ROS publisher.
	pub qos: QosProfile,
	/// `header.frame_id` of republished images.
	pub frame_id: String,
	/// ROS message type of a data track, overriding the one its catalog announces.
	pub ros_type: Option<String>,
}

impl RosTarget {
	/// Publish on `topic`, with an empty `frame_id` and the catalog's message type.
	pub fn new(topic: impl Into<String>, qos: QosProfile) -> Self {
		Self {
			topic: topic.into(),
			qos,
			frame_id: String::new(),
			ros_type: None,
		}
	}

	/// Set [`RosTarget::frame_id`].
	pub fn with_frame_id(mut self, frame_id: impl Into<String>) -> Self {
		self.frame_id = frame_id.into();
		self
	}

	/// Set [`RosTarget::ros_type`].
	pub fn with_ros_type(mut self, ros_type: impl Into<String>) -> Self {
		self.ros_type = Some(ros_type.into());
		self
	}
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

	/// Create the ROS publisher for `target` and start republishing; see [`spawn_moq_to_ros`].
	pub fn spawn(self, node: &mut r2r::Node, target: RosTarget) -> Result<JoinHandle<Result<()>>> {
		let RosTarget {
			topic,
			qos,
			frame_id,
			ros_type,
		} = target;
		let output = match &self.first {
			Subscription::Images(_) => RosOutput::Images {
				publisher: node.create_publisher::<RosImage>(&topic, qos)?,
				frame_id,
			},
			Subscription::Data(data) => {
				let settings = data.settings();
				let ros_type = ros_type
					.or_else(|| settings.ros_type.clone())
					.ok_or_else(|| Error::MissingRosType(self.broadcast.clone()))?;
				RosOutput::Data {
					publisher: node.create_publisher_untyped(&topic, &ros_type, qos)?,
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
				if let Some((from, to)) = data_changed(catalog, data.settings()) {
					return Err(Error::BroadcastChanged { broadcast, from, to });
				}
				republish_data(data, publisher, catalog.format, &mut count).await
			}
			(Subscription::Images(_), RosOutput::Data { .. }) => {
				return Err(Error::BroadcastChanged {
					broadcast,
					from: "data".into(),
					to: "video".into(),
				});
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
/// follow, as `(from, to)`, if it does. The mode doesn't matter for republishing.
fn data_changed(first: &DataSettings, now: &DataSettings) -> Option<(String, String)> {
	let name = |settings: &DataSettings| {
		format!(
			"{} {}",
			settings.format,
			settings.ros_type.as_deref().unwrap_or("(untyped)")
		)
	};
	(first.format != now.format || first.ros_type != now.ros_type).then(|| (name(first), name(now)))
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
				publisher.publish(&image_to_ros(&image, header))?;
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
	let mut warned = WarnOnce::default();
	loop {
		let message = match data.next_message().await {
			Ok(Some(message)) => message,
			Ok(None) => return None,
			Err(err) => return Some(err),
		};
		let published = match format {
			DataFormat::Json => serde_json::from_slice(&message.payload)
				.map_err(Error::from)
				.and_then(|value| publisher.publish(value).map_err(Error::from)),
			DataFormat::Cdr => publisher.publish_raw(&message.payload).map_err(Error::from),
		};
		match published {
			Ok(()) => {
				*count += 1;
				meter.tick();
			}
			Err(err) => {
				if warned.first() {
					tracing::warn!(%err, "skipping message");
				}
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

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
			data_changed(&first, &cdr),
			Some(("json std_msgs/msg/Header".into(), "cdr std_msgs/msg/Header".into()))
		);
		let string = settings(DataFormat::Json, crate::DataMode::Snapshot, "std_msgs/msg/String");
		assert!(data_changed(&first, &string).is_some());
	}
}
