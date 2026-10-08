//! Updates from GitHub releases. On Windows and macOS a new version is downloaded, checked
//! against the release's SHA-256 sums and installed (the installer runs silently and starts
//! MultiMusic again; the macOS app is swapped in place). Linux builds come from the git
//! checkout, so there MultiMusic only says that a new version is out.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// The file with the checksums of a release's files.
pub const SUMS_FILE: &str = "SHA256SUMS.txt";

#[derive(Debug, Clone, PartialEq)]
pub struct Release {
    /// "0.3.0" (without the tag's "v").
    pub version: String,
    pub notes: String,
    /// The release's web page.
    pub page: String,
    /// This platform's download; `None` on Linux or when the release has none.
    pub asset: Option<Asset>,
    pub sums: Option<Asset>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Asset {
    pub name: String,
    /// API URL (works for private repositories with a token).
    pub url: String,
    pub size: u64,
}

/// What this platform downloads from a release.
pub fn asset_name(version: &str) -> Option<String> {
    if cfg!(windows) {
        Some(format!("MultiMusic-Setup-{version}-x64.exe"))
    } else if cfg!(target_os = "macos") {
        let arch = if cfg!(target_arch = "aarch64") {
            "arm64"
        } else {
            "intel"
        };
        Some(format!("MultiMusic-{version}-macos-{arch}.zip"))
    } else {
        None
    }
}

/// Whether this build can install updates by itself.
pub fn can_install() -> bool {
    cfg!(any(windows, target_os = "macos"))
}

/// A client for GitHub's API and downloads: no overall time limit (installers are large), but
/// a stalled connection gives up.
pub fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(crate::http::USER_AGENT)
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(60))
        .build()
        .unwrap_or_default()
}

fn get(http: &reqwest::Client, url: &str, token: &str, accept: &str) -> reqwest::RequestBuilder {
    let req = http
        .get(url)
        .header("Accept", accept)
        .header("X-GitHub-Api-Version", "2022-11-28");
    match token.trim() {
        "" => req,
        token => req.bearer_auth(token),
    }
}

/// The newest release of `repo` ("owner/name").
pub async fn latest(http: &reqwest::Client, api: &str, repo: &str, token: &str) -> Result<Release> {
    let url = format!("{api}/repos/{repo}/releases/latest");
    let resp = get(http, &url, token, "application/vnd.github+json")
        .send()
        .await
        .context("couldn't reach GitHub")?;
    match resp.status().as_u16() {
        200 => {}
        404 if token.trim().is_empty() => {
            bail!("no releases found (while the repository is private, add a GitHub token in Settings → Updates)")
        }
        404 => bail!("no releases found"),
        401 | 403 => bail!("GitHub refused the request ({}): check the token", resp.status()),
        _ => bail!("GitHub answered {}", resp.status()),
    }
    let json: Value = resp.json().await.context("unexpected answer from GitHub")?;
    parse_release(&json).ok_or_else(|| anyhow!("unexpected answer from GitHub"))
}

