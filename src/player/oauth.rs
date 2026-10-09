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

/// A redirect URI Sumo can catch on this computer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redirect {
    pub uri: String,
    pub host: &'static str,
    pub port: u16,
    pub path: String,
}

/// Checks that `uri` is a loopback address with a port (what Spotify allows for desktop apps
/// and what Sumo can listen on), e.g. `http://127.0.0.1:8899/callback`.
pub fn parse_redirect(uri: &str) -> Result<Redirect> {
    let uri = uri.trim();
    let example = "e.g. http://127.0.0.1:8899/login";
    let Some(rest) = uri.strip_prefix("http://") else {
        bail!("the redirect URI must start with http://127.0.0.1:<port>/ so Sumo can catch the login ({example})");
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let path = path.split(['?', '#']).next().unwrap_or("/");
    let (host, port) = if let Some(port) = authority.strip_prefix("127.0.0.1:") {
        ("127.0.0.1", port)
    } else if let Some(port) = authority.strip_prefix("[::1]:") {
        ("::1", port)
    } else if authority.starts_with("localhost") {
        bail!("Spotify no longer accepts \"localhost\" in redirect URIs: use 127.0.0.1 instead, in your Spotify app and here ({example})");
    } else if authority == "127.0.0.1" || authority == "[::1]" {
        bail!("add a port to the redirect URI ({example})");
    } else {
        bail!("the redirect URI must point to this computer: http://127.0.0.1:<port>/… ({example})");
    };
    let port: u16 = port
        .parse()
        .ok()
        .filter(|p| *p >= 1024)
        .ok_or_else(|| anyhow!("the redirect URI needs a port between 1024 and 65535 ({example})"))?;
    Ok(Redirect {
        uri: uri.to_string(),
        host,
        port,
        path: path.to_string(),
    })
}

/// True if a request for `requested` reached the redirect `path` (a trailing slash doesn't matter).
fn same_path(requested: &str, path: &str) -> bool {
    let norm = |p: &str| {
        let p = p.trim_end_matches('/');
        if p.is_empty() {
            "/".to_string()
        } else {
            p.to_string()
        }
    };
    norm(requested) == norm(path)
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
    authorize_url_at(AUTHORIZE_URL, client_id, redirect, scopes, challenge, state)
}

fn authorize_url_at(
    base: &str,
    client_id: &str,
    redirect: &str,
    scopes: &[&str],
    challenge: &str,
    state: &str,
) -> String {
    format!(
        "{base}?response_type=code&client_id={}&redirect_uri={}&code_challenge_method=S256&code_challenge={}&state={}&scope={}",
        urlencoding::encode(client_id),
        urlencoding::encode(redirect),
        challenge,
        state,
        urlencoding::encode(&scopes.join(" ")),
    )
}

/// What Spotify's authorize page says about a client ID and redirect URI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Check {
    /// It would show the login (or consent) page.
    Accepted,
    /// "redirect_uri: Not matching configuration" / "Invalid redirect URI".
    BadRedirect,
    /// "Invalid client": no app with this client ID.
    BadClient,
    /// Couldn't tell (offline, or an answer we don't recognise).
    Unknown,
}

/// Reads Spotify's answer to an authorize request. Only clear error texts count as a "no", so
/// a working login is never blocked by a misread page.
pub fn judge(status: u16, location: Option<&str>, body: &str) -> Check {
    let body = body.to_lowercase();
    let says_redirect = body.contains("not matching configuration") || body.contains("invalid redirect uri");
    let says_client = body.contains("invalid client") || body.contains("invalid_client");
    match status {
        300..=399 if location.is_some_and(|l| l.contains("error")) => Check::Unknown,
        300..=399 => Check::Accepted,
        200 if says_redirect => Check::BadRedirect,
        200 => Check::Accepted,
        400..=499 if says_redirect || body.contains("redirect_uri") || body.contains("redirect uri") => {
            Check::BadRedirect
        }
        400..=499 if says_client => Check::BadClient,
        _ => Check::Unknown,
    }
}

/// Asks Spotify (without a browser) whether it would accept this client ID and redirect URI.
async fn check_at(base: &str, client_id: &str, redirect: &str, scopes: &[&str]) -> Check {
    let (_, challenge) = pkce_pair();
    let url = authorize_url_at(base, client_id, redirect, scopes, &challenge, "check");
    let client = match reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(10))
        .user_agent(crate::http::USER_AGENT)
        .build()
    {
        Ok(c) => c,
        Err(_) => return Check::Unknown,
    };
    let Ok(resp) = client.get(&url).send().await else {
        return Check::Unknown;
    };
    let status = resp.status().as_u16();
    let location = resp
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|l| l.to_str().ok())
        .map(str::to_string);
    let body = resp.text().await.unwrap_or_default();
    let body: String = body.chars().take(20_000).collect();
    let verdict = judge(status, location.as_deref(), &body);
    tracing::debug!("spotify: authorize check for {redirect}: HTTP {status} → {verdict:?}");
    verdict
}

