//! `--topic` and `--broadcast` specs, resolved against the global flags.

use std::collections::HashSet;
use std::str::FromStr;

use anyhow::{bail, ensure};
use clap::ValueEnum;
use moqros::ros::ROS_IMAGE_TYPE;

use crate::args::Format;

/// `NAME[,key=value...]` split into the name and its options.
type Spec<'a> = (&'a str, Vec<(&'a str, &'a str)>);

/// Split `NAME[,key=value...]` into the name and its options, rejecting keys not in `keys`.
fn split_spec<'a>(spec: &'a str, keys: &[&str]) -> Result<Spec<'a>, String> {
	let mut parts = spec.split(',');
	let name = parts.next().unwrap_or_default().trim();
	if name.is_empty() {
		return Err(format!("{spec:?} doesn't start with a name"));
	}
	let mut options: Vec<(&str, &str)> = Vec::new();
	for part in parts {
		let (key, value) = part
			.split_once('=')
			.ok_or_else(|| format!("expected key=value, got {part:?}"))?;
		let (key, value) = (key.trim(), value.trim());
		if !keys.contains(&key) {
			return Err(format!("unknown option {key:?} (expected one of: {})", keys.join(", ")));
		}
		if options.iter().any(|(k, _)| *k == key) {
			return Err(format!("option {key:?} given twice"));
		}
		options.push((key, value));
	}
	Ok((name, options))
}

/// A topic for `moqros-pub`: `TOPIC[,format=…][,type=…][,mode=…][,broadcast=…]`.
///
/// Options left out fall back to the matching command-line flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopicSpec {
	pub topic: String,
	pub format: Option<Format>,
	pub ros_type: Option<String>,
	pub mode: Option<moqros::DataMode>,
	pub broadcast: Option<String>,
}

impl FromStr for TopicSpec {
	type Err = String;

	fn from_str(spec: &str) -> Result<Self, String> {
		let (topic, options) = split_spec(spec, &["format", "type", "mode", "broadcast"])?;
		let mut parsed = Self {
			topic: topic.to_string(),
			format: None,
			ros_type: None,
			mode: None,
			broadcast: None,
		};
		for (key, value) in options {
			match key {
				"format" => parsed.format = Some(Format::from_str(value, true)?),
				"type" => parsed.ros_type = Some(value.to_string()),
				"mode" => parsed.mode = Some(value.parse().map_err(|err| format!("{err}"))?),
				_ => parsed.broadcast = Some(value.to_string()),
			}
		}
		Ok(parsed)
	}
}

/// What `moqros-pub` does with one topic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PubTopic {
	pub topic: String,
	pub broadcast: String,
	pub format: Format,
	pub mode: moqros::DataMode,
	/// Looked up in the ROS graph when `None` (unless the format is video).
	pub ros_type: Option<String>,
}

/// Fill in each topic's options from the flags: `format` and `mode` apply to every topic,
/// while `ros_type` and `broadcast` name one topic, so they need exactly one.
pub fn pub_topics(
	specs: Vec<TopicSpec>,
	format: Format,
	mode: moqros::DataMode,
	ros_type: Option<String>,
	broadcast: Option<String>,
) -> anyhow::Result<Vec<PubTopic>> {
	ensure!(!specs.is_empty(), "no topic to publish");
	ensure!(
		specs.len() == 1 || (ros_type.is_none() && broadcast.is_none()),
		"--type and --broadcast need a single --topic; give several topics `type=` and `broadcast=` options instead"
	);
	let mut broadcasts = HashSet::new();
	specs
		.into_iter()
		.map(|spec| {
			let topic = PubTopic {
				broadcast: spec
					.broadcast
					.or(broadcast.clone())
					.unwrap_or_else(|| moqros::broadcast_name_for_topic(&spec.topic)),
				format: spec.format.unwrap_or(format),
				mode: spec.mode.unwrap_or(mode),
				ros_type: spec.ros_type.or(ros_type.clone()),
				topic: spec.topic,
			};
			if topic.format == Format::Video
				&& let Some(ros_type) = topic.ros_type.as_deref().filter(|t| *t != ROS_IMAGE_TYPE)
			{
				bail!(
					"{}: video needs {ROS_IMAGE_TYPE} messages, not {ros_type}; use format=json or cdr",
					topic.topic
				);
			}
			ensure!(
				broadcasts.insert(topic.broadcast.clone()),
				"several topics publish the broadcast {:?}",
				topic.broadcast
			);
			Ok(topic)
		})
		.collect()
}

/// A broadcast for `moqros-sub`: `BROADCAST[,topic=…][,type=…][,frame_id=…]`.
///
/// Options left out fall back to the matching command-line flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BroadcastSpec {
	pub broadcast: String,
	pub topic: Option<String>,
	pub ros_type: Option<String>,
	pub frame_id: Option<String>,
}