pub fn parse_release(json: &Value) -> Option<Release> {
    let tag = json.get("tag_name")?.as_str()?;
    let version = tag.trim_start_matches(['v', 'V']).to_string();
    let assets: Vec<Asset> = json
        .get("assets")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|a| {
                    Some(Asset {
                        name: a.get("name")?.as_str()?.to_string(),
                        url: a.get("url")?.as_str()?.to_string(),
                        size: a.get("size").and_then(Value::as_u64).unwrap_or(0),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let find = |name: &str| assets.iter().find(|a| a.name == name).cloned();
    Some(Release {
        asset: asset_name(&version).and_then(|n| find(&n)),
        sums: find(SUMS_FILE),
        notes: json.get("body").and_then(Value::as_str).unwrap_or_default().to_string(),
        page: json
            .get("html_url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        version,
    })
}

/// Whether `candidate` ("1.2.10") is newer than `current` ("1.2.9"). A pre-release
/// ("1.3.0-beta") comes before its release.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    fn parts(v: &str) -> (Vec<u64>, bool) {
        let (numbers, pre) = match v.trim().split_once('-') {
            Some((n, _)) => (n, true),
            None => (v.trim(), false),
        };
        (numbers.split('.').map(|p| p.parse().unwrap_or(0)).collect(), pre)
    }
    let (a, a_pre) = parts(candidate);
    let (b, b_pre) = parts(current);
    let len = a.len().max(b.len());
    for i in 0..len {
        let (x, y) = (a.get(i).copied().unwrap_or(0), b.get(i).copied().unwrap_or(0));
        if x != y {
            return x > y;
        }
    }
    // Same numbers: a release beats its pre-release.
    !a_pre && b_pre
}

/// Downloads an asset to `dest`, reporting progress (0.0..=1.0).
pub async fn download(
    http: &reqwest::Client,
    asset: &Asset,
    token: &str,
    dest: &Path,
    progress: impl Fn(f32),
) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let mut resp = get(http, &asset.url, token, "application/octet-stream")
        .send()
        .await
        .context("couldn't start the download")?
        .error_for_status()
        .context("the download was refused")?;
    let total = resp.content_length().unwrap_or(asset.size).max(1);
    let mut file = tokio::fs::File::create(dest)
        .await
        .with_context(|| format!("couldn't write {}", dest.display()))?;
    let mut done = 0u64;
    while let Some(chunk) = resp.chunk().await.context("the download broke off")? {
        file.write_all(&chunk).await?;
        done += chunk.len() as u64;
        progress((done as f32 / total as f32).min(1.0));
    }
    file.flush().await?;
    Ok(())
}

/// Checks `file` against its line in the release's SHA-256 sums.
pub fn verify(file: &Path, name: &str, sums: &str) -> Result<()> {
    let expected = sums
        .lines()
        .find_map(|line| {
            let (hash, file) = line.trim().split_once(char::is_whitespace)?;
            (file.trim().trim_start_matches('*') == name).then(|| hash.to_ascii_lowercase())
        })
        .ok_or_else(|| anyhow!("the release has no checksum for {name}"))?;
    let mut hasher = Sha256::new();
    let mut f = std::fs::File::open(file)?;
    std::io::copy(&mut f, &mut hasher)?;
    let actual: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
    if actual != expected {
        bail!("the download is damaged (checksum mismatch); try again");
    }
    Ok(())
}

/// Where downloads wait to be installed.
pub fn download_dir() -> PathBuf {
    std::env::temp_dir().join("multimusic-update")
}

/// Starts installing a downloaded update. MultiMusic has to quit right after; the new
/// version starts by itself.
pub fn install(file: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        // The installer closes what is left of MultiMusic, installs over it and starts it.
        std::process::Command::new(file)
            .args(["/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART", "/SP-"])
            .spawn()
            .context("couldn't start the installer")?;
        Ok(())
    }
    #[cfg(target_os = "macos")]
    {
        install_app(file)
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = file;
        bail!("update MultiMusic with your package manager")
    }
}

