//! Color conversion throughput: `cargo bench -p moqros --bench convert`.
//!
//! A plain timing loop rather than a benchmark framework, to keep the dependency tree small.

use std::hint::black_box;
use std::time::{Duration, Instant};

use moqros::convert::{I420, yuv_to_rgb8};
use moqros::{Image, PixelFormat};

/// Run `f` for about a second and return the mean time of one call.
fn time(mut f: impl FnMut()) -> Duration {
	f();
	let start = Instant::now();
	let mut runs = 0u32;
	while start.elapsed() < Duration::from_secs(1) {
		f();
		runs += 1;
	}
	start.elapsed() / runs
}

fn image(width: u32, height: u32, format: PixelFormat) -> Image {
	let len = width as usize * height as usize * format.bytes_per_pixel();
	let data: Vec<u8> = (0..len).map(|i| u8::try_from(i * 7 % 251).unwrap()).collect();
	Image::new(width, height, format, data, Duration::ZERO).unwrap()
}

fn main() {
	for (name, width, height) in [("720p", 1280, 720), ("1080p", 1920, 1080)] {
		for format in [
			PixelFormat::Bgr8,
			PixelFormat::Rgba8,
			PixelFormat::Yuyv,
			PixelFormat::Mono8,
		] {
			let image = image(width, height, format);
			let mut yuv = I420::from_image(&image).unwrap();
			let new = time(|| {
				black_box(I420::from_image(black_box(&image)).unwrap());
			});
			let reused = time(|| {
				yuv.fill_from_image(black_box(&image)).unwrap();
				black_box(&yuv);
			});
			println!("{name} {format:>11} -> I420: {new:>10.2?} new, {reused:>10.2?} reused");
		}
		let yuv = I420::from_image(&image(width, height, PixelFormat::Rgb8)).unwrap();
		let planes = yuv.planes();
		let rgb = time(|| {
			black_box(yuv_to_rgb8(black_box(&planes)));
		});
		println!("{name}        I420 -> rgb8: {rgb:>10.2?}");
	}
}