impl FromStr for BroadcastSpec {
	type Err = String;

	fn from_str(spec: &str) -> Result<Self, String> {
		let (broadcast, options) = split_spec(spec, &["topic", "type", "frame_id"])?;
		let mut parsed = Self {
			broadcast: broadcast.to_string(),
			topic: None,
			ros_type: None,
			frame_id: None,
		};
		for (key, value) in options {
			let value = Some(value.to_string());
			match key {
				"topic" => parsed.topic = value,
				"type" => parsed.ros_type = value,
				_ => parsed.frame_id = value,
			}
		}
		Ok(parsed)
	}
}

/// What `moqros-sub` does with one broadcast.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubBroadcast {
	pub broadcast: String,
	pub topic: String,
	/// Taken from the catalog when `None`.
	pub ros_type: Option<String>,
	pub frame_id: String,
}

/// Topic of a lone broadcast that neither `topic=` nor `--topic` names.
pub const DEFAULT_SUB_TOPIC: &str = "/moqros/image";

/// Fill in each broadcast's options from the flags. `topic` and `ros_type` name one
/// broadcast, so they need exactly one. Without either `topic=` or `--topic`, a lone
/// broadcast goes to [`DEFAULT_SUB_TOPIC`], and each of several to `/moqros/<broadcast>`.
pub fn sub_broadcasts(
	specs: Vec<BroadcastSpec>,
	topic: Option<String>,
	ros_type: Option<String>,
	frame_id: &str,
) -> anyhow::Result<Vec<SubBroadcast>> {
	ensure!(!specs.is_empty(), "no broadcast to subscribe to");
	let single = specs.len() == 1;
	ensure!(
		single || (topic.is_none() && ros_type.is_none()),
		"--topic and --type need a single --broadcast; give several broadcasts `topic=` and `type=` options instead"
	);
	let mut topics = HashSet::new();
	specs
		.into_iter()
		.map(|spec| {
			let broadcast = SubBroadcast {
				topic: spec.topic.or(topic.clone()).unwrap_or_else(|| match single {
					true => DEFAULT_SUB_TOPIC.to_string(),
					false => format!("/moqros/{}", spec.broadcast.trim_matches('/')),
				}),
				ros_type: spec.ros_type.or(ros_type.clone()),
				frame_id: spec.frame_id.unwrap_or_else(|| frame_id.to_string()),
				broadcast: spec.broadcast,
			};
			ensure!(
				topics.insert(broadcast.topic.clone()),
				"several broadcasts publish on {}",
				broadcast.topic
			);
			Ok(broadcast)
		})
		.collect()
}