/// macOS: unpacks the new MultiMusic.app and puts it where the running one is.
#[cfg(target_os = "macos")]
fn install_app(zip: &Path) -> Result<()> {
    let exe = std::env::current_exe()?;
    // …/MultiMusic.app/Contents/MacOS/multimusic
    let bundle = exe
        .ancestors()
        .nth(3)
        .filter(|p| p.extension().is_some_and(|e| e == "app"))
        .ok_or_else(|| anyhow!("MultiMusic isn't running from its app bundle"))?
        .to_path_buf();
    let unpack = zip.with_extension("unpacked");
    let _ = std::fs::remove_dir_all(&unpack);
    std::fs::create_dir_all(&unpack)?;
    let status = std::process::Command::new("/usr/bin/ditto")
        .args(["-x", "-k"])
        .arg(zip)
        .arg(&unpack)
        .status()
        .context("couldn't unpack the update")?;
    if !status.success() {
        bail!("couldn't unpack the update");
    }
    let new_app = std::fs::read_dir(&unpack)?
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|e| e == "app"))
        .ok_or_else(|| anyhow!("the update has no app in it"))?;
    let old = bundle.with_extension("app-old");
    let _ = std::fs::remove_dir_all(&old);
    std::fs::rename(&bundle, &old).with_context(|| {
        format!(
            "couldn't replace {} (is it in a folder you can write to, like Applications?)",
            bundle.display()
        )
    })?;
    let moved = std::fs::rename(&new_app, &bundle).or_else(|_| {
        // Another volume: copy it over.
        let ok = std::process::Command::new("/usr/bin/ditto")
            .arg(&new_app)
            .arg(&bundle)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            Ok(())
        } else {
            Err(std::io::Error::other("copy failed"))
        }
    });
    if let Err(e) = moved {
        let _ = std::fs::rename(&old, &bundle);
        bail!("couldn't install the update: {e}");
    }
    let _ = std::fs::remove_dir_all(&old);
    let _ = std::fs::remove_dir_all(&unpack);
    // Start the new version once this one has quit.
    let script = format!(
        "while kill -0 {} 2>/dev/null; do sleep 0.2; done; open \"{}\"",
        std::process::id(),
        bundle.display()
    );
    std::process::Command::new("/bin/sh")
        .args(["-c", &script])
        .spawn()
        .context("couldn't restart MultiMusic")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare() {
        assert!(is_newer("0.2.0", "0.1.0"));
        assert!(is_newer("0.10.0", "0.9.9"));
        assert!(is_newer("1.0", "0.99.1"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
        assert!(is_newer("0.2.0", "0.2.0-beta"));
        assert!(!is_newer("0.2.0-beta", "0.2.0"));
        assert!(is_newer("0.2.1-beta", "0.2.0"));
    }

    #[test]
    fn releases_parse() {
        let json = serde_json::json!({
            "tag_name": "v0.3.0",
            "body": "Fixes",
            "html_url": "https://github.com/o/r/releases/tag/v0.3.0",
            "assets": [
                {"name": "MultiMusic-Setup-0.3.0-x64.exe", "url": "https://api.github.com/a/1", "size": 10},
                {"name": "MultiMusic-0.3.0-macos-arm64.zip", "url": "https://api.github.com/a/2", "size": 20},
                {"name": "MultiMusic-0.3.0-macos-intel.zip", "url": "https://api.github.com/a/4", "size": 20},
                {"name": "SHA256SUMS.txt", "url": "https://api.github.com/a/3", "size": 1}
            ]
        });
        let r = parse_release(&json).unwrap();
        assert_eq!(r.version, "0.3.0");
        assert_eq!(r.notes, "Fixes");
        assert_eq!(r.sums.unwrap().url, "https://api.github.com/a/3");
        match asset_name("0.3.0") {
            Some(name) => assert_eq!(r.asset.unwrap().name, name),
            None => assert!(r.asset.is_none()),
        }
    }

    #[test]
    fn checksums() {
        let dir = std::env::temp_dir().join(format!("multimusic-sums-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("app.zip");
        std::fs::write(&file, b"hello").unwrap();
        let hash = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        let sums = format!("{hash}  app.zip\nabc  other.zip\n");
        verify(&file, "app.zip", &sums).unwrap();
        verify(&file, "app.zip", &format!("{}  *app.zip", hash.to_uppercase())).unwrap();
        assert!(verify(&file, "app.zip", "abc  app.zip").is_err());
        assert!(verify(&file, "missing.zip", &sums).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// The whole check against a stand-in for GitHub's API, with and without a token.
    #[tokio::test]
    async fn latest_release_from_github() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = vec![0u8; 4096];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]).to_lowercase();
                let (status, body) = if !head.starts_with("get /repos/o/r/releases/latest ") {
                    ("404 Not Found", "{}".to_string())
                } else if head.contains("authorization: bearer good") {
                    ("200 OK", r#"{"tag_name":"v9.9.9","assets":[]}"#.to_string())
                } else {
                    ("404 Not Found", r#"{"message":"Not Found"}"#.to_string())
                };
                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            }
        });
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let release = latest(&http, &api, "o/r", "good").await.unwrap();
        assert_eq!(release.version, "9.9.9");
        let err = latest(&http, &api, "o/r", "").await.unwrap_err().to_string();
        assert!(err.contains("token"), "{err}");
    }
}