/// Close relatives of a redirect URI on the same port, in case the app has it saved slightly
/// differently (a trailing slash, or the usual /callback or /login path).
pub fn redirect_variants(uri: &str) -> Vec<String> {
    let Ok(r) = parse_redirect(uri) else {
        return Vec::new();
    };
    let base = match r.host {
        "::1" => format!("http://[::1]:{}", r.port),
        host => format!("http://{host}:{}", r.port),
    };
    let mut out: Vec<String> = Vec::new();
    let trimmed = r.uri.trim_end_matches('/');
    for candidate in [
        trimmed.to_string(),
        format!("{trimmed}/"),
        format!("{base}/callback"),
        format!("{base}/login"),
        format!("{base}/callback/"),
        format!("{base}/login/"),
    ] {
        if candidate != r.uri && !out.contains(&candidate) {
            out.push(candidate);
        }
    }
    out
}

/// The redirect URI to use: the one given when Spotify accepts it (or can't be asked), else
/// the variant of it Spotify does accept. Fails with what to fix when Spotify accepts none.
async fn usable_redirect_at(base: &str, client_id: &str, uri: &str, scopes: &[&str]) -> Result<String> {
    match check_at(base, client_id, uri, scopes).await {
        Check::Accepted | Check::Unknown => return Ok(uri.to_string()),
        Check::BadClient => bail!(
            "Spotify doesn't know the client ID {client_id}. Copy the Client ID from your app's page on \
             developer.spotify.com"
        ),
        Check::BadRedirect => {}
    }
    for variant in redirect_variants(uri) {
        if check_at(base, client_id, &variant, scopes).await == Check::Accepted {
            tracing::info!("spotify: {uri} isn't registered for {client_id}, but {variant} is; using that");
            return Ok(variant);
        }
    }
    bail!(
        "Spotify won't accept the Redirect URI {uri} for the app {client_id}: the app doesn't have it saved. On \
         developer.spotify.com open the app → Settings → Edit, add exactly {uri} under Redirect URIs, click Add, \
         then scroll to the bottom and click Save (it doesn't count until saved), and try again"
    )
}