/// Refuse a bridge that would feed its own output back in: a broadcast or a ROS topic that
/// both directions use. Every message would go round the loop forever.
pub fn check_no_loop(topics: &[PubTopic], broadcasts: &[SubBroadcast]) -> anyhow::Result<()> {
	let name = |name: &str| name.trim_matches('/').to_string();
	for topic in topics {
		for broadcast in broadcasts {
			ensure!(
				name(&topic.broadcast) != name(&broadcast.broadcast),
				"the broadcast {:?} is both published (from {}) and subscribed to; that would loop",
				topic.broadcast,
				topic.topic
			);
			ensure!(
				name(&topic.topic) != name(&broadcast.topic),
				"the topic {} is both published to the relay and republished from {:?}; that would loop",
				topic.topic,
				broadcast.broadcast
			);
		}
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn parses_topic_specs() {
		let spec: TopicSpec = "/scan, format=cdr ,type=sensor_msgs/msg/LaserScan,mode=stream,broadcast=lidar"
			.parse()
			.unwrap();
		assert_eq!(
			spec,
			TopicSpec {
				topic: "/scan".into(),
				format: Some(Format::Cdr),
				ros_type: Some("sensor_msgs/msg/LaserScan".into()),
				mode: Some(moqros::DataMode::Stream),
				broadcast: Some("lidar".into()),
			}
		);
		assert_eq!("/image_raw".parse::<TopicSpec>().unwrap().format, None);
		for bad in [
			"",
			",format=json",
			"/a,json",
			"/a,codec=vp8",
			"/a,format=xml",
			"/a,mode=x",
			"/a,type=x,type=y",
		] {
			assert!(bad.parse::<TopicSpec>().is_err(), "{bad:?}");
		}

		let spec: BroadcastSpec = "camera/image_raw,topic=/out,frame_id=cam,type=sensor_msgs/msg/Image"
			.parse()
			.unwrap();
		assert_eq!(
			(spec.broadcast.as_str(), spec.topic.as_deref(), spec.frame_id.as_deref()),
			("camera/image_raw", Some("/out"), Some("cam"))
		);
		assert!("chatter,format=json".parse::<BroadcastSpec>().is_err());
	}

	#[test]
	fn resolves_pub_topics() {
		let specs = |specs: &[&str]| {
			specs
				.iter()
				.map(|s| s.parse::<TopicSpec>().unwrap())
				.collect::<Vec<_>>()
		};
		let snapshot = moqros::DataMode::Snapshot;

		let topics = pub_topics(
			specs(&[
				"/camera/image_raw",
				"/chatter,format=json",
				"/log,mode=stream,broadcast=logs",
			]),
			Format::Video,
			snapshot,
			None,
			None,
		)
		.unwrap();
		let summary: Vec<_> = topics
			.iter()
			.map(|t| (t.broadcast.as_str(), t.format, t.mode))
			.collect();
		assert_eq!(
			summary,
			[
				("camera/image_raw", Format::Video, snapshot),
				("chatter", Format::Json, snapshot),
				("logs", Format::Video, moqros::DataMode::Stream),
			]
		);

		// Flags that name one topic.
		let one = pub_topics(
			specs(&["/a"]),
			Format::Cdr,
			snapshot,
			Some("t/msg/T".into()),
			Some("b".into()),
		)
		.unwrap();
		assert_eq!(
			(one[0].broadcast.as_str(), one[0].ros_type.as_deref()),
			("b", Some("t/msg/T"))
		);
		assert!(
			pub_topics(
				specs(&["/a", "/b"]),
				Format::Json,
				snapshot,
				Some("t/msg/T".into()),
				None
			)
			.is_err()
		);
		assert!(pub_topics(specs(&["/a", "/b"]), Format::Json, snapshot, None, Some("b".into())).is_err());

		assert!(pub_topics(specs(&["/a", "a,format=json"]), Format::Video, snapshot, None, None).is_err());
		assert!(
			pub_topics(
				specs(&["/a,type=std_msgs/msg/String"]),
				Format::Video,
				snapshot,
				None,
				None
			)
			.is_err()
		);
		assert!(
			pub_topics(
				specs(&["/a,type=sensor_msgs/msg/Image"]),
				Format::Video,
				snapshot,
				None,
				None
			)
			.is_ok()
		);
		assert!(pub_topics(Vec::new(), Format::Video, snapshot, None, None).is_err());
	}

	#[test]
	fn refuses_loops() {
		let topics = |specs: &[&str]| {
			let specs = specs.iter().map(|s| s.parse::<TopicSpec>().unwrap()).collect();
			pub_topics(specs, Format::Json, moqros::DataMode::default(), None, None).unwrap()
		};
		let broadcasts = |specs: &[&str]| {
			let specs = specs.iter().map(|s| s.parse::<BroadcastSpec>().unwrap()).collect();
			sub_broadcasts(specs, None, None, "camera").unwrap()
		};

		let odom = topics(&["/odom,broadcast=robot/odom"]);
		assert!(check_no_loop(&odom, &broadcasts(&["robot/cmd_vel,topic=/cmd_vel"])).is_ok());
		// The same broadcast both ways, however it's spelled.
		assert!(check_no_loop(&odom, &broadcasts(&["/robot/odom/,topic=/other"])).is_err());
		// The same topic both ways.
		assert!(check_no_loop(&odom, &broadcasts(&["robot/cmd_vel,topic=/odom"])).is_err());
		assert!(check_no_loop(&odom, &[]).is_ok());
	}

	#[test]
	fn resolves_sub_broadcasts() {
		let specs = |specs: &[&str]| {
			specs
				.iter()
				.map(|s| s.parse::<BroadcastSpec>().unwrap())
				.collect::<Vec<_>>()
		};
		let topics = |broadcasts: Vec<SubBroadcast>| broadcasts.into_iter().map(|b| b.topic).collect::<Vec<_>>();

		assert_eq!(
			topics(sub_broadcasts(specs(&["image_raw"]), None, None, "camera").unwrap()),
			[DEFAULT_SUB_TOPIC]
		);
		assert_eq!(
			topics(sub_broadcasts(specs(&["image_raw"]), Some("/x".into()), None, "camera").unwrap()),
			["/x"]
		);
		let broadcasts = sub_broadcasts(
			specs(&[
				"sample/image_raw,topic=/moqros/image",
				"chatter",
				"/front/imu/,frame_id=imu",
			]),
			None,
			None,
			"camera",
		)
		.unwrap();
		assert_eq!(broadcasts[2].frame_id, "imu");
		assert_eq!(
			topics(broadcasts),
			["/moqros/image", "/moqros/chatter", "/moqros/front/imu"]
		);

		assert!(sub_broadcasts(specs(&["a", "b"]), Some("/x".into()), None, "camera").is_err());
		assert!(sub_broadcasts(specs(&["a", "b"]), None, Some("t/msg/T".into()), "camera").is_err());
		assert!(sub_broadcasts(specs(&["a,topic=/x", "b,topic=/x"]), None, None, "camera").is_err());
	}
}
