# Medley

A fast, lightweight, native music player for Arch Linux that puts **your local files, Spotify, SoundCloud and your Apple Music library** in one place, with **synced lyrics**, **Discord Rich Presence**, **Last.fm scrobbling** and media-key / MPRIS support.

It's written in Rust with [egui](https://github.com/emilk/egui), like [Spotifast](https://spotifast.rocks/). There's no Electron and no web view, so it starts instantly and stays small in memory.

![Home](docs/screenshots/home.png)

| Now playing + synced lyrics | Playlist |
| --- | --- |
| ![Now playing](docs/screenshots/now-playing.png) | ![Playlist](docs/screenshots/playlist.png) |

## Features

- **One library for every source**
  - **Local files**: MP3, FLAC, Opus, OGG, M4A/ALAC, WAV, AIFF, APE, WavPack and more. Tags and embedded or folder cover art are read automatically, and rescans are incremental.
  - **Spotify**: imports all your playlists and Liked Songs and plays them in the app through [librespot](https://github.com/librespot-org/librespot) (Spotify Premium is needed for playback). Gapless playback, 96/160/320 kbps, volume normalisation.
  - **SoundCloud**: imports your likes and playlists (your own and liked ones), searches the catalogue and plays streams.
  - **Apple Music**: imports your library and playlists from a `Library.xml` export or the Apple Music API. Apple Music streams are DRM-protected and can't play on Linux, so each song plays from a matching local file, Spotify track or SoundCloud upload. Medley finds the match automatically and remembers it.
  - **M3U/M3U8** playlist import and export.
- **Custom playlists that mix sources**: drop a Spotify song, a SoundCloud upload and a FLAC into the same playlist. A cross-source **Liked Songs** collection (♥) also syncs likes back to Spotify and Last.fm.
- **Synced lyrics**: reads `.lrc` files next to your music, then embedded lyrics tags, then [LRCLIB](https://lrclib.net). Lyrics are shown in a side panel and in a full-screen *Now playing* view with a large cover. Click a line to jump to it.
- **Discord Rich Presence** shows "Listening to <song>" with the album cover, a progress bar and an "Open in Spotify/SoundCloud" button. Works with the Discord app, Vesktop and arRPC.
- **Last.fm scrobbling** follows the official rules (half the track or 4 minutes), sends "now playing" updates, and queues scrobbles offline to send later.
- **MPRIS / media keys**: works with `playerctl`, waybar, polybar, KDE/GNOME media widgets and headset buttons.
- **A good-looking UI**: a dark, Spotify-style layout whose accent colour follows the current album cover. Includes a queue, search across every source, an albums grid, gapless playback, ReplayGain, session restore, and drag and drop (drop a folder, `.m3u` or `Library.xml` onto the window).
- **Low memory use**: cover art is decoded at the size it's drawn and kept in a small LRU cache, fonts are memory-mapped from your system, and only two runtime threads are used. The window doesn't redraw at all while nothing changes.

## Install (Arch Linux)

```sh
sudo pacman -S --needed base-devel git rust mpv
git clone https://github.com/simo1337s/localmusicplayer.git
cd localmusicplayer
makepkg -si          # builds and installs the `medley` package
```

Then launch **Medley** from your app launcher, or run `medley`.

Optional extras:

```sh
sudo pacman -S pipewire-alsa noto-fonts-cjk inter-font
```

- `pipewire-alsa`: Spotify output on PipeWire systems.
- `noto-fonts-cjk`: Japanese, Chinese and Korean titles.
- `inter-font`: a nicer UI font. Medley uses Inter, Noto Sans, Cantarell or DejaVu, whichever is installed.

To run without packaging: `cargo build --release && ./target/release/medley`.

## Setting up your accounts

Everything is under **Settings** (gear icon in the sidebar).

| Service | What to do |
| --- | --- |
| **Local files** | `~/Music` is scanned by default. Add more folders in Settings, or drag a folder onto the window. |
| **Spotify** | Click **Log in with Spotify**. Your browser opens Spotify's login page; approve it and your playlists and Liked Songs import automatically. Playback needs **Spotify Premium**. If you ever hit rate limits, create your own app at developer.spotify.com (redirect URI `http://127.0.0.1:8898/login`) and paste its client ID under *Advanced*. |
| **SoundCloud** | Paste your profile URL (`https://soundcloud.com/you`) and click **Sync**. Private likes and playlists also need your `oauth_token` cookie from soundcloud.com (DevTools → Application → Cookies). |
| **Apple Music** | On a Mac or in iTunes on Windows: *File → Library → Export Library…*, then drop the `Library.xml` onto Medley or paste its path. You can also paste your `media-user-token` cookie from music.apple.com and use *Import with the Apple Music API*. Songs play from local files, Spotify or SoundCloud. |
| **Last.fm** | Create an API account at [last.fm/api/account/create](https://www.last.fm/api/account/create), paste the key and secret, click **Connect account** and approve in the browser. |
| **Discord** | Create an application at [discord.com/developers/applications](https://discord.com/developers/applications) (call it "Medley", or anything you like), and paste its **Application ID**. |

## Keyboard shortcuts

| Key | Action |
| --- | --- |
| `Space` | Play / pause |
| `←` / `→` | Seek −5 s / +5 s |
| `Ctrl+←` / `Ctrl+→` | Previous / next track |
| `↑` / `↓` | Volume up / down |
| `Ctrl+F` | Search |
| `L` | Toggle the full-screen *Now playing* / lyrics view |
| `Esc` | Leave the *Now playing* view |
| `Ctrl+Q` | Quit |

Double-click a song to play it. Right-click a song for *Play next*, *Add to queue*, *Add to playlist*, *Like* and *Open in Spotify / Show in folder*.

## Memory use

I measured this in a VM while a local song was playing, with the album view and lyrics open:

| Process | Resident memory |
| --- | --- |
| `medley` | ~157 MB in total, but ~66 MB of that is the VM's *software* OpenGL renderer (llvmpipe). Expect roughly **~90 MB** with a real GPU. I estimated that figure and haven't measured it on real hardware. |
| `mpv` (playback of local files & SoundCloud) | ~54 MB RSS, of which only ~13 MB is private; the rest is shared ffmpeg libraries. It only runs while you play local files or SoundCloud. |

Spotify playback runs inside the Medley process (librespot) and doesn't start another process. The official Spotify client usually uses 400–800 MB.

To keep memory low: lower *Covers kept in memory* in Settings → Appearance, and close the lyrics panel (it repaints 10× per second while playing, compared with twice per second otherwise).

## Files

| What | Where |
| --- | --- |
| Settings | `~/.config/medley/config.toml` (also editable by hand) |
| Library database, Spotify login, scrobble queue | `~/.local/share/medley/` |
| Cover art and lyrics cache | `~/.cache/medley/` |

Logs: run `MEDLEY_LOG=medley=debug medley` in a terminal.

## Troubleshooting

- **No sound from Spotify on PipeWire**: install `pipewire-alsa`. You can also build with PulseAudio output: `cargo build --release --features pulseaudio` (needs `libpulse`).
- **Spotify says Premium is required**: playlists and search work on free accounts, but librespot can only stream with Premium.
- **Some SoundCloud tracks won't play**: SoundCloud Go+ tracks only offer 30-second previews to third-party apps, and some tracks are region-locked.
- **Japanese/Korean/Chinese text shows boxes**: install `noto-fonts-cjk`. Medley loads a CJK font only when your library needs it.
- **Media keys don't work**: Medley registers as `org.mpris.MediaPlayer2.medley`; check with `playerctl -l`. It needs a D-Bus session, which every normal desktop session has.
- **Interface too small or too large**: Settings → Appearance → *Interface scale*.

## How it works

```
src/
  main.rs              window + runtime setup
  service.rs           background service: playback state machine, sync jobs, integrations
  player/              play queue, mpv (JSON IPC) engine, librespot engine + Spotify OAuth
  library/             SQLite store, incremental tag scanner (lofty), M3U import/export
  providers/           Spotify Web API, SoundCloud api-v2, Apple Music (XML + API)
  integrations/        lyrics (LRC/LRCLIB), Last.fm, Discord RPC, MPRIS
  ui/                  egui views, theme, widgets, cover art cache
```

The UI thread only draws. Playback, network and disk work run on a small tokio runtime and publish snapshots that the UI reads each frame.

## Disclaimer

Medley is an unofficial client and isn't affiliated with Spotify, SoundCloud, Apple or Discord. Spotify support uses librespot. Use the integrations in line with each service's terms.

## License

MIT
