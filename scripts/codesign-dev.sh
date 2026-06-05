#!/usr/bin/env bash
# Sign the local macOS dev build with a STABLE self-signed code-signing
# identity so the Keychain ACL ("Always Allow" for the LLM key) and the
# TCC grants (Input Monitoring / Accessibility) persist across rebuilds.
#
# Without a stable signature, every `cargo build` changes the binary's
# code identity (cdhash), so macOS re-prompts for the Keychain and TCC on
# each run — which also blocks daemon startup on the Keychain dialog.
#
# Idempotent: creates the identity once (in a dedicated keychain, so we
# never touch the login-keychain password), then re-signs the binary.
#
# Usage: scripts/codesign-dev.sh [path-to-binary]   (default: target/debug/hyprcorrect)
set -euo pipefail

CERT_NAME="hyprcorrect-dev"
KC_NAME="hyprcorrect-dev.keychain-db"
KC_PASS="hcdev"
BIN="${1:-target/debug/hyprcorrect}"

cd "$(git rev-parse --show-toplevel 2>/dev/null || dirname "$(dirname "$0")")"

if [ ! -f "$BIN" ]; then
  echo "codesign-dev: binary not found: $BIN" >&2
  exit 1
fi

# --- 1. Ensure the dedicated keychain exists, is unlocked, and is on the
#        codesign search list -------------------------------------------------
if ! security list-keychains -d user | grep -q "$KC_NAME"; then
  security create-keychain -p "$KC_PASS" "$KC_NAME" 2>/dev/null || true
fi
security set-keychain-settings "$KC_NAME"            # no auto-lock
security unlock-keychain -p "$KC_PASS" "$KC_NAME"
# Append our keychain to the user search list (without dropping the others).
EXISTING=$(security list-keychains -d user | sed -e 's/^[[:space:]]*//' -e 's/"//g')
if ! printf '%s\n' "$EXISTING" | grep -q "$KC_NAME"; then
  # shellcheck disable=SC2086
  security list-keychains -d user -s $EXISTING "$HOME/Library/Keychains/$KC_NAME"
fi

# --- 2. Create the self-signed code-signing identity if missing -------------
# NOTE: use `find-certificate` (not `find-identity -v`) — a self-signed cert
# is untrusted, so `-v` (valid only) never lists it and we'd recreate it
# every run, ending up with "ambiguous" duplicates.
if ! security find-certificate -c "$CERT_NAME" "$KC_NAME" >/dev/null 2>&1; then
  echo "codesign-dev: creating self-signed identity '$CERT_NAME'…"
  TMP="$(mktemp -d)"
  cat > "$TMP/cs.cnf" <<'EOF'
[req]
distinguished_name = dn
x509_extensions = v3
prompt = no
[dn]
CN = hyprcorrect-dev
[v3]
basicConstraints = critical,CA:false
keyUsage = critical,digitalSignature
extendedKeyUsage = critical,codeSigning
EOF
  openssl req -x509 -newkey rsa:2048 -nodes \
    -keyout "$TMP/key.pem" -out "$TMP/cert.pem" \
    -days 3650 -config "$TMP/cs.cnf" >/dev/null 2>&1
  openssl pkcs12 -export -legacy \
    -inkey "$TMP/key.pem" -in "$TMP/cert.pem" \
    -name "$CERT_NAME" -out "$TMP/id.p12" -passout pass:"$KC_PASS" >/dev/null 2>&1 \
  || openssl pkcs12 -export \
    -inkey "$TMP/key.pem" -in "$TMP/cert.pem" \
    -name "$CERT_NAME" -out "$TMP/id.p12" -passout pass:"$KC_PASS" >/dev/null 2>&1
  security import "$TMP/id.p12" -k "$KC_NAME" -P "$KC_PASS" \
    -T /usr/bin/codesign -f pkcs12 >/dev/null
  # Let codesign use the private key without an interactive prompt.
  security set-key-partition-list -S apple-tool:,apple:,codesign: \
    -s -k "$KC_PASS" "$KC_NAME" >/dev/null 2>&1 || true
  rm -rf "$TMP"
fi

# --- 3. Sign the binary -----------------------------------------------------
codesign --force --sign "$CERT_NAME" --keychain "$KC_NAME" --timestamp=none "$BIN"
echo "codesign-dev: signed $BIN with '$CERT_NAME'"
codesign -dvv "$BIN" 2>&1 | grep -E "Authority|Identifier|TeamIdentifier|Signature" || true
