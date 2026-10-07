//! Shared HTTP client.

use std::time::Duration;

pub const USER_AGENT: &str = concat!(
    "Medley/",
    env!("CARGO_PKG_VERSION"),
    " (https://github.com/simo1337s/localmusicplayer)"
);

/// One client for the whole app so connections and TLS sessions are reused.
pub fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .pool_max_idle_per_host(2)
        .build()
        .expect("failed to build HTTP client")
}
