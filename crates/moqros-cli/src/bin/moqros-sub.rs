//! Subscribe to MoQ broadcasts and republish each as a ROS 2 topic: video decoded into
//! images, or data tracks as the message type they carry.

use clap::Parser;
use moqros_cli::{Bridges, BroadcastSpec, QosArgs, Relay, RelayArgs, init_logging};

#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
	/// Broadcast to subscribe to; repeat for several. Options after the name override
	/// the flags below for that broadcast: `chatter,topic=/chatter,type=std_msgs/msg/String,frame_id=cam`.
	#[arg(long, default_value = "image_raw")]
	broadcast: Vec<BroadcastSpec>,

	/// Topic to publish on: decoded `rgb8` images for video, or the data track's messages.
	/// Single broadcast only; defaults to `/moqros/image` for one broadcast, and to
	/// `/moqros/<broadcast>` for each of several.
	#[arg(long)]
	topic: Option<String>,

	/// `header.frame_id` of the republished images.
	#[arg(long, default_value = "camera")]
	frame_id: String,

	/// Message type to republish a data track as; defaults to the type its catalog names.
	/// Single broadcast only.
	#[arg(id = "type", long = "type")]
	ros_type: Option<String>,

	#[command(flatten)]
	relay: RelayArgs,

	#[command(flatten)]
	qos: QosArgs,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
	init_logging();
	let (cli, node) = moqros_cli::init::<Cli>("moqros_sub")?;
	let broadcasts = moqros_cli::sub_broadcasts(cli.broadcast, cli.topic, cli.ros_type, &cli.frame_id)?;

	cli.relay.warn_if_insecure();
	let subscriber = moqros::Subscriber::connect(&cli.relay.config())?;
	tracing::info!(url = %cli.relay.redacted_url(), broadcasts = broadcasts.len(), "connecting to the relay");
	let bridges = Bridges {
		topics: Vec::new(),
		broadcasts,
		encoder: Default::default(),
		qos: cli.qos.profile(),
	};
	moqros_cli::run_bridges(node, Relay::subscriber(subscriber), bridges).await
}