/// Runs the whole browser login and returns the token. `redirect_uri` must be registered
/// for `client_id` (Spotify is asked first, and a slightly different registered form of it
/// is found and used).
pub async fn login(client_id: &str, redirect_uri: &str, scopes: &[&str]) -> Result<Token> {
    parse_redirect(redirect_uri)?;
    let redirect_uri = usable_redirect_at(AUTHORIZE_URL, client_id, redirect_uri, scopes).await?;
    let target = parse_redirect(&redirect_uri)?;
    let redirect = target.uri.clone();
    let (host, port) = (target.host, target.port);
    let listener = TcpListener::bind((host, port)).await.map_err(|e| {
        anyhow!("couldn't listen on {host}:{port} for the login redirect ({e}). Is another login window still open?")
    })?;
    let (verifier, challenge) = pkce_pair();
    let state: String = (0..16).map(|_| format!("{:x}", rand::random_range(0..16u8))).collect();
    let url = authorize_url(client_id, &redirect, scopes, &challenge, &state);
    tracing::info!("opening Spotify login: {url}");
    if open::that_detached(&url).is_err() {
        tracing::warn!("couldn't open a browser; open this URL manually: {url}");
    }
    let code = tokio::time::timeout(LOGIN_TIMEOUT, wait_for_code(&listener, &state, &target.path))
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

/// An app token from the app's Client ID and secret (no user, no browser, no redirect URI).
/// Good for search and catalog pages, not for anything in a user's library.
pub async fn client_credentials(client_id: &str, secret: &str) -> Result<Token> {
    client_credentials_at(&crate::http::client(), TOKEN_URL, client_id, secret).await
}

async fn client_credentials_at(http: &reqwest::Client, url: &str, client_id: &str, secret: &str) -> Result<Token> {
    let basic = base64::engine::general_purpose::STANDARD.encode(format!("{client_id}:{secret}"));
    let resp = http
        .post(url)
        .header(reqwest::header::AUTHORIZATION, format!("Basic {basic}"))
        .form(&[("grant_type", "client_credentials")])
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
        if reason.contains("invalid_client") {
            bail!("Spotify didn't accept the Client ID and secret ({reason}); copy both again from your app's page");
        }
        bail!("Spotify token request failed ({status}): {reason}");
    }
    let t: TokenResponse = serde_json::from_str(&body).context("unexpected token response from Spotify")?;
    Ok(Token {
        access_token: t.access_token,
        refresh_token: None,
        expires_at: Instant::now() + Duration::from_secs(t.expires_in.unwrap_or(3600)),
    })
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

/// Accepts connections until the redirect to `path` arrives; returns the authorization code.
async fn wait_for_code(listener: &TcpListener, state: &str, path: &str) -> Result<String> {
    let expected = path;
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
        let (requested, query) = path.split_once('?').unwrap_or((path.as_str(), ""));
        if !same_path(requested, expected) {
            respond(&mut stream, "404 Not Found", "").await;
            continue;
        }
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
            &page("Logged in to Spotify", "You can close this tab and go back to Sumo."),
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
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>Sumo</title><style>\
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

    #[tokio::test]
    async fn app_token_from_client_secret() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/api/token", listener.local_addr().unwrap());
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = vec![0u8; 4096];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]).to_lowercase();
                // "id:secret" in base64.
                let good = request.contains("authorization: basic awq6c2vjcmv0");
                let form = request.contains("grant_type=client_credentials");
                let (status, body) = if good && form {
                    (
                        "200 OK",
                        r#"{"access_token":"apptoken","token_type":"Bearer","expires_in":3600}"#,
                    )
                } else {
                    (
                        "400 Bad Request",
                        r#"{"error":"invalid_client","error_description":"Invalid client secret"}"#,
                    )
                };
                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            }
        });
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let token = client_credentials_at(&http, &url, "id", "secret").await.unwrap();
        assert_eq!(token.access_token, "apptoken");
        assert!(token.refresh_token.is_none());
        let err = client_credentials_at(&http, &url, "id", "wrong")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("Client ID and secret"), "{err}");
    }

    #[test]
    fn reads_spotify_answers() {
        assert_eq!(
            judge(303, Some("https://accounts.spotify.com/login?continue=x"), ""),
            Check::Accepted
        );
        assert_eq!(judge(200, None, "<html>Log in to Spotify</html>"), Check::Accepted);
        assert_eq!(
            judge(400, None, "redirect_uri: Not matching configuration"),
            Check::BadRedirect
        );
        assert_eq!(
            judge(400, None, "INVALID_CLIENT: Invalid redirect URI"),
            Check::BadRedirect
        );
        assert_eq!(judge(400, None, "INVALID_CLIENT: Invalid client"), Check::BadClient);
        assert_eq!(
            judge(200, None, "<p>redirect_uri: Not matching configuration</p>"),
            Check::BadRedirect
        );
        // The login page mentions the redirect URI in its links: still a yes.
        assert_eq!(
            judge(200, None, "<a href='/login?continue=...redirect_uri%3D...'>"),
            Check::Accepted
        );
        assert_eq!(judge(500, None, ""), Check::Unknown);
        assert_eq!(
            judge(302, Some("https://accounts.spotify.com/authorize/error"), ""),
            Check::Unknown
        );
    }

    #[test]
    fn redirect_variants_keep_the_port() {
        assert_eq!(
            redirect_variants("http://127.0.0.1:1337"),
            vec![
                "http://127.0.0.1:1337/",
                "http://127.0.0.1:1337/callback",
                "http://127.0.0.1:1337/login",
                "http://127.0.0.1:1337/callback/",
                "http://127.0.0.1:1337/login/"
            ]
        );
        let v = redirect_variants("http://127.0.0.1:8899/login");
        assert_eq!(v[0], "http://127.0.0.1:8899/login/");
        assert!(!v.contains(&"http://127.0.0.1:8899/login".to_string()));
        assert!(redirect_variants("https://example.com/cb").is_empty());
    }

    /// Against a stand-in for accounts.spotify.com that only has "http://127.0.0.1:1337/" saved.
    #[tokio::test]
    async fn finds_the_registered_form_of_the_redirect_uri() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/authorize", listener.local_addr().unwrap());
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = vec![0u8; 4096];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]).to_string();
                let line = head.lines().next().unwrap_or("").to_string();
                let resp = if !line.contains("client_id=good") {
                    {
                        let body = "INVALID_CLIENT: Invalid client";
                        format!(
                            "HTTP/1.1 400 Bad Request\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        )
                    }
                } else if line.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A1337%2F&") {
                    "HTTP/1.1 303 See Other\r\nLocation: https://accounts.spotify.com/login\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
                } else {
                    let body = "redirect_uri: Not matching configuration";
                    format!(
                        "HTTP/1.1 400 Bad Request\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                };
                let _ = sock.write_all(resp.as_bytes()).await;
            }
        });
        let scopes = ["user-library-read"];
        assert_eq!(
            usable_redirect_at(&base, "good", "http://127.0.0.1:1337", &scopes)
                .await
                .unwrap(),
            "http://127.0.0.1:1337/"
        );
        let err = usable_redirect_at(&base, "good", "http://127.0.0.1:9999/cb", &scopes)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("Save"), "{err}");
        let err = usable_redirect_at(&base, "bad", "http://127.0.0.1:1337", &scopes)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("client ID"), "{err}");
        // No answer at all: go ahead as configured.
        assert_eq!(
            usable_redirect_at("http://127.0.0.1:1/authorize", "good", "http://127.0.0.1:1337", &scopes)
                .await
                .unwrap(),
            "http://127.0.0.1:1337"
        );
    }

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
        let waiter = tokio::spawn(async move { wait_for_code(&listener, "xyz", "/login").await });
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

    #[test]
    fn redirect_uri_without_a_path_is_sent_as_is() {
        // Spotify compares character by character: no slash may be added.
        let r = parse_redirect("http://127.0.0.1:1337").unwrap();
        assert_eq!(
            (r.uri.as_str(), r.port, r.path.as_str()),
            ("http://127.0.0.1:1337", 1337, "/")
        );
        let url = authorize_url(
            "b9793ae53d764abeb54f4f0950b25cb1",
            &r.uri,
            &["user-library-read"],
            "c",
            "s",
        );
        assert!(url.contains("&redirect_uri=http%3A%2F%2F127.0.0.1%3A1337&"), "{url}");
        assert!(url.contains("client_id=b9793ae53d764abeb54f4f0950b25cb1&"), "{url}");
    }

    #[test]
    fn redirect_uris() {
        let r = parse_redirect("http://127.0.0.1:8899/login").unwrap();
        assert_eq!((r.host, r.port, r.path.as_str()), ("127.0.0.1", 8899, "/login"));
        let r = parse_redirect(" http://127.0.0.1:8888/callback ").unwrap();
        assert_eq!(
            (r.port, r.path.as_str(), r.uri.as_str()),
            (8888, "/callback", "http://127.0.0.1:8888/callback")
        );
        assert_eq!(parse_redirect("http://127.0.0.1:9000").unwrap().path, "/");
        assert_eq!(parse_redirect("http://[::1]:9000/cb").unwrap().host, "::1");
        for bad in [
            "http://localhost:8899/login",
            "https://127.0.0.1:8899/login",
            "http://127.0.0.1/login",
            "http://127.0.0.1:80/login",
            "http://example.com:8899/login",
            "127.0.0.1:8899/login",
            "",
        ] {
            assert!(parse_redirect(bad).is_err(), "{bad}");
        }
        assert!(format!("{:#}", parse_redirect("http://localhost:8899/login").unwrap_err()).contains("127.0.0.1"));
        assert!(same_path("/callback/", "/callback"));
        assert!(same_path("/", ""));
        assert!(!same_path("/login", "/callback"));
    }

    /// The redirect is caught on whatever path the user's app registered.
    #[tokio::test]
    async fn catches_redirect_on_a_custom_path() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let waiter = tokio::spawn(async move { wait_for_code(&listener, "st", "/callback").await });
        for (path, ok) in [
            ("/login?code=wrong&state=st", false),
            ("/callback?code=right&state=st", true),
        ] {
            let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            s.write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
                .await
                .unwrap();
            let mut resp = String::new();
            s.read_to_string(&mut resp).await.unwrap();
            assert_eq!(resp.starts_with("HTTP/1.1 200"), ok, "{resp}");
        }
        assert_eq!(waiter.await.unwrap().unwrap(), "right");
    }

    #[tokio::test]
    async fn denied_login_is_an_error() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let waiter = tokio::spawn(async move { wait_for_code(&listener, "s", "/login").await });
        let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        s.write_all(b"GET /login?error=access_denied&state=s HTTP/1.1\r\n\r\n")
            .await
            .unwrap();
        let err = waiter.await.unwrap().unwrap_err().to_string();
        assert!(err.contains("access_denied"), "{err}");
    }
}
