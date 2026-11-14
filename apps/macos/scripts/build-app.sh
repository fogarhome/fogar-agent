#!/bin/sh
# Builds Fogar Desktop into build/Fogar Desktop.app and signs it.
#   scripts/build-app.sh            release build
#   scripts/build-app.sh --debug    debug build (honours FOGAR_AGENT_SOCKET, for the stand-in agent)
# Signing: the first "Apple Development" identity in the keychain (free Personal Team), with the
# hardened runtime the agent's peer check requires; ad hoc ("-") when there is none, in which case
# the agent treats the app as uid_only and refuses pair/unpair (ADR-0002 amendment).
# FOGAR_SIGN_IDENTITY overrides the identity.
set -eu
here=$(cd "$(dirname "$0")/.." && pwd)
cd "$here"
config=release
[ "${1:-}" = "--debug" ] && config=debug

swift build -c "$config" --product FogarDesktop
bin=$(swift build -c "$config" --show-bin-path)/FogarDesktop

app="$here/build/Fogar Desktop.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources/Fonts"
cp "$bin" "$app/Contents/MacOS/FogarDesktop"
cp Sources/FogarDesktop/Info.plist "$app/Contents/Info.plist"
cp Resources/Fonts/* "$app/Contents/Resources/Fonts/"

# Icon: Resources/AppIcon.png (scripts/make-icon.swift) -> .icns.
iconset=$(mktemp -d)/AppIcon.iconset
mkdir -p "$iconset"
for s in 16 32 128 256 512; do
  sips -z $s $s Resources/AppIcon.png --out "$iconset/icon_${s}x${s}.png" >/dev/null
  d=$((s * 2))
  sips -z $d $d Resources/AppIcon.png --out "$iconset/icon_${s}x${s}@2x.png" >/dev/null
done
iconutil -c icns "$iconset" -o "$app/Contents/Resources/AppIcon.icns"
rm -rf "$(dirname "$iconset")"

identity=${FOGAR_SIGN_IDENTITY:-$(security find-identity -v -p codesigning 2>/dev/null | awk -F'"' '/Apple Development/ {print $2; exit}')}
if [ -z "$identity" ]; then
  identity=-
  echo "warning: no Apple Development identity: signing ad hoc; the agent will refuse pair/unpair (uid_only)" >&2
fi
codesign --force --options runtime --timestamp=none --sign "$identity" "$app"
codesign --verify --strict "$app"
echo "built $app ($config, signed: $identity)"
