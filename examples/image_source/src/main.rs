//! Sample ROS 2 node: play a video file (decoded by ffmpeg) or a directory of
//! images as a `sensor_msgs/Image` topic at a fixed frame rate, looping forever.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::time::Duration;

use anyhow::{Context, bail};
use clap::{Parser, ValueEnum};
use moqros::ros::{QosProfile, RosImage, image_to_ros, now_stamp, r2r};
use moqros::{Image, PixelFormat};
use moqros_cli::init_logging;

#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
	/// A video file (anything ffmpeg can read) or a directory of PNG/JPEG frames.
	input: PathBuf,

	/// Topic to publish on.
	#[arg(long, default_value = "/image_raw")]
	topic: String,

	/// Frames per second.
	#[arg(long, default_value_t = 30.0)]
	fps: f64,

	/// Pixel encoding of the published messages.
	#[arg(long, value_enum, default_value_t = Encoding::Bgr8)]
	encoding: Encoding,

	/// Scale video input to this width (keeps aspect ratio). Ignored for image directories.
	#[arg(long)]
	width: Option<u32>,

	/// `header.frame_id` of the published images.
	#[arg(long, default_value = "camera")]
	frame_id: String,

	/// Stop after one pass instead of looping.
	#[arg(long)]
	once: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Encoding {
	Rgb8,
	Bgr8,
}

impl Encoding {
	fn format(self) -> PixelFormat {
		match self {
			Self::Rgb8 => PixelFormat::Rgb8,
			Self::Bgr8 => PixelFormat::Bgr8,
		}
	}

	fn ffmpeg_pix_fmt(self) -> &'static str {
		match self {
			Self::Rgb8 => "rgb24",
			Self::Bgr8 => "bgr24",
		}
	}
}

/// Something that yields frames in the requested encoding.
trait Source {
	fn next_frame(&mut self) -> anyhow::Result<Option<Image>>;
}

struct VideoSource {
	child: Child,
	stdout: ChildStdout,
	width: u32,
	height: u32,
	format: PixelFormat,
}

impl VideoSource {
	fn open(path: &Path, cli: &Cli) -> anyhow::Result<Self> {
		let (mut width, mut height) = probe(path)?;
		let mut filters = vec![format!("fps={}", cli.fps)];
		if let Some(w) = cli.width {
			// Keep the height even, as H.264 4:2:0 requires.
			height = ((height as f64 * w as f64 / width as f64 / 2.0).round() as u32) * 2;
			width = w & !1;
			filters.push(format!("scale={width}:{height}"));
		}

		let mut cmd = Command::new("ffmpeg");
		cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin"]);
		if !cli.once {
			cmd.args(["-stream_loop", "-1"]);
		}
		cmd.arg("-i")
			.arg(path)
			.args(["-an", "-vf", &filters.join(","), "-f", "rawvideo", "-pix_fmt"])
			.arg(cli.encoding.ffmpeg_pix_fmt())
			.arg("-")
			.stdout(Stdio::piped());
		let mut child = cmd.spawn().context("failed to run ffmpeg; is it installed?")?;
		let stdout = child.stdout.take().expect("piped stdout");
		tracing::info!(?path, width, height, "decoding video with ffmpeg");

		Ok(Self {
			child,
			stdout,
			width,
			height,
			format: cli.encoding.format(),
		})
	}
}

impl Source for VideoSource {
	fn next_frame(&mut self) -> anyhow::Result<Option<Image>> {
		let mut buf = vec![0u8; (self.width * self.height * 3) as usize];
		match self.stdout.read_exact(&mut buf) {
			Ok(()) => Ok(Some(Image::new(
				self.width,
				self.height,
				self.format,
				buf,
				Duration::ZERO,
			)?)),
			Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => Ok(None),
			Err(err) => Err(err.into()),
		}
	}
}

impl Drop for VideoSource {
	fn drop(&mut self) {
		let _ = self.child.kill();
		let _ = self.child.wait();
	}
}

