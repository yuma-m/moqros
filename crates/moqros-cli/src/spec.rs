//! `--topic` and `--broadcast` specs, resolved against the global flags.

use std::collections::HashSet;
use std::str::FromStr;

use anyhow::{bail, ensure};
use clap::ValueEnum;
use moqros::ros::{QosProfile, ROS_COMPRESSED_IMAGE_TYPE, ROS_IMAGE_TYPE, RosTarget, StampSource};

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
				&& let Some(ros_type) = &topic.ros_type
			{
				check_video_type(&topic.topic, ros_type)?;
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

/// Fail unless `topic` of type `ros_type` can be published as video.
pub(crate) fn check_video_type(topic: &str, ros_type: &str) -> anyhow::Result<()> {
	ensure!(
		[ROS_IMAGE_TYPE, ROS_COMPRESSED_IMAGE_TYPE].contains(&ros_type),
		"{topic}: video needs {ROS_IMAGE_TYPE} or {ROS_COMPRESSED_IMAGE_TYPE} messages, not {ros_type}; use format=json or cdr"
	);
	Ok(())
}

/// A broadcast for `moqros-sub`: `BROADCAST[,topic=…][,type=…][,frame_id=…][,stamp=…][,max_height=…][,encoding=…]`.
///
/// Options left out fall back to the matching command-line flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BroadcastSpec {
	pub broadcast: String,
	pub topic: Option<String>,
	pub ros_type: Option<String>,
	pub frame_id: Option<String>,
	pub stamp: Option<StampSource>,
	pub max_height: Option<u32>,
	pub encoding: Option<moqros::PixelFormat>,
}

impl FromStr for BroadcastSpec {
	type Err = String;

	fn from_str(spec: &str) -> Result<Self, String> {
		let (broadcast, options) = split_spec(spec, &["topic", "type", "frame_id", "stamp", "max_height", "encoding"])?;
		let mut parsed = Self {
			broadcast: broadcast.to_string(),
			topic: None,
			ros_type: None,
			frame_id: None,
			stamp: None,
			max_height: None,
			encoding: None,
		};
		for (key, value) in options {
			match key {
				"stamp" => parsed.stamp = Some(value.parse().map_err(|err| format!("{err}"))?),
				"max_height" => {
					parsed.max_height = Some(
						value
							.parse()
							.map_err(|_| format!("max_height must be a number, not {value:?}"))?,
					);
				}
				"encoding" => parsed.encoding = Some(parse_encoding(value)?),
				"topic" => parsed.topic = Some(value.to_string()),
				"type" => parsed.ros_type = Some(value.to_string()),
				_ => parsed.frame_id = Some(value.to_string()),
			}
		}
		Ok(parsed)
	}
}

/// Parse a pixel encoding images can be republished in: `rgb8`, `bgr8`, `rgba8`, `bgra8`
/// or `mono8`.
pub fn parse_encoding(value: &str) -> Result<moqros::PixelFormat, String> {
	use moqros::PixelFormat::{Bgr8, Bgra8, Mono8, Rgb8, Rgba8};
	match value.parse::<moqros::PixelFormat>() {
		Ok(format @ (Rgb8 | Bgr8 | Rgba8 | Bgra8 | Mono8)) => Ok(format),
		_ => Err(format!(
			"unsupported encoding {value:?} (expected one of: rgb8, bgr8, rgba8, bgra8, mono8)"
		)),
	}
}

/// How a broadcast's video is republished on ROS: from the flags, or a broadcast's options.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepublishOptions {
	/// `header.frame_id` of the images; the publisher's when `None`.
	pub frame_id: Option<String>,
	pub stamp: StampSource,
	/// The tallest video rendition to pick; the largest when `None`.
	pub max_height: Option<u32>,
	/// Pixel encoding of the images.
	pub encoding: moqros::PixelFormat,
}

impl Default for RepublishOptions {
	fn default() -> Self {
		Self {
			frame_id: None,
			stamp: StampSource::default(),
			max_height: None,
			encoding: moqros::PixelFormat::Rgb8,
		}
	}
}

