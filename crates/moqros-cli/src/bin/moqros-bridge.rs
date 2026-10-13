//! Bridge both ways in one node: publish ROS 2 topics as MoQ broadcasts (like
//! `moqros-pub`), and republish MoQ broadcasts as ROS 2 topics (like `moqros-sub`).
//! E.g. a robot that streams its camera and takes velocity commands from a browser.

use anyhow::ensure;
use clap::Parser;
use moqros_cli::{Bridges, BroadcastSpec, EncoderArgs, Format, QosArgs, Relay, RelayArgs, TopicSpec, init_logging};

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
	let topics = match cli.topic.is_empty() {
		true => Vec::new(),
		false => moqros_cli::pub_topics(cli.topic, cli.format, cli.mode, None, None)?,
	};
	let broadcasts = match cli.broadcast.is_empty() {
		true => Vec::new(),
		false => moqros_cli::sub_broadcasts(cli.broadcast, None, None, &cli.frame_id)?,
	};
	moqros_cli::check_no_loop(&topics, &broadcasts)?;

	cli.relay.warn_if_insecure();
	// One session carries both directions.
	let (publisher, subscriber) = moqros::connect_both(&cli.relay.config())?;
	tracing::info!(
		url = %cli.relay.redacted_url(),
		topics = topics.len(),
		broadcasts = broadcasts.len(),
		"connecting to the relay"
	);
	let bridges = Bridges {
		topics,
		broadcasts,
		encoder: cli.encoder.settings(),
		qos: cli.qos.profile(),
	};
	moqros_cli::run_bridges(node, Relay::both(publisher, subscriber), bridges).await
}
