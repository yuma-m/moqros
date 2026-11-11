//! # moqros
//!
//! Stream ROS 2 topics over [Media over QUIC](https://moq.dev).
//!
//! Images are converted to I420, encoded (VP8 via libvpx by default, AV1 via libaom with the
//! `av1` feature, or H.264 via Cisco's OpenH264 binary with the `h264` feature; on a GPU through
//! GStreamer with the `hw` feature) and published as
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
//! let mut camera = publisher.create_image_broadcast("camera", moqros::EncoderSettings::default())?;
//! camera.publish(&moqros::Image::new(640, 480, moqros::PixelFormat::Rgb8, rgb_bytes, capture_time)?)?;
//!
//! let subscriber = moqros::Subscriber::connect(&config)?;
//! let mut images = subscriber.subscribe_images("camera").await?;
//! while let Some(decoded) = images.next_image().await? {
//!     // decoded.image is rgb8, captured at decoded.capture_time
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
//! | `av1` | AV1 (Main profile, 8-bit) through the system libaom in real-time mode. Needs libaom, pkg-config, and libclang at build time. |
//! | `h264` | H.264 (Constrained Baseline, `avc3`) through Cisco's prebuilt OpenH264 binary, loaded at runtime. |
//! | `hw` | Hardware encoders (VA-API, NVENC) through GStreamer, falling back to software. Needs the GStreamer development files at build time. |
//! | `compressed` (default) | Decode JPEG and PNG images ([`Image::from_compressed`], `sensor_msgs/CompressedImage`) with pure-Rust decoders. |
//! | `ros` | ROS 2 bridges built on r2r. Needs a sourced ROS 2 environment and libclang at build time. |
//!
//! Pick the codec with [`EncoderSettings::codec`]. A subscriber picks the best rendition in
//! the catalog that this build can decode.
//!
//! With the `hw` feature, [`EncoderSettings::hardware`] ([`Hardware::Auto`] by default) encodes
//! on the first hardware encoder GStreamer can start for the codec: VA-API (`va` plugin, or the
//! older `vaapi` plugin) on Intel/AMD GPUs, or NVENC (`nvcodec` plugin) on NVIDIA GPUs. When
//! none works, it logs a warning and encodes in software, so the codec's own feature is still
//! required. Decoding always runs in software.
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
//!   picture. Subscribers always jump to the newest group. With
//!   [`ImageBroadcastSettings::renditions`], smaller renditions go into tracks of their own
//!   (`video_<height>p`), all listed in the same catalog; [`ImageOptions`] picks one.
//! - **Pixel formats:** see [`PixelFormat`]. An odd trailing row or column is cropped,
//!   because 4:2:0 video needs even dimensions.
//! - **Timestamps:** capture times become a µs timeline that starts at 0 for each broadcast.
//! - **Data broadcasts:** the catalog lists one track, [`DATA_TRACK`], in its `json` section
//!   ([`DataFormat::Json`]) or its `binary` section ([`DataFormat::Cdr`], MIME type
//!   [`CDR_MIME`]), with the [`DataMode`] and the ROS message type (`rosType`). Each MoQ frame
//!   is one message payload, without a container.
//! - **Capture times:** a root `moqros` section of the catalog can carry `captureOrigin`, the
//!   capture time in µs that frame timestamp 0 maps to, in the publisher's clock (e.g. ROS time,
//!   which may be sim time). It is republished whenever the timeline re-anchors. When the
//!   capture times are wall-clock times since 2020, hang's `clock` carries the same origin.
//!   Data broadcasts published with [`DataPublisher::publish_at`] use it.

// On docs.rs, label feature-gated items with the feature they need.
#![cfg_attr(docsrs, feature(doc_cfg))]
#![warn(missing_docs)]

mod catalog;
mod client;
mod clock;
mod codec;
#[cfg(feature = "compressed")]
mod compressed;
mod convert;
mod data;
mod error;
mod image;
mod names;
mod publisher;
mod subscriber;
mod track;

#[cfg(feature = "ros")]
pub mod ros;

pub use catalog::{CDR_MIME, DATA_TRACK, VIDEO_TRACK};
pub use client::{
	ClientConfig, Session, broadcast_is_under, broadcast_name_for_topic, connect_both, normalize_broadcast_name,
};
#[cfg(feature = "h264")]
pub use codec::set_openh264_library;
pub use codec::{Codec, DEFAULT_MAX_PIXELS, Decoder, EncodedFrame, Encoder, EncoderSettings, Hardware};
pub use convert::{I420, Planes, yuv_to_packed, yuv_to_rgb8};
pub use data::{DataFormat, DataMessage, DataMode, DataPublisher, DataSettings, DataSubscriber};
pub use error::{Error, Result};
pub use image::{Image, PixelFormat};
pub use publisher::{ImageBroadcastSettings, ImagePublisher, Published, Publisher, RenditionSettings};
pub use subscriber::{
	Announcements, DecodedImage, ImageOptions, ImageSubscriber, Subscriber, Subscription, VideoFrame,
};

pub use url::Url;
