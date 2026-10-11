//! Shared HTTP client.

use std::sync::OnceLock;
use std::time::Duration;

pub const USER_AGENT: &str = concat!(
    "Sumo/",
    env!("CARGO_PKG_VERSION"),
    " (https://github.com/v0-0x/sumo-music)"
);

/// Most an image may be (covers, avatars): a bigger answer is no image Sumo wants.
pub const MAX_IMAGE: usize = 16 << 20;

/// The body of `resp`, giving up once it passes `max` bytes (counted after decompression, so
/// a small compressed answer can't unpack into gigabytes).
pub async fn read_capped(mut resp: reqwest::Response, max: usize) -> anyhow::Result<Vec<u8>> {
    if resp.content_length().is_some_and(|n| n > max as u64) {
        anyhow::bail!("the answer is too big");
    }
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        if body.len() + chunk.len() > max {
            anyhow::bail!("the answer is too big");
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// One client for the whole app: clones share the connection pool and the TLS
/// context (which holds the parsed system CA store).
pub fn client() -> reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .user_agent(USER_AGENT)
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(30))
                .pool_max_idle_per_host(2)
                .build()
                .expect("failed to build HTTP client")
        })
        .clone()
}
