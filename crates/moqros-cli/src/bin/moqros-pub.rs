//! Subscribe to a ROS 2 `sensor_msgs/Image` topic and publish it as a MoQ broadcast.

use std::time::Duration;

use anyhow::Context;
use clap::Parser;
use moqros::ros::spawn_ros_to_moq;
use moqros_cli::{QosArgs, RelayArgs, init_logging, spawn_spinner};

#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
	/// Image topic to read.
	#[arg(long, default_value = "/image_raw")]
	topic: String,

	/// Broadcast name; defaults to the topic without its leading slash.
	#[arg(long)]
	broadcast: Option<String>,

	/// Video codec (`vp8`, or `h264` when built with the `h264` feature and
	/// OPENH264_LIBRARY points at Cisco's OpenH264 binary).
	#[arg(long, default_value_t = moqros::Codec::default())]
	codec: moqros::Codec,

	/// Target bitrate in bits per second.
	#[arg(long, default_value_t = 2_000_000)]
	bitrate: u32,

	/// Expected maximum input frame rate.
	#[arg(long, default_value_t = 30.0)]
	fps: f32,

	/// Maximum keyframe (MoQ group) interval in milliseconds.
	#[arg(long, default_value_t = 2000)]
	keyframe_interval_ms: u64,

	#[command(flatten)]
	relay: RelayArgs,

	#[command(flatten)]
	qos: QosArgs,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
	init_logging();
	let (cli, mut node) = moqros_cli::init::<Cli>("moqros_pub")?;
	let broadcast = cli
		.broadcast
		.clone()
		.unwrap_or_else(|| moqros::broadcast_name_for_topic(&cli.topic));

	let publisher = moqros::Publisher::connect(&cli.relay.config())?;
	let settings = moqros::EncoderSettings {
		codec: cli.codec,
		bitrate: cli.bitrate,
		max_fps: cli.fps,
		keyframe_interval: Duration::from_millis(cli.keyframe_interval_ms),
		..Default::default()
	};
	let images = publisher.create_image_broadcast(&broadcast, settings)?;
	let bridge = spawn_ros_to_moq(&mut node, &cli.topic, cli.qos.profile(), images)?;
	let mut spinner = spawn_spinner(node);

	tracing::info!(topic = cli.topic, broadcast, url = %cli.relay.url, "bridging ROS -> MoQ");

	tokio::select! {
		res = bridge => {
			let stats = res??;
			tracing::info!(?stats, "topic stream ended");
		}
		res = publisher.closed() => res.context("relay connection closed")?,
		err = spinner.failed() => return Err(err),
		_ = tokio::signal::ctrl_c() => tracing::info!("interrupted"),
	}
	Ok(())
}
