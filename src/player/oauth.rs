//! Spotify OAuth 2.0 authorization-code flow with PKCE.
//!
//! The redirect is caught by a small loopback server that gives up after a timeout and is
//! dropped (freeing its port) when the login is cancelled or retried.

use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const AUTHORIZE_URL: &str = "https://accounts.spotify.com/authorize";
const TOKEN_URL: &str = "https://accounts.spotify.com/api/token";
/// How long we wait for the user to finish logging in in the browser.
pub const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);

const LOGO_SVG: &str = include_str!("../../assets/logo.svg");

#[derive(Debug, Clone)]
pub struct Token {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: Instant,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    refresh_token: Option<String>,
}

#[derive(Deserialize)]
struct TokenError {
    error: String,
    #[serde(default)]
    error_description: Option<String>,
}

pub fn redirect_uri(port: u16) -> String {
    format!("http://127.0.0.1:{port}/login")
}

/// (verifier, S256 challenge)
pub fn pkce_pair() -> (String, String) {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";
    let verifier: String = (0..64)
        .map(|_| CHARS[rand::random_range(0..CHARS.len())] as char)
        .collect();
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

pub fn authorize_url(client_id: &str, redirect: &str, scopes: &[&str], challenge: &str, state: &str) -> String {
    format!(
        "{AUTHORIZE_URL}?response_type=code&client_id={}&redirect_uri={}&code_challenge_method=S256&code_challenge={}&state={}&scope={}",
        urlencoding::encode(client_id),
        urlencoding::encode(redirect),
        challenge,
        state,
        urlencoding::encode(&scopes.join(" ")),
    )
}

/// Runs the whole browser login and returns the token.
pub async fn login(client_id: &str, port: u16, scopes: &[&str]) -> Result<Token> {
    let redirect = redirect_uri(port);
    let listener = TcpListener::bind(("127.0.0.1", port)).await.map_err(|e| {
        anyhow!("couldn't listen on 127.0.0.1:{port} for the login redirect ({e}). Is another login window still open?")
    })?;
    let (verifier, challenge) = pkce_pair();
    let state: String = (0..16).map(|_| format!("{:x}", rand::random_range(0..16u8))).collect();
    let url = authorize_url(client_id, &redirect, scopes, &challenge, &state);
    tracing::info!("opening Spotify login: {url}");
    if open::that_detached(&url).is_err() {
        tracing::warn!("couldn't open a browser; open this URL manually: {url}");
    }
    let code = tokio::time::timeout(LOGIN_TIMEOUT, wait_for_code(&listener, &state))
        .await
        .map_err(|_| anyhow!("timed out waiting for the Spotify login to finish in the browser"))??;
    drop(listener);
    let form = [
        ("grant_type", "authorization_code"),
        ("code", code.as_str()),
        ("redirect_uri", redirect.as_str()),
        ("client_id", client_id),
        ("code_verifier", verifier.as_str()),
    ];
    request_token(&form).await
}

pub async fn refresh(client_id: &str, refresh_token: &str) -> Result<Token> {
    let form = [
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", client_id),
    ];
    let mut token = request_token(&form).await?;
    // Spotify may or may not rotate the refresh token.
    if token.refresh_token.is_none() {
        token.refresh_token = Some(refresh_token.to_string());
    }
    Ok(token)
}

async fn request_token(form: &[(&str, &str)]) -> Result<Token> {
    let resp = crate::http::client()
        .post(TOKEN_URL)
        .form(form)
        .send()
        .await
        .context("couldn't reach accounts.spotify.com")?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        let reason = serde_json::from_str::<TokenError>(&body)
            .map(|e| match e.error_description {
                Some(d) if !d.is_empty() => format!("{}: {d}", e.error),
                _ => e.error,
            })
            .unwrap_or_else(|_| body.chars().take(200).collect());
        bail!("Spotify token request failed ({status}): {reason}");
    }
    let t: TokenResponse = serde_json::from_str(&body).context("unexpected token response from Spotify")?;
    Ok(Token {
        access_token: t.access_token,
        refresh_token: t.refresh_token.filter(|r| !r.is_empty()),
        expires_at: Instant::now() + Duration::from_secs(t.expires_in.unwrap_or(3600)),
    })
}

