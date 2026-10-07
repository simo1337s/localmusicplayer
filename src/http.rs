//! Shared HTTP client.

use std::sync::OnceLock;
use std::time::Duration;

pub const USER_AGENT: &str = concat!(
    "MultiMusic/",
    env!("CARGO_PKG_VERSION"),
    " (https://github.com/simo1337s/localmusicplayer)"
);

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
