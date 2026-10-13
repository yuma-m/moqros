//! Bridge both ways in one node: publish ROS 2 topics as MoQ broadcasts (like
//! `moqros-pub`), and republish MoQ broadcasts as ROS 2 topics (like `moqros-sub`).
//! E.g. a robot that streams its camera and takes velocity commands from a browser.

use std::time::Duration;

use anyhow::{Context, bail, ensure};
use clap::Parser;
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use moqros::ros::MoqToRos;
use moqros_cli::{
	BroadcastSpec, EncoderArgs, Format, QosArgs, RelayArgs, SharedNode, TopicSpec, init_logging, spawn_spinner,
};

#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
	/// ROS topic to publish to the relay (ROS -> MoQ); repeat for several. Options as in
	/// `moqros-pub`: `/odom,format=json,type=nav_msgs/msg/Odometry,mode=stream,broadcast=robot/odom`.
	#[arg(long)]
	topic: Vec<TopicSpec>,

	/// Broadcast to republish on ROS (MoQ -> ROS); repeat for several. Options as in
	/// `moqros-sub`: `robot/cmd_vel,topic=/cmd_vel,type=geometry_msgs/msg/Twist,frame_id=cam`.
	/// The topic defaults to `/moqros/<broadcast>`, or `/moqros/image` for a lone broadcast.
	#[arg(long)]
	broadcast: Vec<BroadcastSpec>,

	/// How to send the topics: `video` encodes `sensor_msgs/msg/Image`; `json` and `cdr`
	/// forward any message type as data.
	#[arg(long, value_enum, default_value_t = Format::Video)]
	format: Format,

	/// For `json` and `cdr` topics: `snapshot` sends each message on its own so subscribers
	/// skip to the newest; `stream` delivers every message in order.
	#[arg(long, default_value_t = moqros::DataMode::default())]
	mode: moqros::DataMode,

	/// `header.frame_id` of the republished images.
	#[arg(long, default_value = "camera")]
	frame_id: String,

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
	let (cli, node) = moqros_cli::init::<Cli>("moqros_bridge")?;
	ensure!(
		!cli.topic.is_empty() || !cli.broadcast.is_empty(),
		"nothing to bridge; give at least one --topic or --broadcast"
	);
	let mut pending = match cli.topic.is_empty() {
		true => Vec::new(),
		false => moqros_cli::pub_topics(cli.topic.clone(), cli.format, cli.mode, None, None)?,
	};
	let broadcasts = match cli.broadcast.is_empty() {
		true => Vec::new(),
		false => moqros_cli::sub_broadcasts(cli.broadcast.clone(), None, None, &cli.frame_id)?,
	};

	cli.relay.warn_if_insecure();
	let config = cli.relay.config();
	let publisher = moqros::Publisher::connect(&config)?;
	let subscriber = moqros::Subscriber::connect(&config)?;
	tracing::info!(
		url = %cli.relay.redacted_url(),
		topics = pending.len(),
		broadcasts = broadcasts.len(),
		"connecting to the relay"
	);
	let node = SharedNode::new(node);
	let mut spinner = spawn_spinner(node.clone());

	// A topic's type depends on what its broadcast carries, so wait for each broadcast
	// (all at once) before creating its publisher.
	let mut subscribing: FuturesUnordered<_> = broadcasts
		.into_iter()
		.map(|broadcast| {
			let subscriber = subscriber.clone();
			async move { (MoqToRos::subscribe(subscriber, &broadcast.broadcast).await, broadcast) }
		})
		.collect();
	// ROS -> MoQ and MoQ -> ROS bridges.
	let mut pub_bridges = FuturesUnordered::new();
	let mut sub_bridges = FuturesUnordered::new();
	let mut logged = false;
	// Created once, so a signal arriving while the loop body runs isn't missed.
	let shutdown = moqros_cli::shutdown_signal();
	tokio::pin!(shutdown);

	loop {
		// Start every topic whose type is known by now; data topics without a `type`
		// wait until something publishes them.
		let mut waiting = Vec::new();
		for topic in std::mem::take(&mut pending) {
			let started = node
				.with(|node| moqros_cli::start_pub_topic(node, &publisher, &topic, &cli.encoder, cli.qos.profile()))?;
			match started {
				Some(bridge) => {
					let topic_name = topic.topic.clone();
					pub_bridges.push(async move { (topic_name, bridge.await) });
				}
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
			Some((ready, broadcast)) = subscribing.next() => {
				let bridge = node.with(|node| moqros_cli::spawn_sub_broadcast(ready, node, &broadcast, cli.qos.profile()));
				match bridge {
					Ok(bridge) => sub_bridges.push(async move { (broadcast.broadcast, bridge.await) }),
					// E.g. a message type this build has no bindings for: the other
					// broadcasts and topics can still go on.
					Err(err) => {
						tracing::error!(%err, broadcast = broadcast.broadcast, topic = broadcast.topic, "skipping broadcast");
						if subscribing.is_empty() && sub_bridges.is_empty() && pub_bridges.is_empty() && pending.is_empty() {
							bail!("nothing left to bridge");
						}
					}
				}
			}
			Some((topic, res)) = pub_bridges.next() => {
				let stats = res.context("bridge panicked")??;
				tracing::info!(topic, ?stats, "topic stream ended");
				return Ok(());
			}
			Some((broadcast, res)) = sub_bridges.next() => {
				res.context("bridge panicked")?.with_context(|| format!("broadcast {broadcast}"))?;
				return Ok(());
			}
			_ = tokio::time::sleep(Duration::from_millis(500)), if !pending.is_empty() => {}
			res = publisher.closed() => return res.context("relay connection closed"),
			// E.g. the relay refused the session: no broadcast would ever be announced.
			res = subscriber.closed() => return res.context("relay connection closed"),
			err = spinner.failed() => return Err(err),
			res = &mut shutdown => {
				tracing::info!(signal = res?, "shutting down");
				let (published, subscribed) = tokio::join!(publisher.close(), subscriber.close());
				for err in [published.err(), subscribed.err()].into_iter().flatten() {
					tracing::warn!(%err, "the relay connection didn't close cleanly");
				}
				return Ok(());
			}
		}
	}
}
