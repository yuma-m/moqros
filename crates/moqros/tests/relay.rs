//! End-to-end test through a real `moq-relay` process.
//!
//! Skipped unless `MOQROS_RELAY_BIN` points at a moq-relay binary:
//!
//! ```sh
//! cargo install moq-relay
//! MOQROS_RELAY_BIN=$(which moq-relay) cargo test -p moqros --test relay
//! ```

use std::process::{Child, Command, Stdio};
use std::time::Duration;

use moqros::{ClientConfig, EncoderSettings, Image, PixelFormat, Publisher, Subscriber};

struct Relay(Child);

impl Drop for Relay {
	fn drop(&mut self) {
		let _ = self.0.kill();
		let _ = self.0.wait();
	}
}

fn start_relay(bin: &str, port: u16) -> Relay {
	let config = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docker/relay.toml");
	let child = Command::new(bin)
		.arg(config)
		.env("MOQ_LISTEN_BIND", format!("127.0.0.1:{port}"))
		.env("MOQ_WEB_HTTP_LISTEN", format!("127.0.0.1:{port}"))
		.stdout(Stdio::null())
		.stderr(Stdio::null())
		.spawn()
		.expect("failed to spawn moq-relay");
	Relay(child)
}

fn frame(width: u32, height: u32, n: u32) -> Image {
	let data: Vec<u8> = (0..height)
		.flat_map(|y| (0..width).flat_map(move |x| [((x + n * 8) % 256) as u8, (y % 256) as u8, 200]))
		.collect();
	Image::new(
		width,
		height,
		PixelFormat::Rgb8,
		data,
		Duration::from_millis(n as u64 * 33),
	)
	.unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn publish_and_subscribe_through_relay() -> anyhow::Result<()> {
	let Ok(bin) = std::env::var("MOQROS_RELAY_BIN") else {
		eprintln!("MOQROS_RELAY_BIN not set; skipping relay test");
		return Ok(());
	};
	let _ = tracing_subscriber::fmt().with_env_filter("moqros=debug").try_init();

	let port = 14443;
	let _relay = start_relay(&bin, port);
	tokio::time::sleep(Duration::from_millis(500)).await;

	let config = ClientConfig::new(format!("http://127.0.0.1:{port}/anon").parse()?);

	let publisher = Publisher::connect(&config)?;
	tokio::time::timeout(Duration::from_secs(10), publisher.established()).await??;
	let mut broadcast = publisher.create_image_broadcast(
		"test/image",
		EncoderSettings {
			keyframe_interval: Duration::from_millis(200),
			..Default::default()
		},
	)?;

	let subscriber = Subscriber::connect(&config)?;
	tokio::time::timeout(Duration::from_secs(10), subscriber.established()).await??;

	// Publish continuously from a blocking thread, like a ROS callback would.
	let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
	let pub_task = tokio::task::spawn_blocking(move || -> moqros::Result<()> {
		for n in 0.. {
			if stop_rx.try_recv().is_ok() {
				break;
			}
			broadcast.publish(&frame(160, 120, n))?;
			std::thread::sleep(Duration::from_millis(33));
		}
		Ok(())
	});

	let mut images = tokio::time::timeout(Duration::from_secs(10), subscriber.subscribe_images("test/image")).await??;

	let mut received = Vec::new();
	while received.len() < 10 {
		let image = tokio::time::timeout(Duration::from_secs(5), images.next_image())
			.await??
			.expect("broadcast ended early");
		assert_eq!((image.width, image.height, image.format), (160, 120, PixelFormat::Rgb8));
		received.push(image.timestamp);
	}
	assert!(
		received.windows(2).all(|w| w[0] < w[1]),
		"timestamps increase: {received:?}"
	);

	stop_tx.send(()).ok();
	pub_task.await??;
	Ok(())
}
