#!/bin/sh
# Builds Fogar Desktop into build/Fogar Desktop.app and signs it.
#   scripts/build-app.sh            release build, with the Rust agent inside (S2 PR 6):
#                                   Contents/MacOS/fogar-agentd + its LaunchAgent plist in
#                                   Contents/Library/LaunchAgents/com.fogar.agent.plist (BundleProgram),
#                                   registered by the app with SMAppService.agent
#   scripts/build-app.sh --debug    debug build (honours FOGAR_AGENT_SOCKET and FOGAR_AGENT_UNSIGNED,
#                                   for the stand-in agent): "Fogar Desktop (debug).app", bundle id
#                                   com.fogar.desktop.debug, so it is never mistaken for the real app.
#                                   No agent inside: it talks to the stand-in or to an agent run by hand
# Signing: the first "Apple Development" identity in the keychain (free Personal Team), with the
# hardened runtime the peer checks require, inside out and without --deep: first the agent (identifier
# com.fogar.agent, microphone entitlement, never get-task-allow), then the app (microphone
# entitlement + NSMicrophoneUsageDescription: macOS attributes the agent's microphone to the app).
# Ad hoc ("-") when there is no identity: then neither side trusts the other beyond the uid
# (the agent refuses pair/unpair, the app refuses the conversation; ADR-0002 amendment).
# FOGAR_SIGN_IDENTITY overrides the identity. CARGO_TARGET_DIR is honoured for the agent build.
set -eu
here=$(cd "$(dirname "$0")/.." && pwd)
cd "$here"
config=release
[ "${1:-}" = "--debug" ] && config=debug
repo=$(cd "$here/../.." && pwd)

swift build -c "$config" --product FogarDesktop
bin=$(swift build -c "$config" --show-bin-path)/FogarDesktop

if [ "$config" = debug ]; then
  app="$here/build/Fogar Desktop (debug).app"
else
  app="$here/build/Fogar Desktop.app"
fi
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources/Fonts"
cp "$bin" "$app/Contents/MacOS/FogarDesktop"
cp Sources/FogarDesktop/Info.plist "$app/Contents/Info.plist"
if [ "$config" = debug ]; then
  /usr/libexec/PlistBuddy -c "Set :CFBundleIdentifier com.fogar.desktop.debug" \
    -c "Set :CFBundleName Fogar Desktop (debug)" -c "Set :CFBundleDisplayName Fogar Desktop (debug)" \
    "$app/Contents/Info.plist"