impl RepublishOptions {
	/// These options, with the ones `spec` gives instead.
	fn overridden_by(&self, spec: &BroadcastSpec) -> Self {
		Self {
			frame_id: spec.frame_id.clone().or_else(|| self.frame_id.clone()),
			stamp: spec.stamp.unwrap_or(self.stamp),
			max_height: spec.max_height.or(self.max_height),
			encoding: spec.encoding.unwrap_or(self.encoding),
		}
	}
}

/// What `moqros-sub` does with one broadcast.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubBroadcast {
	pub broadcast: String,
	pub topic: String,
	/// Taken from the catalog when `None`.
	pub ros_type: Option<String>,
	pub options: RepublishOptions,
}

impl SubBroadcast {
	/// Which video rendition to subscribe to.
	pub fn image_options(&self) -> moqros::ImageOptions {
		let mut options = moqros::ImageOptions::default();
		options.max_height = self.options.max_height;
		options
	}

	/// The ROS topic to republish on, with `qos`.
	pub fn target(&self, qos: QosProfile) -> RosTarget {
		let mut target = RosTarget::new(self.topic.clone(), qos)
			.with_stamp(self.options.stamp)
			.with_encoding(self.options.encoding);
		target.frame_id = self.options.frame_id.clone();
		target.ros_type = self.ros_type.clone();
		target
	}
}

/// Topic of a lone broadcast that neither `topic=` nor `--topic` names.
pub const DEFAULT_SUB_TOPIC: &str = "/moqros/image";

