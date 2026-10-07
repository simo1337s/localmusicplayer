//! mpv playback engine (local files and SoundCloud streams), controlled over its JSON IPC socket.
//!
//! mpv is spawned once as an audio-only, config-less child process. It handles every codec,
//! HLS streams, ReplayGain, gapless playback and outputs straight to PipeWire/PulseAudio/ALSA.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::UnixStream;
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot};

#[derive(Debug, Clone, PartialEq)]
pub enum MpvEvent {
    /// A new file started (playlist advanced, possibly gaplessly).
    StartFile { playlist_entry_id: i64 },
    FileLoaded,
    EndFile { reason: String, error: Option<String> },
    Pause(bool),
    Duration(f64),
    /// The mpv process exited or the socket closed.
    Died,
}

#[derive(Debug, Clone)]
pub struct MpvOptions {
    pub binary: String,
    pub volume: f32,
    pub replaygain: bool,
    pub gapless: bool,
    pub audio_device: String,
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>>;

pub struct Mpv {
    child: Child,
    writer: tokio::sync::Mutex<OwnedWriteHalf>,
    pending: Pending,
    next_id: AtomicU64,
    socket: PathBuf,
}

impl Mpv {
    pub async fn spawn(opts: &MpvOptions, events: mpsc::UnboundedSender<MpvEvent>) -> Result<Mpv> {
        let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let name = format!("medley-mpv-{}.sock", std::process::id());
        let mut socket = runtime_dir.join(&name);
        // Unix socket paths are limited to ~108 bytes.
        if socket.as_os_str().len() > 100 {
            socket = PathBuf::from("/tmp").join(&name);
        }
        let _ = std::fs::remove_file(&socket);

        let mut cmd = Command::new(&opts.binary);
        cmd.arg("--no-config")
            .arg("--idle=yes")
            .arg("--video=no")
            .arg("--audio-display=no")
            .arg("--no-terminal")
            .arg("--ytdl=no")
            .arg("--keep-open=no")
            .arg("--prefetch-playlist=yes")
            .arg("--audio-client-name=Medley")
            .arg("--cache=yes")
            // Keep mpv's memory small: a few MB of demuxer buffer is plenty for audio.
            .arg("--demuxer-max-bytes=8MiB")
            .arg("--demuxer-max-back-bytes=2MiB")
            .arg("--volume-max=100")
            .arg(format!("--volume={}", opts.volume.clamp(0.0, 100.0)))
            .arg(format!("--gapless-audio={}", if opts.gapless { "weak" } else { "no" }))
            .arg(format!("--replaygain={}", if opts.replaygain { "track" } else { "no" }))
            .arg(format!("--input-ipc-server={}", socket.display()))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        // Make mpv exit with us even if we're killed without a chance to clean up.
        #[cfg(target_os = "linux")]
        unsafe {
            cmd.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                Ok(())
            });
        }
        if !opts.audio_device.is_empty() {
            cmd.arg(format!("--audio-device={}", opts.audio_device));
        }
        let child = cmd
            .spawn()
            .with_context(|| format!("could not start `{}` (is mpv installed?)", opts.binary))?;

