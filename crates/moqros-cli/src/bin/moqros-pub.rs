//! Subscribe to ROS 2 topics and publish each as a MoQ broadcast: images as video, or
//! any message type as JSON or CDR data.

use std::time::Duration;

use anyhow::Context;
use clap::Parser;
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use moqros_cli::{EncoderArgs, Format, QosArgs, RelayArgs, SharedNode, TopicSpec, init_logging, spawn_spinner};

#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
	/// Topic to read; repeat for several. Options after the name override the flags
	/// below for that topic: `/chatter,format=json,type=std_msgs/msg/String,mode=stream,broadcast=chat`.
	#[arg(long, default_value = "/image_raw")]
	topic: Vec<TopicSpec>,

	/// Broadcast name; defaults to the topic without its leading slash. Single topic only.
	#[arg(long)]
	broadcast: Option<String>,

	/// How to send the topics: `video` encodes `sensor_msgs/msg/Image`; `json` and `cdr`
	/// forward any message type as data.
	#[arg(long, value_enum, default_value_t = Format::Video)]
	format: Format,

	/// Message type of the topic (e.g. `sensor_msgs/msg/Imu`); looked up in the ROS graph
	/// if omitted. Single topic only.
	#[arg(id = "type", long = "type")]
	ros_type: Option<String>,

	/// For `json` and `cdr`: `snapshot` sends each message on its own so subscribers skip
	/// to the newest; `stream` delivers every message in order.
	#[arg(long, default_value_t = moqros::DataMode::default())]
	mode: moqros::DataMode,

	#[command(flatten)]
	encoder: EncoderArgs,

	#[command(flatten)]
	relay: RelayArgs,

	#[command(flatten)]
	qos: QosArgs,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
	init_logging();
	let (cli, node) = moqros_cli::init::<Cli>("moqros_pub")?;
	let mut pending = moqros_cli::pub_topics(
		cli.topic.clone(),
		cli.format,
		cli.mode,
		cli.ros_type.clone(),
		cli.broadcast.clone(),
	)?;

	cli.relay.warn_if_insecure();
	let publisher = moqros::Publisher::connect(&cli.relay.config())?;
	tracing::info!(url = %cli.relay.redacted_url(), topics = pending.len(), "connecting to the relay");
	let node = SharedNode::new(node);
	let mut spinner = spawn_spinner(node.clone());
	let mut bridges = FuturesUnordered::new();
	let mut logged = false;
	// Created once, so a signal arriving while the loop body runs isn't missed.
	let shutdown = moqros_cli::shutdown_signal();
	tokio::pin!(shutdown);

	loop {
		// Start every topic whose type is known by now; data topics without a `type`
		// wait until something publishes them.
		let mut waiting = Vec::new();
		for topic in std::mem::take(&mut pending) {
			let topic_name = topic.topic.clone();
			let started = node
				.with(|node| moqros_cli::start_pub_topic(node, &publisher, &topic, &cli.encoder, cli.qos.profile()))?;
			match started {
				Some(bridge) => bridges.push(async move { (topic_name, bridge.await) }),
				None => {
					waiting.push(topic.topic.clone());
					pending.push(topic);
				}
			}
		}
		if !waiting.is_empty() && !logged {
			tracing::info!(topics = ?waiting, "waiting for topics to appear to learn their types");
			logged = true;
		}

		tokio::select! {
			Some((topic, res)) = bridges.next() => {
				let stats = res.context("bridge panicked")??;
				tracing::info!(topic, ?stats, "topic stream ended");
				return Ok(());
			}
			_ = tokio::time::sleep(Duration::from_millis(500)), if !pending.is_empty() => {}
			res = publisher.closed() => return res.context("relay connection closed"),
			err = spinner.failed() => return Err(err),
			res = &mut shutdown => {
				tracing::info!(signal = res?, "shutting down");
				if let Err(err) = publisher.close().await {
					tracing::warn!(%err, "the relay connection didn't close cleanly");
				}
				return Ok(());
			}
		}
	}
}