fn probe(path: &Path) -> anyhow::Result<(u32, u32)> {
	let out = Command::new("ffprobe")
		.args([
			"-v",
			"error",
			"-select_streams",
			"v:0",
			"-show_entries",
			"stream=width,height",
			"-of",
			"csv=p=0",
		])
		.arg(path)
		.output()
		.context("failed to run ffprobe")?;
	let text = String::from_utf8_lossy(&out.stdout);
	let mut parts = text.trim().split(',').map(str::parse::<u32>);
	match (parts.next(), parts.next()) {
		(Some(Ok(w)), Some(Ok(h))) => Ok((w, h)),
		_ => bail!("ffprobe could not read the video size of {path:?}: {text:?}"),
	}
}

struct DirectorySource {
	files: Vec<PathBuf>,
	next: usize,
	looping: bool,
	format: PixelFormat,
}

impl DirectorySource {
	fn open(dir: &Path, cli: &Cli) -> anyhow::Result<Self> {
		let mut files: Vec<PathBuf> = std::fs::read_dir(dir)?
			.filter_map(|e| e.ok().map(|e| e.path()))
			.filter(|p| {
				p.extension()
					.and_then(|e| e.to_str())
					.is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "png" | "jpg" | "jpeg"))
			})
			.collect();
		files.sort();
		if files.is_empty() {
			bail!("no PNG/JPEG files in {dir:?}");
		}
		tracing::info!(?dir, frames = files.len(), "playing image sequence");
		Ok(Self {
			files,
			next: 0,
			looping: !cli.once,
			format: cli.encoding.format(),
		})
	}
}

impl Source for DirectorySource {
	fn next_frame(&mut self) -> anyhow::Result<Option<Image>> {
		if self.next == self.files.len() {
			if !self.looping {
				return Ok(None);
			}
			self.next = 0;
		}
		let path = &self.files[self.next];
		self.next += 1;

		let rgb = image::open(path)
			.with_context(|| format!("failed to decode {path:?}"))?
			.into_rgb8();
		let (width, height) = rgb.dimensions();
		let mut data = rgb.into_raw();
		if self.format == PixelFormat::Bgr8 {
			data.as_chunks_mut::<3>().0.iter_mut().for_each(|px| px.swap(0, 2));
		}
		Ok(Some(Image::new(width, height, self.format, data, Duration::ZERO)?))
	}
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
	init_logging();
	let cli = Cli::parse();

	let mut source: Box<dyn Source + Send> = if cli.input.is_dir() {
		Box::new(DirectorySource::open(&cli.input, &cli)?)
	} else {
		Box::new(VideoSource::open(&cli.input, &cli)?)
	};

	let ctx = r2r::Context::create()?;
	let mut node = r2r::Node::create(ctx, "image_source", "")?;
	// Reliable writers also match best-effort readers, and let large images be
	// recovered fragment-by-fragment instead of dropped whole.
	let publisher = node.create_publisher::<RosImage>(&cli.topic, QosProfile::default())?;
	// Publish-only node: no spinning needed, but it must outlive the publisher.
	let _node = node;

	tracing::info!(topic = cli.topic, fps = cli.fps, encoding = ?cli.encoding, "publishing");

	let mut ticker = tokio::time::interval(Duration::from_secs_f64(1.0 / cli.fps));
	ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
	let mut count = 0u64;
	loop {
		tokio::select! {
			_ = ticker.tick() => {}
			_ = tokio::signal::ctrl_c() => break,
		}
		let Some(image) = source.next_frame()? else {
			break;
		};
		let header = r2r::std_msgs::msg::Header {
			stamp: now_stamp(),
			frame_id: cli.frame_id.clone(),
		};
		publisher.publish(&image_to_ros(&image, header))?;
		count += 1;
		if count.is_multiple_of(300) {
			tracing::info!(count, "published frames");
		}
	}
	tracing::info!(count, "done");
	Ok(())
}
