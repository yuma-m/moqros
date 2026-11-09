//! Subscribe to MoQ broadcasts and republish each as a ROS 2 topic: video decoded into
//! images, or data tracks as the message type they carry.

use clap::Parser;
use moqros_cli::{Bridges, BroadcastSpec, DiagnosticsArgs, Discovery, QosArgs, Relay, RelayArgs, init_logging};

#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
	/// Broadcast to subscribe to; repeat for several. Options after the name override
	/// the flags below for that broadcast: `chatter,topic=/chatter,type=std_msgs/msg/String,frame_id=cam,max_height=480`.
	/// Defaults to `image_raw` without `--broadcast-prefix`.
	#[arg(long)]
	broadcast: Vec<BroadcastSpec>,

	/// Also republish every broadcast announced under this prefix (whole path segments), on
	/// the topic `--topic-template` names; repeat for several. Broadcast names come from whoever
	/// announces them: ones that don't make plain topic names are skipped.
	#[arg(long)]
	broadcast_prefix: Vec<String>,

	/// Topic of a broadcast found by `--broadcast-prefix`, `{name}` standing for its name.
	#[arg(long, default_value = moqros_cli::DEFAULT_TOPIC_TEMPLATE)]
	topic_template: String,

	/// Topic to publish on: decoded `rgb8` images for video, or the data track's messages.
	/// Single broadcast only; defaults to `/moqros/image` for one broadcast, and to
	/// `/moqros/<broadcast>` for each of several.
	#[arg(long)]
	topic: Option<String>,

	/// `header.frame_id` of the republished images.
	#[arg(long, default_value = "camera")]
	frame_id: String,

	/// Pick the largest video rendition at most this tall (the smallest if none is), instead of
	/// the largest.
	#[arg(long)]
	max_height: Option<u32>,

	/// Message type to republish a data track as; defaults to the type its catalog names.
	/// Single broadcast only.
	#[arg(id = "type", long = "type")]
	ros_type: Option<String>,

	#[command(flatten)]
	relay: RelayArgs,

	#[command(flatten)]
	qos: QosArgs,

	#[command(flatten)]
	diagnostics: DiagnosticsArgs,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
	init_logging();
	let (cli, node) = moqros_cli::init::<Cli>("moqros_sub")?;
	let discovery = match cli.broadcast_prefix.is_empty() {
		true => None,
		false => Some(Discovery::new(
			cli.broadcast_prefix,
			cli.topic_template,
			cli.frame_id.clone(),
			cli.max_height,
		)?),
	};
	let specs = match (cli.broadcast.is_empty(), &discovery) {
		(true, None) => vec!["image_raw".parse().expect("a valid spec")],
		_ => cli.broadcast,
	};
	let broadcasts = match specs.is_empty() {
		true => Vec::new(),
		false => moqros_cli::sub_broadcasts(specs, cli.topic, cli.ros_type, &cli.frame_id, cli.max_height)?,
	};

	cli.relay.warn_if_insecure();
	let subscriber = moqros::Subscriber::connect(&cli.relay.config())?;
	tracing::info!(url = %cli.relay.redacted_url(), broadcasts = broadcasts.len(), "connecting to the relay");
	let bridges = Bridges {
		topics: Vec::new(),
		broadcasts,
		video: Default::default(),
		qos: cli.qos.profile(),
		diagnostics_period: cli.diagnostics.period(),
		discovery,
	};
	moqros_cli::run_bridges(node, Relay::subscriber(subscriber), bridges).await
}
