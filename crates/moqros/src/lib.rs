//! # moqros
//!
//! Stream ROS 2 `sensor_msgs/Image` topics over [Media over QUIC](https://moq.dev).
//!
//! Images are converted to I420, encoded (VP8 via libvpx by default, or H.264 via
//! Cisco's OpenH264 binary with the `h264` feature) and published as
//! [hang](https://docs.rs/hang) broadcasts, the format played by the `@moq/watch`
//! web component. The reverse direction (MoQ -> decoded images) is also provided.
//!
//! The core API is ROS-independent:
//!
//! ```no_run
//! # async fn run(images: Vec<moqros::Image>) -> moqros::Result<()> {
//! let config = moqros::ClientConfig::new("http://localhost:4443/anon".parse().unwrap());
//! let publisher = moqros::Publisher::connect(&config)?;
//! let mut camera = publisher.create_image_broadcast("camera", Default::default())?;
//! for image in &images {
//!     camera.publish(image)?;
//! }
//! # Ok(()) }
//! ```
//!
//! Enable the `ros` feature for [`ros`] bridges built on r2r.

mod client;
mod codec;
mod convert;
mod error;
mod image;
mod publisher;
mod subscriber;

#[cfg(feature = "ros")]
pub mod ros;

pub use client::{ClientConfig, broadcast_name_for_topic};
#[cfg(feature = "h264")]
pub use codec::set_openh264_library;
pub use codec::{Codec, Decoder, EncodedFrame, Encoder, EncoderSettings};
pub use convert::{I420, Planes, yuv_to_rgb8};
pub use error::{Error, Result};
pub use image::{Image, PixelFormat};
pub use publisher::{ImagePublisher, Publisher, VIDEO_TRACK};
pub use subscriber::{ImageSubscriber, Subscriber, VideoFrame};

/// Re-exported so applications can name hang/moq types without version skew.
pub use hang;
pub use url::Url;
