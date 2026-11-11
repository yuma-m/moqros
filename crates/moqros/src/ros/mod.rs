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
//! let settings = moqros::DataSettings::new(moqros::DataFormat::Json)
//!     .with_ros_type(wait_for_topic_type(&node, "/imu").await?);
//! let broadcast = publisher.create_data_broadcast("imu", settings)?;
//! let bridge = spawn_ros_to_moq_data(&mut node, "/imu", QosProfile::sensor_data(), broadcast)?;
//! # Ok(()) }
//! ```
//!
//! [`spawn_moq_to_ros`] goes the other way for both kinds of broadcast.
//!
//! Data bridges handle message types at runtime through r2r's untyped API, so they
//! support the message types r2r generated bindings for when it was built.

mod bridge;
mod convert;
mod from_moq;
mod graph;
mod meter;
mod to_moq;

pub use bridge::{BitrateControl, Bridge, BridgeStats, Teardown};
#[cfg(feature = "compressed")]
pub use convert::compressed_image_from_ros;
pub use convert::{image_from_ros, image_into_ros, image_to_ros, now_stamp, stamp};
pub use from_moq::{MoqToRos, RosTarget, StampSource, spawn_moq_to_ros};
pub use graph::{topic_type, wait_for_topic_type};
pub use r2r;
pub use r2r::QosProfile;
#[cfg(feature = "compressed")]
pub use to_moq::spawn_ros_to_moq_compressed;
pub use to_moq::{spawn_ros_to_moq, spawn_ros_to_moq_data};

/// The ROS message type moqros encodes as video.
pub type RosImage = r2r::sensor_msgs::msg::Image;

/// Type name of [`RosImage`], as ROS graph queries report it.
pub const ROS_IMAGE_TYPE: &str = "sensor_msgs/msg/Image";

/// The compressed image message moqros decodes and encodes as video (feature `compressed`).
pub type RosCompressedImage = r2r::sensor_msgs::msg::CompressedImage;

/// Type name of [`RosCompressedImage`], as ROS graph queries report it.
pub const ROS_COMPRESSED_IMAGE_TYPE: &str = "sensor_msgs/msg/CompressedImage";
