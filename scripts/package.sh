#!/usr/bin/env bash
# Packaging local de kioku. Aucune publication : le script produit des archives
# dans un dossier ; Cargo utilise aussi son repertoire de build habituel.
# Aucun jeton, aucune identite, aucun registre ne sont touches.
#
#   scripts/package.sh [dossier-de-sortie] [cible]
#
# Sans cible, l'hote est deduit de uname (usage local). Avec une cible, le nom
# publie vient du triple, jamais de uname : les runners de la matrice de
# publication construisent chacun leur cible et le nom ne depend que d'elle.
#
#   scripts/package.sh dist aarch64-unknown-linux-gnu
#
# Le contenu de l'archive est fige plus bas ; chaque lien relatif qu'il porte
# doit se resoudre a l'interieur, sinon le packaging echoue.
#
# Codes de sortie :
#   0  archive construite et verifiee ;
#   2  usage invalide ou cible non supportee ;
#   3  construction, lien casse, archivage ou verification en echec.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

OUT="${1:-dist}"
case "$OUT" in
  /*) ;;
  *) OUT="$PWD/$OUT" ;;
esac
TARGET="${2:-}"
TARGET_DIR="${CARGO_TARGET_DIR:-target}"

cd "$REPO_ROOT"
# Cargo is provided through the explicitly configured PATH; HOME may be private.
command -v cargo >/dev/null || { echo "cargo absent from PATH" >&2; exit 3; }

# Version du paquet : premiere ligne "version = " de Cargo.toml.
VERSION="$(awk -F'"' '/^version = "/{print $2; exit}' Cargo.toml)"
[ -n "$VERSION" ] || { echo "version introuvable dans Cargo.toml" >&2; exit 2; }
NAME="kioku-$VERSION"

# Empreinte : sha256sum sur Linux, shasum sur macOS.
sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d' ' -f1
  else
    shasum -a 256 "$1" | cut -d' ' -f1
  fi
}

# Hote reel, pour savoir si le binaire de l'archive peut etre demarre ici.
case "$(uname -s)" in
  Darwin) HOST_OS=darwin ;;
  Linux) HOST_OS=linux ;;
  MINGW*|MSYS*|CYGWIN*) HOST_OS=windows ;;
  *) HOST_OS=inconnu ;;
esac
case "$(uname -m)" in
  arm64|aarch64) HOST_MACH=aarch64 ;;
  x86_64|amd64|AMD64) HOST_MACH=x86_64 ;;
  *) HOST_MACH=inconnu ;;
esac

# Plateforme publiee et architecture publiee : le triple cible fait foi.
if [ -n "$TARGET" ]; then
  case "$TARGET" in
    x86_64-unknown-linux-*) OS=linux; MACH=x86_64 ;;
    aarch64-unknown-linux-*) OS=linux; MACH=aarch64 ;;
    aarch64-apple-darwin) OS=darwin; MACH=aarch64 ;;
    x86_64-apple-darwin) OS=darwin; MACH=x86_64 ;;
    x86_64-pc-windows-*) OS=windows; MACH=x86_64 ;;
    *) echo "cible non supportee: $TARGET" >&2; exit 2 ;;
  esac
else
  OS="$HOST_OS"
  MACH="$HOST_MACH"
  [ "$OS" != inconnu ] || { echo "systeme non supporte: $(uname -s)" >&2; exit 2; }
  [ "$MACH" != inconnu ] || { echo "architecture non supportee: $(uname -m)" >&2; exit 2; }
fi

# Nom publie : seul Apple silicon s'ecrit arm64, tout le reste suit le triple.
case "$OS-$MACH" in
  darwin-aarch64) ARCH=arm64; EXT=tar.gz; BIN=kioku ;;
  darwin-x86_64) ARCH=x86_64; EXT=tar.gz; BIN=kioku ;;
  linux-aarch64|linux-x86_64) ARCH="$MACH"; EXT=tar.gz; BIN=kioku ;;
  windows-x86_64) ARCH=x86_64; EXT=zip; BIN=kioku.exe ;;
  *) echo "couple non supporte: $OS-$MACH" >&2; exit 2 ;;
esac
ARCHIVE="$OUT/$NAME-$OS-$ARCH.$EXT"

if [ -n "$TARGET" ]; then
  echo "== build $TARGET"
  cargo build --release --locked --target "$TARGET"
  BIN_DIR="$TARGET_DIR/$TARGET/release"
else
  echo "== build hote"
  cargo build --release --locked
  BIN_DIR="$TARGET_DIR/release"
fi

mkdir -p "$OUT"
[ ! -e "$OUT/stage" ] || { echo "stage already exists: $OUT/stage" >&2; exit 2; }
STAGE="$OUT/stage"
VERIFY=""
trap 'rm -rf "$STAGE" ${VERIFY:+"$VERIFY"}' EXIT

[ -f "$BIN_DIR/$BIN" ] || { echo "binaire absent: $BIN_DIR/$BIN" >&2; exit 3; }
mkdir "$STAGE"
cp "$BIN_DIR/$BIN" "$STAGE/$BIN"

# The archive contains the binary and every document the shipped pages link:
# tests, agent instructions and CI files stay in the repository.
for file in LICENSE README.md README.fr.md CHANGELOG.md CONTRIBUTING.md SECURITY.md \
  docs/usage.md docs/configuration.md docs/fr/usage.md docs/fr/configuration.md \
  docs/REPLAY.md docs/demo-session.txt docs/demo.sh \
  docs/demo-helper-aarch64.S \
  docs/assets/banner-light.jpg docs/assets/banner-dark.jpg; do
  [ -f "$file" ] || { echo "missing release document: $file" >&2; exit 3; }
  mkdir -p "$STAGE/$(dirname "$file")"
  cp "$file" "$STAGE/$file"
done

# Un lien relatif sans cible ferait une archive qui renvoie dans le vide.
command -v python3 >/dev/null || { echo "python3 absent from PATH" >&2; exit 3; }
python3 scripts/check-archive-links.py "$STAGE" ||
  { echo "ECHEC: lien relatif casse dans l'archive" >&2; exit 3; }

case "$EXT" in
  zip)
    if command -v zip >/dev/null 2>&1; then
      (cd "$STAGE" && zip -q -r "$ARCHIVE" .)
    else
      command -v 7z >/dev/null 2>&1 || { echo "zip and 7z absent from PATH" >&2; exit 3; }
      (cd "$STAGE" && 7z a -tzip "$ARCHIVE" . >/dev/null)
    fi
    ;;
  *) tar -czf "$ARCHIVE" -C "$STAGE" . ;;
esac
rm -rf "$STAGE"

[ -f "$ARCHIVE" ] || { echo "ECHEC: archive absente: $ARCHIVE" >&2; exit 3; }

# Verification de l'archive : on la decompresse dans un dossier jetable et, si
# l'hote correspond a la cible, on demarre le binaire qu'elle contient. Une
# archive qui echoue ici est supprimee : rien de non verifie ne reste a publier.
fail() {
  rm -rf "$STAGE" "$VERIFY"
  rm -f "$ARCHIVE"
  echo "ECHEC: $*" >&2
  exit 3
}
VERIFY="$(mktemp -d)"
case "$ARCHIVE" in
  *.zip)
    if command -v unzip >/dev/null 2>&1; then
      (cd "$VERIFY" && unzip -q "$ARCHIVE") || fail "archive illisible: $ARCHIVE"
    else
      7z t -tzip "$ARCHIVE" >/dev/null || fail "archive illisible: $ARCHIVE"
      (cd "$VERIFY" && 7z x -tzip "$ARCHIVE" >/dev/null) || fail "archive illisible: $ARCHIVE"
    fi
    ;;
  *) (cd "$VERIFY" && tar -xzf "$ARCHIVE") || fail "archive illisible: $ARCHIVE" ;;
esac
[ -f "$VERIFY/$BIN" ] || fail "binaire absent de l'archive: $BIN"
if [ "$HOST_OS" = "$OS" ] && [ "$HOST_MACH" = "$MACH" ]; then
  "$VERIFY/$BIN" --version >/dev/null || fail "le binaire de l'archive ne demarre pas"
  echo "== controle: archive decompressee, binaire execute (--version) -> OK"
else
  echo "== controle: binaire $OS/$ARCH non executable sur $HOST_OS/$HOST_MACH, demarrage ignore"
fi

SHA="$(sha256_of "$ARCHIVE")"
printf '%s  %s\n' "$SHA" "$(basename "$ARCHIVE")" > "$OUT/SHA256SUMS"

echo "== archive : $ARCHIVE"
echo "== sha256  : $SHA"
echo "== aucune publication effectuee"
