//! # moqros
//!
//! Stream ROS 2 topics over [Media over QUIC](https://moq.dev).
//!
//! Images are converted to I420, encoded (VP8 via libvpx by default, or H.264 via
//! Cisco's OpenH264 binary with the `h264` feature) and published as
//! [hang](https://docs.rs/hang) broadcasts, the format played by the `@moq/watch`
//! web component. The reverse direction (MoQ -> decoded images) is also provided.
//!
//! Messages of any other type travel as JSON or CDR payloads in a hang data track
//! ([`Publisher::create_data_broadcast`], [`Subscriber::subscribe_data`]).
//!
//! The core API is ROS-independent:
//!
//! ```no_run
//! # async fn run(rgb_bytes: Vec<u8>, capture_time: std::time::Duration) -> moqros::Result<()> {
//! let config = moqros::ClientConfig::new("http://localhost:4443/anon".parse().unwrap());
//!
//! let publisher = moqros::Publisher::connect(&config)?;
//! let mut camera = publisher.create_image_broadcast("camera", Default::default())?;
//! camera.publish(&moqros::Image::new(640, 480, moqros::PixelFormat::Rgb8, rgb_bytes, capture_time)?)?;
//!
//! let subscriber = moqros::Subscriber::connect(&config)?;
//! let mut images = subscriber.subscribe_images("camera").await?;
//! while let Some(image) = images.next_image().await? {
//!     // image is rgb8
//! }
//! # Ok(()) }
//! ```
//!
//! Data broadcasts carry opaque payloads:
//!
//! ```no_run
//! # async fn run(publisher: moqros::Publisher, subscriber: moqros::Subscriber) -> moqros::Result<()> {
//! let settings = moqros::DataSettings::new(moqros::DataFormat::Json);
//! let mut status = publisher.create_data_broadcast("status", settings)?;
//! status.publish(r#"{"battery": 0.82}"#)?;
//!
//! let mut messages = subscriber.subscribe_data("status").await?;
//! while let Some(message) = messages.next_message().await? {
//!     // message.payload is one JSON document
//! }
//! # Ok(()) }
//! ```
//!
//! [`Subscriber::subscribe`] returns either kind, depending on what the broadcast's catalog
//! offers. Enable the `ros` feature for [`ros`] bridges built on r2r.
//!
//! ## Features
//!
//! | Feature | What |
//! | --- | --- |
//! | `vp8` (default) | VP8 through the system libvpx, real-time CBR. Needs libvpx, pkg-config, and libclang at build time. |
//! | `h264` | H.264 (Constrained Baseline, `avc3`) through Cisco's prebuilt OpenH264 binary, loaded at runtime. |
//! | `ros` | ROS 2 bridges built on r2r. Needs a sourced ROS 2 environment and libclang at build time. |
//!
//! Pick the codec with [`EncoderSettings::codec`]. A subscriber picks the best rendition in
//! the catalog that this build can decode.
//!
//! The `h264` feature never compiles OpenH264: Cisco only covers the MPEG LA royalties for
//! binaries downloaded from Cisco. Give the binary's path in `OPENH264_LIBRARY` or with
//! `set_openh264_library`; its SHA-256 must match a known Cisco release.
//! `scripts/fetch_openh264.sh` in the repository downloads the right one for the platform.
//!
//! ## Wire format
//!
//! - **Video broadcasts:** the catalog (`catalog.json`) lists one video track, [`VIDEO_TRACK`],
//!   and is republished whenever the resolution changes. Each frame goes in the hang "legacy"
//!   container (a timestamp plus the codec payload). Each keyframe starts a new MoQ group, so
//!   [`EncoderSettings::keyframe_interval`] bounds how long a new viewer waits for the first
//!   picture. Subscribers always jump to the newest group.
//! - **Pixel formats:** see [`PixelFormat`]. An odd trailing row or column is cropped,
//!   because 4:2:0 video needs even dimensions.
//! - **Timestamps:** capture times become a µs timeline that starts at 0 for each broadcast.
//! - **Data broadcasts:** the catalog lists one track, [`DATA_TRACK`], in its `json` section
//!   ([`DataFormat::Json`]) or its `binary` section ([`DataFormat::Cdr`], MIME type
//!   [`CDR_MIME`]), with the [`DataMode`] and the ROS message type (`rosType`). Each MoQ frame
//!   is one message payload, without a container.

// On docs.rs, label feature-gated items with the feature they need.
#![cfg_attr(docsrs, feature(doc_cfg))]

mod client;
mod clock;
mod codec;
mod convert;
mod data;
mod error;
mod image;
mod publisher;
mod subscriber;

#[cfg(feature = "ros")]
pub mod ros;

pub use client::{ClientConfig, broadcast_name_for_topic, connect_both};
#[cfg(feature = "h264")]
pub use codec::set_openh264_library;
pub use codec::{Codec, DEFAULT_MAX_PIXELS, Decoder, EncodedFrame, Encoder, EncoderSettings};
pub use convert::{I420, Planes, yuv_to_rgb8};
pub use data::{CDR_MIME, DATA_TRACK, DataFormat, DataMessage, DataMode, DataPublisher, DataSettings, DataSubscriber};
pub use error::{Error, Result};
pub use image::{Image, PixelFormat};
pub use publisher::{ImagePublisher, Publisher, VIDEO_TRACK};
pub use subscriber::{ImageSubscriber, Subscriber, Subscription, VideoFrame};

/// Re-exported so applications can name hang/moq types without version skew.
pub use hang;
pub use url::Url;
