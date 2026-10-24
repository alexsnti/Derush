#!/usr/bin/env bash
# Construit Derush.app et le .dmg, avec ffmpeg, ffprobe et le serveur MCP embarqués.
# À lancer sur un Mac à puce Apple avec Rust, Node et les outils en ligne de commande Xcode
# (ou sur GitHub Actions, voir .github/workflows/mac.yml). Le Mac qui installe l'app n'a besoin de rien.
set -euo pipefail
cd "$(dirname "$0")/.."

TRIPLE=$(rustc -vV | sed -n 's/^host: //p')
case "$TRIPLE" in
  aarch64-apple-darwin) ARCH=arm64 ;;
  x86_64-apple-darwin) ARCH=amd64 ;;
  *) echo "À lancer sur un Mac (triplet actuel : $TRIPLE)" >&2; exit 1 ;;
esac
BIN=app/src-tauri/binaries
mkdir -p "$BIN"

echo "==> ffmpeg et ffprobe statiques ($ARCH)"
for tool in ffmpeg ffprobe; do
  dest="$BIN/$tool-$TRIPLE"
  if [ ! -x "$dest" ]; then
    tmp=$(mktemp -d)
    curl -fsSL -o "$tmp/$tool.zip" "https://ffmpeg.martin-riedl.de/redirect/latest/macos/$ARCH/release/$tool.zip"
    unzip -q "$tmp/$tool.zip" -d "$tmp/x"
    found=$(find "$tmp/x" -type f -name "$tool" | head -1)
    [ -n "$found" ] || { echo "$tool introuvable dans l'archive" >&2; exit 1; }
    mv "$found" "$dest"
    chmod +x "$dest"
    xattr -c "$dest" 2>/dev/null || true
    rm -rf "$tmp"
  fi
  # Un binaire lié à des bibliothèques Homebrew ne marcherait pas sur un autre Mac.
  if otool -L "$dest" | grep -qE "/opt/homebrew|/usr/local"; then
    echo "$dest dépend de Homebrew : il ne serait pas autonome" >&2
    exit 1
  fi
done
"$BIN/ffmpeg-$TRIPLE" -hide_banner -version | head -1
if "$BIN/ffmpeg-$TRIPLE" -hide_banner -encoders 2>/dev/null | grep -q h264_videotoolbox; then
  echo "    encodeur matériel Apple disponible"
else
  echo "    pas d'encodeur matériel Apple dans ce build : l'export passera en x264 (plus lent)"
fi

echo "==> serveur MCP"
cargo build --release -p derush-mcp
cp target/release/derush-mcp "$BIN/derush-mcp-$TRIPLE"

echo "==> app"
cd app
if [ -f package-lock.json ]; then npm ci; else npm install; fi
npx tauri icon src-tauri/icons/icon.png >/dev/null
npx tauri build --config src-tauri/tauri.bundle.conf.json
cd ..

echo
echo "==> prêt :"
ls -1 target/release/bundle/dmg/*.dmg
