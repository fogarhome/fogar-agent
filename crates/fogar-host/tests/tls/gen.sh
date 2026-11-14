#!/usr/bin/env bash
# Regenerates the TEST-ONLY PKI used by tests/tls.rs (never trusted outside these tests).
# Run from this directory: ./gen.sh   (OpenSSL/LibreSSL with EC support)
set -euo pipefail
days=36500
openssl ecparam -name prime256v1 -genkey -noout -out ca.key.tmp
openssl pkcs8 -topk8 -nocrypt -in ca.key.tmp -out ca.key && rm ca.key.tmp
openssl req -x509 -new -key ca.key -sha256 -days $days -subj "/CN=fogar test CA" \
  -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign,cRLSign" -out ca.pem
openssl ecparam -name prime256v1 -genkey -noout -out other-ca.key.tmp
openssl req -x509 -new -key other-ca.key.tmp -sha256 -days $days -subj "/CN=unrelated CA" \
  -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign,cRLSign" -out other-ca.pem
rm other-ca.key.tmp
openssl ecparam -name prime256v1 -genkey -noout -out leaf.key.tmp
openssl pkcs8 -topk8 -nocrypt -in leaf.key.tmp -out leaf.key && rm leaf.key.tmp
openssl req -new -key leaf.key -subj "/CN=localhost" -out leaf.csr
printf 'basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=serverAuth\nsubjectAltName=DNS:localhost,IP:127.0.0.1\n' > leaf.ext
openssl x509 -req -in leaf.csr -CA ca.pem -CAkey ca.key -CAcreateserial -sha256 -days $days -extfile leaf.ext -out leaf.pem
rm -f leaf.csr leaf.ext ca.srl ca.key