        // Wait for the IPC socket to show up.
        let mut stream = None;
        for _ in 0..100 {
            if let Ok(s) = UnixStream::connect(&socket).await {
                stream = Some(s);
                break;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        let stream = stream.ok_or_else(|| anyhow!("mpv did not open its IPC socket"))?;
        let (read, write) = stream.into_split();

        let pending: Pending = Arc::default();
        let reader_pending = pending.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(read).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if let Some(id) = msg.get("request_id").and_then(Value::as_u64) {
                    if let Some(tx) = reader_pending.lock().unwrap().remove(&id) {
                        let ok = msg.get("error").and_then(Value::as_str) == Some("success");
                        let _ = tx.send(if ok {
                            Ok(msg.get("data").cloned().unwrap_or(Value::Null))
                        } else {
                            Err(anyhow!(
                                "mpv: {}",
                                msg.get("error").and_then(Value::as_str).unwrap_or("error")
                            ))
                        });
                    }
                    continue;
                }
                if let Some(ev) = parse_event(&msg) {
                    if events.send(ev).is_err() {
                        break;
                    }
                }
            }
            let _ = events.send(MpvEvent::Died);
        });

        let mpv = Mpv {
            child,
            writer: tokio::sync::Mutex::new(write),
            pending,
            next_id: AtomicU64::new(1),
            socket,
        };
        mpv.command(json!(["observe_property", 1, "pause"])).await?;
        mpv.command(json!(["observe_property", 2, "duration"])).await?;
        Ok(mpv)
    }

    pub async fn command(&self, args: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let mut line = serde_json::to_vec(&json!({ "command": args, "request_id": id }))?;
        line.push(b'\n');
        {
            let mut w = self.writer.lock().await;
            if let Err(e) = w.write_all(&line).await {
                self.pending.lock().unwrap().remove(&id);
                bail!("mpv IPC write failed: {e}");
            }
        }
        match tokio::time::timeout(Duration::from_secs(5), rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => bail!("mpv closed"),
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                bail!("mpv did not answer")
            }
        }
    }

    /// Replaces the playlist with `url` and starts playing at `start` seconds.
    pub async fn load(&self, url: &str, start: f64) -> Result<()> {
        if start > 0.5 {
            let opt = format!("start={start:.1}");
            // mpv >= 0.38 takes an insertion index before the per-file options; older versions don't.
            if self.command(json!(["loadfile", url, "replace", -1, opt])).await.is_err() {
                self.command(json!(["loadfile", url, "replace", opt])).await?;
            }
        } else {
            self.command(json!(["loadfile", url, "replace"])).await?;
        }
        self.set_pause(false).await
    }

    /// Appends `url` so mpv can continue into it gaplessly.
    pub async fn append(&self, url: &str) -> Result<()> {
        self.command(json!(["loadfile", url, "append"])).await.map(|_| ())
    }

    /// Removes every playlist entry except the current one.
    pub async fn clear_upcoming(&self) -> Result<()> {
        self.command(json!(["playlist-clear"])).await.map(|_| ())
    }

    pub async fn set_pause(&self, pause: bool) -> Result<()> {
        self.command(json!(["set_property", "pause", pause])).await.map(|_| ())
    }

    pub async fn seek(&self, secs: f64) -> Result<()> {
        self.command(json!(["seek", secs.max(0.0), "absolute"])).await.map(|_| ())
    }

    pub async fn set_volume(&self, volume: f32) -> Result<()> {
        self.command(json!(["set_property", "volume", volume.clamp(0.0, 100.0)]))
            .await
            .map(|_| ())
    }

    pub async fn stop(&self) -> Result<()> {
        self.command(json!(["stop"])).await.map(|_| ())
    }

    pub async fn time_pos(&self) -> Option<f64> {
        self.command(json!(["get_property", "time-pos"])).await.ok()?.as_f64()
    }

    pub async fn quit(mut self) {
        let _ = self.command(json!(["quit"])).await;
        let _ = tokio::time::timeout(Duration::from_secs(1), self.child.wait()).await;
        let _ = std::fs::remove_file(&self.socket);
    }
}

impl Drop for Mpv {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket);
    }
}