fi
cp Resources/Fonts/* "$app/Contents/Resources/Fonts/"

# The agent (release only). Its Info.plist (identifier, microphone purpose string) is linked into
# the binary's __TEXT,__info_plist section: a bare executable has no bundle of its own. The linker
# argument cannot hold spaces, so the plist goes through a temporary path without them.
if [ "$config" = release ]; then
  agent_plist_dir=$(mktemp -d)
  cp Packaging/Agent/Info.plist "$agent_plist_dir/Info.plist"
  target_dir=${CARGO_TARGET_DIR:-$repo/target}
  (cd "$repo" && cargo rustc --locked --release -p fogar-agentd --bin fogar-agentd -- \
    -C "link-arg=-Wl,-sectcreate,__TEXT,__info_plist,$agent_plist_dir/Info.plist")
  rm -rf "$agent_plist_dir"
  cp "$target_dir/release/fogar-agentd" "$app/Contents/MacOS/fogar-agentd"
  mkdir -p "$app/Contents/Library/LaunchAgents"
  cp Packaging/Agent/com.fogar.agent.plist "$app/Contents/Library/LaunchAgents/com.fogar.agent.plist"
  plutil -lint "$app/Contents/Library/LaunchAgents/com.fogar.agent.plist" >/dev/null
fi

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

# Nothing in Contents/MacOS but the app's own binary and, in release, the agent: anything else
# there would be signed and run as part of the app.
expected="FogarDesktop"
[ "$config" = release ] && expected="FogarDesktop fogar-agentd"
actual=$(ls -A "$app/Contents/MacOS" | sort | tr '\n' ' ' | sed 's/ $//')
if [ "$actual" != "$(echo "$expected" | tr ' ' '\n' | sort | tr '\n' ' ' | sed 's/ $//')" ]; then
  echo "error: Contents/MacOS has [$actual], expected [$expected]" >&2
  exit 1
fi

identity=${FOGAR_SIGN_IDENTITY:-$(security find-identity -v -p codesigning 2>/dev/null | awk -F'"' '/Apple Development/ {print $2; exit}')}
if [ -z "$identity" ]; then
  identity=-
  echo "warning: no Apple Development identity: signing ad hoc; agent and app will trust each other only by uid" >&2
fi

# Inside out, never --deep: the agent first, then the app seals it.
if [ "$config" = release ]; then
  codesign --force --options runtime --timestamp=none --identifier com.fogar.agent \
    --entitlements Packaging/Agent/fogar-agentd.entitlements --sign "$identity" "$app/Contents/MacOS/fogar-agentd"
fi
codesign --force --options runtime --timestamp=none \
  --entitlements Packaging/FogarDesktop.entitlements --sign "$identity" "$app"
codesign --verify --strict "$app"

# What the app's peer check and TCC need, checked on what was just signed.
fail() { echo "error: $*" >&2; exit 1; }
sig_field() { codesign -dvvv "$1" 2>&1 | sed -n "s/^$2=//p" | head -1; }
has_entitlement() { codesign -d --entitlements - --xml "$1" 2>/dev/null | grep -q "<key>$2</key>"; }
for forbidden in com.apple.security.get-task-allow com.apple.security.cs.disable-library-validation \
  com.apple.security.cs.allow-dyld-environment-variables; do
  ! has_entitlement "$app" "$forbidden" || fail "the app is signed with $forbidden"
done
has_entitlement "$app" com.apple.security.device.audio-input || fail "the app lacks the microphone entitlement"
/usr/libexec/PlistBuddy -c "Print :NSMicrophoneUsageDescription" "$app/Contents/Info.plist" >/dev/null \
  || fail "the app's Info.plist lacks NSMicrophoneUsageDescription"
if [ "$config" = release ]; then
  agent="$app/Contents/MacOS/fogar-agentd"
  [ "$(sig_field "$agent" Identifier)" = com.fogar.agent ] || fail "the agent is not signed as com.fogar.agent"
  [ "$(sig_field "$agent" TeamIdentifier)" = "$(sig_field "$app" TeamIdentifier)" ] \
    || fail "the agent and the app are signed by different Teams"
  codesign -dvvv "$agent" 2>&1 | grep -Eq '^CodeDirectory .*flags=.*runtime' || fail "the agent lacks the hardened runtime"
  for forbidden in com.apple.security.get-task-allow com.apple.security.cs.disable-library-validation \
    com.apple.security.cs.allow-dyld-environment-variables; do
    ! has_entitlement "$agent" "$forbidden" || fail "the agent is signed with $forbidden"
  done
  has_entitlement "$agent" com.apple.security.device.audio-input || fail "the agent lacks the microphone entitlement"
  # The embedded Info.plist made it into the signature (codesign reads it from __info_plist).
  codesign -dvvv "$agent" 2>&1 | grep -Eq '^Info.plist entries=[1-9]' || fail "the agent has no embedded Info.plist"
  launchagent="$app/Contents/Library/LaunchAgents/com.fogar.agent.plist"
  [ "$(/usr/libexec/PlistBuddy -c "Print :BundleProgram" "$launchagent")" = Contents/MacOS/fogar-agentd ] \
    || fail "the LaunchAgent's BundleProgram does not point at the agent"
  [ "$(/usr/libexec/PlistBuddy -c "Print :Label" "$launchagent")" = com.fogar.agent ] || fail "LaunchAgent label"
fi
echo "built $app ($config, signed: $identity)"
