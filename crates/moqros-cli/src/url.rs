//! Relay URLs: insecurity warnings, and redaction for logs.

/// Why connecting to `url` doesn't authenticate the relay, unless it's a development setup
/// on this machine.
pub(crate) fn insecurity(url: &moqros::Url, insecure: bool) -> Option<&'static str> {
	if insecure {
		return Some("--insecure skips TLS certificate verification");
	}
	let loopback = url.host_str().is_some_and(|host| {
		host == "localhost"
			|| host.ends_with(".localhost")
			|| host
				.trim_matches(['[', ']'])
				.parse::<std::net::IpAddr>()
				.is_ok_and(|ip| ip.is_loopback())
	});
	(url.scheme() == "http" && !loopback).then_some(
		"an http:// relay URL fetches the certificate fingerprint over plain HTTP; use https:// outside development",
	)
}

pub(crate) fn redact_url(url: &moqros::Url) -> moqros::Url {
	const REDACTED: &str = "REDACTED";
	let mut url = url.clone();
	if url.password().is_some() {
		let _ = url.set_password(Some(REDACTED));
	}
	// Some relays (e.g. Cloudflare) take the token as a path segment instead of `?jwt=`.
	if url.path_segments().is_some_and(|mut segments| segments.any(is_jwt)) {
		let path = url
			.path_segments()
			.into_iter()
			.flatten()
			.map(|segment| if is_jwt(segment) { REDACTED } else { segment })
			.collect::<Vec<_>>()
			.join("/");
		url.set_path(&path);
	}
	if url.query_pairs().any(|(key, _)| key == "jwt") {
		let pairs: Vec<(String, String)> = url
			.query_pairs()
			.map(|(key, value)| {
				let value = if key == "jwt" {
					REDACTED.into()
				} else {
					value.into_owned()
				};
				(key.into_owned(), value)
			})
			.collect();
		url.query_pairs_mut().clear().extend_pairs(pairs);
	}
	url
}

/// Whether a path segment looks like a JWT: three base64url parts, the first a JSON header (`eyJ`).
fn is_jwt(segment: &str) -> bool {
	segment.starts_with("eyJ")
		&& segment.split('.').count() == 3
		&& segment
			.bytes()
			.all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn flags_unauthenticated_relays() {
		let check = |url: &str, insecure| insecurity(&url.parse().unwrap(), insecure).is_some();
		for url in [
			"http://localhost:4443/anon",
			"http://foo.localhost:4443/anon",
			"http://127.0.0.1:4443/anon",
			"http://[::1]:4443/anon",
			"https://relay.example.com/robot",
		] {
			assert!(!check(url, false), "{url}");
		}
		assert!(check("http://relay:4443/anon", false));
		assert!(check("http://192.168.1.10:4443/anon", false));
		assert!(check("http://localhost.example.com:4443/anon", false));
		assert!(check("https://relay.example.com/robot", true));
		assert!(check("http://localhost:4443/anon", true));
	}

	#[test]
	fn redacts_relay_url_secrets() {
		let redact = |url: &str| redact_url(&url.parse().unwrap()).to_string();
		assert_eq!(
			redact("https://relay.example/robot?jwt=eyJ.secret&a=1"),
			"https://relay.example/robot?jwt=REDACTED&a=1"
		);
		assert_eq!(
			redact("https://user:pass@relay.example/robot"),
			"https://user:REDACTED@relay.example/robot"
		);
		assert_eq!(
			redact("https://relay.example/eyJhbGciOiJFZERTQSJ9.eyJzdWIiOiJyb2JvdCJ9.c2ln"),
			"https://relay.example/REDACTED"
		);
		assert_eq!(
			redact("https://relay.example/robot/eyJh.eyJz.sig-_/cam"),
			"https://relay.example/robot/REDACTED/cam"
		);
		// URLs without secrets come back unchanged.
		for url in [
			"http://localhost:4443/anon",
			"https://relay.example/robot?a=1",
			"https://relay.example/eyJ/robot.v1",
		] {
			assert_eq!(redact(url), url);
		}
	}
}