/// Fill in each broadcast's options from the flags: `options` apply to every broadcast, while
/// `topic` and `ros_type` name one broadcast, so they need exactly one. Without either `topic=` or `--topic`, a lone
/// broadcast goes to [`DEFAULT_SUB_TOPIC`], and each of several to `/moqros/<broadcast>`.
pub fn sub_broadcasts(
	specs: Vec<BroadcastSpec>,
	topic: Option<String>,
	ros_type: Option<String>,
	options: &RepublishOptions,
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
			let options = options.overridden_by(&spec);
			let broadcast = SubBroadcast {
				topic: spec.topic.or(topic.clone()).unwrap_or_else(|| match single {
					true => DEFAULT_SUB_TOPIC.to_string(),
					false => format!("/moqros/{}", moqros::normalize_broadcast_name(&spec.broadcast)),
				}),
				options,
				ros_type: spec.ros_type.or(ros_type.clone()),
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

/// Broadcasts `moqros-sub` finds by prefix, and republishes on topics named after them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovery {
	/// Broadcast name prefixes, without leading or trailing slashes; whole path segments match.
	pub prefixes: Vec<String>,
	/// The topic of a found broadcast, with `{name}` standing for the broadcast name.
	pub topic_template: String,
	pub options: RepublishOptions,
}

/// Default of [`Discovery::topic_template`].
pub const DEFAULT_TOPIC_TEMPLATE: &str = "/moqros/{name}";

impl Discovery {
	/// Check the template on the way in.
	pub fn new(prefixes: Vec<String>, topic_template: String, options: RepublishOptions) -> anyhow::Result<Self> {
		ensure!(
			topic_template.contains("{name}"),
			"--topic-template {topic_template:?} has no {{name}}"
		);
		let discovery = Self {
			prefixes: prefixes
				.iter()
				.map(|p| moqros::normalize_broadcast_name(p).to_string())
				.collect(),
			topic_template,
			options,
		};
		if let Err(err) = discovery.broadcast("name") {
			bail!("--topic-template {:?}: {err}", discovery.topic_template);
		}
		Ok(discovery)
	}

	/// Whether the broadcast `name` is under one of the prefixes.
	pub fn covers(&self, name: &str) -> bool {
		self.prefixes
			.iter()
			.any(|prefix| moqros::broadcast_is_under(name, prefix))
	}

	/// What to do with the found broadcast `name`, or why it can't be republished. Names come
	/// from whoever announces the broadcast, so the topic they make must be a plain ROS name.
	pub fn broadcast(&self, name: &str) -> Result<SubBroadcast, String> {
		let topic = self
			.topic_template
			.replace("{name}", moqros::normalize_broadcast_name(name));
		if !is_ros_topic(&topic) {
			return Err(format!(
				"{topic:?} isn't a topic name moqros republishes on (letters, digits, `_` and `/` only)"
			));
		}
		Ok(SubBroadcast {
			broadcast: name.to_string(),
			topic,
			ros_type: None,
			options: self.options.clone(),
		})
	}
}

/// Whether `topic` is an absolute ROS topic name of plain tokens: each a letter or `_` followed
/// by letters, digits or `_`, without `__`.
fn is_ros_topic(topic: &str) -> bool {
	let Some(tokens) = topic.strip_prefix('/') else {
		return false;
	};
	let is_token = |token: &str| {
		let mut chars = token.chars();
		chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
			&& chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
			&& !token.contains("__")
	};
	!tokens.is_empty() && tokens.split('/').all(is_token)
}

/// Refuse a bridge that would feed its own output back in: a broadcast or a ROS topic that
/// both directions use. Every message would go round the loop forever.
pub fn check_no_loop(
	topics: &[PubTopic],
	broadcasts: &[SubBroadcast],
	discovery: Option<&Discovery>,
) -> anyhow::Result<()> {
	let name = moqros::normalize_broadcast_name;
	for topic in topics {
		if let Some(discovery) = discovery {
			ensure!(
				!discovery.covers(&topic.broadcast),
				"the broadcast {:?} is published (from {}) and under a --broadcast-prefix; that would loop",
				topic.broadcast,
				topic.topic
			);
		}
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

		let spec: BroadcastSpec = "camera/image_raw,topic=/out,frame_id=cam,type=sensor_msgs/msg/Image,stamp=receive"
			.parse()
			.unwrap();
		assert_eq!(
			(
				spec.broadcast.as_str(),
				spec.topic.as_deref(),
				spec.frame_id.as_deref(),
				spec.stamp
			),
			(
				"camera/image_raw",
				Some("/out"),
				Some("cam"),
				Some(StampSource::Receive)
			)
		);
		assert!("camera,stamp=now".parse::<BroadcastSpec>().is_err());
		assert!("chatter,format=json".parse::<BroadcastSpec>().is_err());
		assert_eq!(
			"cam,max_height=360".parse::<BroadcastSpec>().unwrap().max_height,
			Some(360)
		);
		assert!("cam,max_height=tall".parse::<BroadcastSpec>().is_err());
		assert_eq!(
			"cam,encoding=mono8".parse::<BroadcastSpec>().unwrap().encoding,
			Some(moqros::PixelFormat::Mono8)
		);
		assert!("cam,encoding=yuv422".parse::<BroadcastSpec>().is_err());
		assert!("cam,encoding=16UC1".parse::<BroadcastSpec>().is_err());
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
			sub_broadcasts(specs, None, None, &RepublishOptions::default()).unwrap()
		};

		let odom = topics(&["/odom,broadcast=robot/odom"]);
		assert!(check_no_loop(&odom, &broadcasts(&["robot/cmd_vel,topic=/cmd_vel"]), None).is_ok());
		// The same broadcast both ways, however it's spelled.
		assert!(check_no_loop(&odom, &broadcasts(&["/robot/odom/,topic=/other"]), None).is_err());
		// The same topic both ways.
		assert!(check_no_loop(&odom, &broadcasts(&["robot/cmd_vel,topic=/odom"]), None).is_err());
		assert!(check_no_loop(&odom, &[], None).is_ok());
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
			topics(sub_broadcasts(specs(&["image_raw"]), None, None, &RepublishOptions::default()).unwrap()),
			[DEFAULT_SUB_TOPIC]
		);
		assert_eq!(
			topics(
				sub_broadcasts(
					specs(&["image_raw"]),
					Some("/x".into()),
					None,
					&RepublishOptions::default()
				)
				.unwrap()
			),
			["/x"]
		);
		let broadcasts = sub_broadcasts(
			specs(&[
				"sample/image_raw,topic=/moqros/image",
				"chatter",
				"/front/imu/,frame_id=imu,stamp=receive",
			]),
			None,
			None,
			&RepublishOptions {
				frame_id: Some("camera".into()),
				stamp: StampSource::Source,
				max_height: Some(480),
				encoding: moqros::PixelFormat::Bgr8,
			},
		)
		.unwrap();
		assert_eq!(broadcasts[0].options.frame_id.as_deref(), Some("camera"));
		assert_eq!(broadcasts[0].options.stamp, StampSource::Source);
		assert_eq!(broadcasts[2].options.frame_id.as_deref(), Some("imu"));
		assert_eq!(broadcasts[2].options.stamp, StampSource::Receive);
		assert_eq!(broadcasts[2].options.max_height, Some(480));
		assert_eq!(broadcasts[2].options.encoding, moqros::PixelFormat::Bgr8);
		assert_eq!(
			sub_broadcasts(specs(&["image_raw"]), None, None, &RepublishOptions::default()).unwrap()[0]
				.options
				.frame_id,
			None
		);
		assert_eq!(
			topics(broadcasts),
			["/moqros/image", "/moqros/chatter", "/moqros/front/imu"]
		);

		assert!(
			sub_broadcasts(
				specs(&["a", "b"]),
				Some("/x".into()),
				None,
				&RepublishOptions::default()
			)
			.is_err()
		);
		assert!(
			sub_broadcasts(
				specs(&["a", "b"]),
				None,
				Some("t/msg/T".into()),
				&RepublishOptions::default()
			)
			.is_err()
		);
		assert!(
			sub_broadcasts(
				specs(&["a,topic=/x", "b,topic=/x"]),
				None,
				None,
				&RepublishOptions::default()
			)
			.is_err()
		);
	}

	#[test]
	fn discovers_broadcasts_under_prefixes() {
		let discovery = Discovery::new(
			vec!["robots/".into(), "/drones".into()],
			DEFAULT_TOPIC_TEMPLATE.into(),
			RepublishOptions {
				frame_id: Some("camera".into()),
				stamp: StampSource::Receive,
				max_height: Some(480),
				encoding: moqros::PixelFormat::Mono8,
			},
		)
		.unwrap();
		assert!(discovery.covers("robots/r1/camera"));
		assert!(discovery.covers("drones"));
		assert!(!discovery.covers("robots2/camera"));
		assert!(!discovery.covers("cars/robots"));

		let found = discovery.broadcast("robots/r1/camera").unwrap();
		assert_eq!(found.topic, "/moqros/robots/r1/camera");
		assert_eq!(found.options.max_height, Some(480));
		assert_eq!(found.options.encoding, moqros::PixelFormat::Mono8);
		assert_eq!(
			(found.options.frame_id.as_deref(), found.options.stamp),
			(Some("camera"), StampSource::Receive)
		);
		// Names from the network that don't make plain topic names are refused.
		for bad in [
			"robots/r-1/camera",
			"robots/1/camera",
			"robots//camera",
			"robots/a b",
			"robots/__x",
			"robots/{x}",
		] {
			assert!(discovery.broadcast(bad).is_err(), "{bad}");
		}

		assert!(Discovery::new(vec![], "/remote".into(), RepublishOptions::default()).is_err());
		assert!(Discovery::new(vec![], "remote/{name}".into(), RepublishOptions::default()).is_err());
		let custom = Discovery::new(vec![], "/remote/{name}/image".into(), RepublishOptions::default()).unwrap();
		assert_eq!(custom.broadcast("r1").unwrap().topic, "/remote/r1/image");
	}

	#[test]
	fn refuses_to_discover_its_own_broadcasts() {
		let topics = pub_topics(
			vec!["/robots/me/camera".parse().unwrap()],
			Format::Video,
			moqros::DataMode::Snapshot,
			None,
			None,
		)
		.unwrap();
		let discovery = |prefix: &str| {
			Discovery::new(
				vec![prefix.into()],
				DEFAULT_TOPIC_TEMPLATE.into(),
				RepublishOptions::default(),
			)
			.unwrap()
		};
		assert!(check_no_loop(&topics, &[], Some(&discovery("robots"))).is_err());
		assert!(check_no_loop(&topics, &[], Some(&discovery("drones"))).is_ok());
	}
}
