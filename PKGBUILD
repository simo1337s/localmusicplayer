# Maintainer: simo1337s
# Builds Medley from this checkout:  makepkg -si
pkgname=medley
pkgver=0.1.0
pkgrel=1
pkgdesc="Lightweight native music player for local files, Spotify and SoundCloud with synced lyrics, Discord Rich Presence and Last.fm scrobbling"
arch=('x86_64' 'aarch64')
url="https://github.com/simo1337s/localmusicplayer"
license=('MIT')
depends=('mpv' 'alsa-lib' 'openssl' 'libxkbcommon' 'libglvnd' 'wayland' 'libx11' 'libxcursor' 'libxrandr' 'libxi'
         'hicolor-icon-theme' 'gcc-libs' 'glibc')
makedepends=('cargo')
optdepends=('pipewire-alsa: Spotify playback on PipeWire systems (default ALSA output)'
            'noto-fonts-cjk: Japanese, Chinese and Korean song titles'
            'inter-font: nicer interface font'
            'discord: Rich Presence (also works with Vesktop / arRPC)')
options=('!lto' '!debug')

_root() {
  # The PKGBUILD lives in the repository root.
  printf '%s' "$startdir"
}

pkgver() {
  cd "$(_root)"
  local v
  v=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
  if git rev-parse --git-dir >/dev/null 2>&1; then
    printf '%s.r%s.g%s' "$v" "$(git rev-list --count HEAD)" "$(git rev-parse --short HEAD)"
  else
    printf '%s' "$v"
  fi
}

prepare() {
  cd "$(_root)"
  export RUSTUP_TOOLCHAIN=stable
  cargo fetch --locked --target "$(rustc -vV | sed -n 's/host: //p')"
}

build() {
  cd "$(_root)"
  export RUSTUP_TOOLCHAIN=stable
  cargo build --frozen --release
}

check() {
  cd "$(_root)"
  export RUSTUP_TOOLCHAIN=stable
  cargo test --frozen --release
}

package() {
  cd "$(_root)"
  install -Dm755 target/release/medley "$pkgdir/usr/bin/medley"
  install -Dm644 packaging/medley.desktop "$pkgdir/usr/share/applications/medley.desktop"
  install -Dm644 packaging/medley.svg "$pkgdir/usr/share/icons/hicolor/scalable/apps/medley.svg"
  install -Dm644 LICENSE "$pkgdir/usr/share/licenses/$pkgname/LICENSE"
}