fn parse_event(msg: &Value) -> Option<MpvEvent> {
    match msg.get("event")?.as_str()? {
        "start-file" => Some(MpvEvent::StartFile {
            playlist_entry_id: msg.get("playlist_entry_id").and_then(Value::as_i64).unwrap_or(0),
        }),
        "file-loaded" => Some(MpvEvent::FileLoaded),
        "end-file" => Some(MpvEvent::EndFile {
            reason: msg.get("reason").and_then(Value::as_str).unwrap_or("").to_string(),
            error: msg.get("file_error").and_then(Value::as_str).map(str::to_string),
        }),
        "property-change" => match msg.get("name")?.as_str()? {
            "pause" => Some(MpvEvent::Pause(msg.get("data")?.as_bool()?)),
            "duration" => Some(MpvEvent::Duration(msg.get("data")?.as_f64()?)),
            _ => None,
        },
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ipc_events() {
        let ev = |s: &str| parse_event(&serde_json::from_str(s).unwrap());
        assert_eq!(
            ev(r#"{"event":"end-file","reason":"eof","playlist_entry_id":1}"#),
            Some(MpvEvent::EndFile { reason: "eof".into(), error: None })
        );
        assert_eq!(
            ev(r#"{"event":"end-file","reason":"error","file_error":"unrecognized file format"}"#),
            Some(MpvEvent::EndFile { reason: "error".into(), error: Some("unrecognized file format".into()) })
        );
        assert_eq!(ev(r#"{"event":"property-change","id":1,"name":"pause","data":true}"#), Some(MpvEvent::Pause(true)));
        assert_eq!(
            ev(r#"{"event":"property-change","id":2,"name":"duration","data":201.5}"#),
            Some(MpvEvent::Duration(201.5))
        );
        assert_eq!(ev(r#"{"event":"property-change","id":2,"name":"duration"}"#), None);
        assert_eq!(ev(r#"{"event":"start-file","playlist_entry_id":3}"#), Some(MpvEvent::StartFile { playlist_entry_id: 3 }));
    }

    /// Talks to a real mpv if one is installed: plays a generated WAV file.
    #[tokio::test]
    async fn real_mpv_roundtrip() {
        if std::process::Command::new("mpv").arg("--version").output().is_err() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("medley-mpv-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let wav = dir.join("tone.wav");
        write_test_wav(&wav, 2.0);

        let (tx, mut rx) = mpsc::unbounded_channel();
        let opts = MpvOptions {
            binary: "mpv".into(),
            volume: 0.0,
            replaygain: false,
            gapless: true,
            audio_device: String::new(),
        };
        let mpv = Mpv::spawn(&opts, tx).await.unwrap();
        // Use the null audio output so the test needs no sound card.
        mpv.command(json!(["set_property", "ao", "null"])).await.unwrap();
        mpv.load(&wav.to_string_lossy(), 0.0).await.unwrap();
        let mut got_loaded = false;
        let mut got_eof = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while let Ok(Some(ev)) = tokio::time::timeout_at(deadline, rx.recv()).await {
            match ev {
                MpvEvent::FileLoaded => {
                    got_loaded = true;
                    assert!(mpv.time_pos().await.is_some());
                }
                MpvEvent::EndFile { reason, .. } if reason == "eof" => {
                    got_eof = true;
                    break;
                }
                _ => {}
            }
        }
        mpv.quit().await;
        std::fs::remove_dir_all(dir).unwrap();
        assert!(got_loaded && got_eof);
    }

    fn write_test_wav(path: &std::path::Path, secs: f32) {
        let rate = 8000u32;
        let n = (rate as f32 * secs) as u32;
        let mut data = Vec::with_capacity(44 + n as usize * 2);
        data.extend_from_slice(b"RIFF");
        data.extend_from_slice(&(36 + n * 2).to_le_bytes());
        data.extend_from_slice(b"WAVEfmt ");
        data.extend_from_slice(&16u32.to_le_bytes());
        data.extend_from_slice(&1u16.to_le_bytes());
        data.extend_from_slice(&1u16.to_le_bytes());
        data.extend_from_slice(&rate.to_le_bytes());
        data.extend_from_slice(&(rate * 2).to_le_bytes());
        data.extend_from_slice(&2u16.to_le_bytes());
        data.extend_from_slice(&16u16.to_le_bytes());
        data.extend_from_slice(b"data");
        data.extend_from_slice(&(n * 2).to_le_bytes());
        for i in 0..n {
            let s = ((i as f32 * 440.0 * std::f32::consts::TAU / rate as f32).sin() * 8000.0) as i16;
            data.extend_from_slice(&s.to_le_bytes());
        }
        std::fs::write(path, data).unwrap();
    }
}
