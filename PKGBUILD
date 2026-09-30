# PKGBUILD для Arch Linux / EndeavourOS
#
# Локальная сборка:  makepkg -si
# (требует nodejs, pnpm, rust; профильная группа `input` и udev-правило
#  ставятся пакетом, перелогин необходим)
#
# После первого релизного тега v0.5.2+ перейдите на тарбол:
#   source=("$pkgname-$pkgver.tar.gz::$url/archive/refs/tags/v$pkgver.tar.gz")
#   sha256sums=(...)   # makepkg -g

pkgname=keymaster-linux
pkgver=0.5.1
pkgrel=1
pkgdesc='Keyboard & mouse automation for Linux — remapping, macros, layers, text expansion'
arch=('x86_64')
url='https://github.com/zsanya322-maker/keymaster-linux'
license=('custom:FCL')
depends=(
  'webkit2gtk-4.1'
  'gtk3'
  'libayatana-appindicator'
  'librsvg'
  'hicolor-icon-theme'
)
makedepends=('cargo' 'nodejs' 'pnpm')
optdepends=('openssl: автообновления через Tauri updater')
install=keymaster-linux.install
source=("git+$url.git")
sha256sums=('SKIP')
_branch=main

pkgver() {
  cd "$srcdir/$pkgname"
  git describe --tags --always | sed 's/^v//; s/-/./g'
}

prepare() {
  cd "$srcdir/$pkgname"
  git checkout "$_branch" 2>/dev/null || true
}

build() {
  cd "$srcdir/$pkgname"
  export NODE_ENV=production
  pnpm install --frozen-lockfile
  pnpm run build
  cd src-tauri
  cargo build --release --locked --frozen
}

package() {
  cd "$srcdir/$pkgname"
  install -Dm755 src-tauri/target/release/keymaster-linux \
    "$pkgdir/usr/bin/keymaster-linux"

  # udev-правило для /dev/uinput
  install -Dm644 packaging/udev/70-keymaster-linux.rules \
    "$pkgdir/usr/lib/udev/rules.d/70-keymaster-linux.rules"

  # .desktop
  install -d "$pkgdir/usr/share/applications"
  cat > "$pkgdir/usr/share/applications/keymaster-linux.desktop" <<'EOF'
[Desktop Entry]
Type=Application
Name=KeyMaster Linux
GenericName=Keyboard & Mouse Automation
Comment=Remapping, macros, layers and text expansion
Exec=keymaster-linux
Icon=keymaster-linux
Categories=Utility;Accessibility;
StartupNotify=false
X-KDE-autostart-after=panel
EOF

  # Иконки
  install -Dm644 src-tauri/icons/32x32.png \
    "$pkgdir/usr/share/icons/hicolor/32x32/apps/keymaster-linux.png"
  install -Dm644 src-tauri/icons/128x128.png \
    "$pkgdir/usr/share/icons/hicolor/128x128/apps/keymaster-linux.png"
  install -Dm644 src-tauri/icons/128x128@2x.png \
    "$pkgdir/usr/share/icons/hicolor/256x256/apps/keymaster-linux.png"

  # Лицензия
  install -Dm644 LICENSE "$pkgdir/usr/share/licenses/$pkgname/LICENSE"
}
