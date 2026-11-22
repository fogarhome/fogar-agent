#!/bin/sh
# CI only (job `macos`): builds scripts/ci/peer-agent.c and signs it six ways with a temporary
# self-signed code-signing certificate (Team "FOGARTEST1" in its OU), so the app's peer check
# (Sources/FogarWire/PeerCheck.swift) is tested against real signed processes:
#   ok          identifier com.fogar.agent, hardened runtime             -> accepted
#   otro        identifier com.fogar.otro, hardened runtime              -> refused (identifier)
#   sinruntime  identifier com.fogar.agent, no hardened runtime          -> refused (runtime flag)
#   depurable   identifier com.fogar.agent, runtime + get-task-allow     -> refused (entitlement)
#   dyld        identifier com.fogar.agent, runtime + allow-dyld-environment-variables -> refused
#   sinlv       identifier com.fogar.agent, runtime + disable-library-validation       -> refused
# A self-signed certificate is not Apple's: the tests swap `anchor apple generic` for the
# certificate's own hash and also check that the real anchor refuses it. Writes the paths and the
# hash as FOGAR_CI_* variables to $GITHUB_ENV (or prints them). Never run it on a personal Mac:
# it creates a keychain and adds it to the search list.
set -eu
out=${1:?usage: ci-signed-agents.sh <output dir>}
here=$(cd "$(dirname "$0")" && pwd)
mkdir -p "$out"
out=$(cd "$out" && pwd)
team=FOGARTEST1
pass=fogar-ci-temporal

cat > "$out/cert.cnf" <<CNF
[req]
distinguished_name = dn
prompt = no
x509_extensions = ext
[dn]
CN = Fogar CI test signer
OU = $team
O = Fogar CI
[ext]
basicConstraints = critical,CA:false
keyUsage = critical,digitalSignature
extendedKeyUsage = critical,codeSigning
CNF
# /usr/bin/openssl is LibreSSL: its PKCS#12 is the format `security import` reads.
/usr/bin/openssl req -x509 -newkey rsa:2048 -nodes -days 2 -config "$out/cert.cnf" \
  -keyout "$out/key.pem" -out "$out/cert.pem" 2>/dev/null
/usr/bin/openssl pkcs12 -export -inkey "$out/key.pem" -in "$out/cert.pem" -out "$out/cert.p12" -passout "pass:$pass"
sha1=$(/usr/bin/openssl x509 -in "$out/cert.pem" -noout -fingerprint -sha1 | sed 's/.*=//; s/://g')

kc="$out/fogar-ci.keychain-db"
security create-keychain -p "$pass" "$kc"
security set-keychain-settings -lut 3600 "$kc"
security unlock-keychain -p "$pass" "$kc"
# shellcheck disable=SC2046 # the existing search list, one keychain per word
security list-keychains -d user -s "$kc" $(security list-keychains -d user | tr -d '"')
security import "$out/cert.p12" -k "$kc" -P "$pass" -T /usr/bin/codesign >/dev/null
security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "$pass" "$kc" >/dev/null

clang -O2 -Wall -Werror -o "$out/peer-agent" "$here/ci/peer-agent.c"
entitlement() { # file key
  cat > "$out/$1" <<ENT
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict><key>$2</key><true/></dict></plist>
ENT
}
entitlement debug.entitlements com.apple.security.get-task-allow
entitlement dyld.entitlements com.apple.security.cs.allow-dyld-environment-variables
entitlement sinlv.entitlements com.apple.security.cs.disable-library-validation

sign() { # name identifier [codesign options...]
  name=$1 id=$2
  shift 2
  cp "$out/peer-agent" "$out/agent-$name"
  codesign --force --timestamp=none --keychain "$kc" --sign "$sha1" --identifier "$id" "$@" "$out/agent-$name"
}
if ! sign ok com.fogar.agent --options runtime 2>"$out/sign.err"; then
  # Some macOS versions refuse an untrusted signing certificate: trust it for code signing on this
  # throwaway runner only, then retry.
  cat "$out/sign.err" >&2
  sudo security add-trusted-cert -d -r trustRoot -p codeSign -k /Library/Keychains/System.keychain "$out/cert.pem"
  sign ok com.fogar.agent --options runtime
fi
sign otro com.fogar.otro --options runtime
sign sinruntime com.fogar.agent
sign depurable com.fogar.agent --options runtime --entitlements "$out/debug.entitlements"
sign dyld com.fogar.agent --options runtime --entitlements "$out/dyld.entitlements"
sign sinlv com.fogar.agent --options runtime --entitlements "$out/sinlv.entitlements"
for a in ok otro sinruntime depurable dyld sinlv; do codesign -dvv "$out/agent-$a" 2>&1 | grep -E '^(Identifier|CodeDirectory)'; done

vars="FOGAR_CI_SIGNED_AGENTS=$out
FOGAR_CI_TEST_TEAM=$team
FOGAR_CI_TEST_CERT_SHA1=$sha1"
if [ -n "${GITHUB_ENV:-}" ]; then echo "$vars" >> "$GITHUB_ENV"; else echo "$vars"; fi