/// Accepts connections until the `/login` redirect arrives; returns the authorization code.
async fn wait_for_code(listener: &TcpListener, state: &str) -> Result<String> {
    loop {
        let (mut stream, _) = listener.accept().await?;
        let request = match read_request(&mut stream).await {
            Ok(r) => r,
            Err(_) => continue,
        };
        let path = request
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .unwrap_or("/")
            .to_string();
        let Some(query) = path.strip_prefix("/login") else {
            respond(&mut stream, "404 Not Found", "").await;
            continue;
        };
        let params = parse_query(query.trim_start_matches('?'));
        let get = |k: &str| params.iter().find(|(key, _)| key == k).map(|(_, v)| v.clone());
        if let Some(err) = get("error") {
            respond(
                &mut stream,
                "200 OK",
                &page("Login cancelled", &format!("Spotify said: {err}")),
            )
            .await;
            bail!("Spotify login was not completed ({err})");
        }
        if get("state").as_deref() != Some(state) {
            respond(
                &mut stream,
                "400 Bad Request",
                &page("Something went wrong", "Please try logging in again."),
            )
            .await;
            bail!("Spotify login returned an unexpected state; please try again");
        }
        let Some(code) = get("code") else {
            respond(
                &mut stream,
                "400 Bad Request",
                &page("Something went wrong", "No login code received."),
            )
            .await;
            bail!("Spotify login returned no code");
        };
        respond(
            &mut stream,
            "200 OK",
            &page(
                "Logged in to Spotify",
                "You can close this tab and go back to MultiMusic.",
            ),
        )
        .await;
        return Ok(code);
    }
}

async fn read_request(stream: &mut TcpStream) -> Result<String> {
    let mut buf = Vec::with_capacity(2048);
    let mut chunk = [0u8; 2048];
    loop {
        let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut chunk)).await??;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() > 16 * 1024 {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

async fn respond(stream: &mut TcpStream, status: &str, body: &str) {
    let resp = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(resp.as_bytes()).await;
    let _ = stream.shutdown().await;
}

fn page(title: &str, text: &str) -> String {
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>MultiMusic</title><style>\
         body{{background:#121212;color:#f2f0ea;font-family:system-ui,sans-serif;display:grid;place-items:center;height:100vh;margin:0}}\
         .c{{text-align:center}}svg{{width:96px;height:96px}}h1{{font-weight:700;margin:18px 0 6px}}p{{color:#a7a59f}}\
         </style></head><body><div class=\"c\">{LOGO_SVG}<h1>{title}</h1><p>{text}</p></div></body></html>"
    )
}

pub fn parse_query(q: &str) -> Vec<(String, String)> {
    q.split('&')
        .filter(|p| !p.is_empty())
        .map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            let dec = |s: &str| {
                urlencoding::decode(&s.replace('+', " "))
                    .map(|c| c.into_owned())
                    .unwrap_or_else(|_| s.to_string())
            };
            (dec(k), dec(v))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_matches_rfc7636_example() {
        // RFC 7636 appendix B.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        assert_eq!(challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
        let (v, c) = pkce_pair();
        assert_eq!(v.len(), 64);
        assert_eq!(c, URL_SAFE_NO_PAD.encode(Sha256::digest(v.as_bytes())));
    }

    #[test]
    fn authorize_url_is_encoded() {
        let url = authorize_url(
            "abc",
            "http://127.0.0.1:8898/login",
            &["streaming", "user-read-private"],
            "CH",
            "st",
        );
        assert!(url.starts_with("https://accounts.spotify.com/authorize?response_type=code&client_id=abc"));
        assert!(url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A8898%2Flogin"));
        assert!(url.contains("scope=streaming%20user-read-private"));
        assert!(url.contains("code_challenge_method=S256&code_challenge=CH&state=st"));
    }

    #[test]
    fn query_parsing() {
        let q = parse_query("code=AQ%2Fx+y&state=12&error=");
        assert_eq!(q[0], ("code".into(), "AQ/x y".into()));
        assert_eq!(q[1], ("state".into(), "12".into()));
        assert_eq!(q[2], ("error".into(), "".into()));
    }

    /// Full redirect handling against the real loopback listener (no Spotify involved).
    #[tokio::test]
    async fn catches_redirect() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let waiter = tokio::spawn(async move { wait_for_code(&listener, "xyz").await });
        // A stray request first (favicon), then the redirect.
        for path in ["/favicon.ico", "/login?code=the-code&state=xyz"] {
            let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            s.write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
                .await
                .unwrap();
            let mut resp = String::new();
            s.read_to_string(&mut resp).await.unwrap();
            assert!(resp.starts_with("HTTP/1.1"));
        }
        assert_eq!(waiter.await.unwrap().unwrap(), "the-code");
    }

    #[tokio::test]
    async fn denied_login_is_an_error() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let waiter = tokio::spawn(async move { wait_for_code(&listener, "s").await });
        let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        s.write_all(b"GET /login?error=access_denied&state=s HTTP/1.1\r\n\r\n")
            .await
            .unwrap();
        let err = waiter.await.unwrap().unwrap_err().to_string();
        assert!(err.contains("access_denied"), "{err}");
    }
}
