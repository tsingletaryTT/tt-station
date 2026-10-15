#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.
#
# sign-release.sh: sign and verify TT Station release packages (Ed25519).
#
#   scripts/sign-release.sh sign FILE...      # writes FILE.sig for each FILE
#   scripts/sign-release.sh verify FILE...    # checks each FILE.sig against
#                                             # deploy/release-keys.pub (or $KEYS)
#   scripts/sign-release.sh pubkey KEY.pem    # the key's line for release-keys.pub
#
# sign reads the PEM private key (`openssl genpkey -algorithm ed25519`) from
# $TT_STATION_RELEASE_SIGNING_KEY (the key itself, as the release workflow's
# secret holds it) or from the file named by $TT_STATION_RELEASE_SIGNING_KEY_FILE.
#
# A FILE.sig is the base64 of an Ed25519 signature over three lines:
#   tt-station-release-v1
#   file=<basename of FILE>
#   sha256=<lowercase hex sha256 of FILE>
# The agent checks exactly this (crates/tt-station-agentd/src/release_keys.rs).
# Needs only coreutils and OpenSSL >= 3.0. Doc: docs/reference/releases.md.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
KEYS="${KEYS:-$ROOT/deploy/release-keys.pub}"

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
chmod 700 "$TMP"

statement() { # FILE -> the signed statement on stdout
    local sha
    sha="$(sha256sum "$1" | cut -d' ' -f1)"
    printf 'tt-station-release-v1\nfile=%s\nsha256=%s\n' "$(basename "$1")" "$sha"
}

pubkey_pem() { # base64 raw key -> PEM public key on stdout
    # DER prefix of an Ed25519 SubjectPublicKeyInfo; the raw 32-byte key follows.
    { printf '\x30\x2a\x30\x05\x06\x03\x2b\x65\x70\x03\x21\x00'; printf '%s' "$1" | base64 -d; } \
        | openssl pkey -pubin -inform DER -outform PEM
}

cmd="${1:-}"
shift || true
case "$cmd" in
sign)
    [ $# -gt 0 ] || { echo "usage: $0 sign FILE..." >&2; exit 2; }
    key="$TMP/key.pem"
    if [ -n "${TT_STATION_RELEASE_SIGNING_KEY:-}" ]; then
        (umask 077; printf '%s\n' "$TT_STATION_RELEASE_SIGNING_KEY" > "$key")
    elif [ -n "${TT_STATION_RELEASE_SIGNING_KEY_FILE:-}" ]; then
        cp "$TT_STATION_RELEASE_SIGNING_KEY_FILE" "$key"
    else
        echo "error: no signing key (set TT_STATION_RELEASE_SIGNING_KEY or TT_STATION_RELEASE_SIGNING_KEY_FILE)" >&2
        exit 1
    fi
    for f in "$@"; do
        statement "$f" > "$TMP/msg"
        openssl pkeyutl -sign -inkey "$key" -rawin -in "$TMP/msg" | base64 -w0 > "$f.sig"
        echo >> "$f.sig"
        echo "signed $(basename "$f") -> $(basename "$f").sig"
    done
    ;;
verify)
    [ $# -gt 0 ] || { echo "usage: $0 verify FILE..." >&2; exit 2; }
    mapfile -t lines < <(grep -vE '^\s*(#|$)' "$KEYS" || true)
    if [ ${#lines[@]} -eq 0 ]; then
        echo "error: $KEYS lists no keys; nothing could verify these packages" >&2
        exit 1
    fi
    fail=0
    for f in "$@"; do
        [ -f "$f.sig" ] || { echo "MISSING  $(basename "$f").sig" >&2; fail=1; continue; }
        statement "$f" > "$TMP/msg"
        base64 -d < "$f.sig" > "$TMP/sig" 2>/dev/null || { echo "BAD      $(basename "$f").sig (not base64)" >&2; fail=1; continue; }
        ok=""
        for line in "${lines[@]}"; do
            read -r name b64 <<< "$line"
            pubkey_pem "$b64" > "$TMP/pub.pem"
            if openssl pkeyutl -verify -pubin -inkey "$TMP/pub.pem" -rawin \
                -in "$TMP/msg" -sigfile "$TMP/sig" >/dev/null 2>&1; then
                ok="$name"
                break
            fi
        done
        if [ -n "$ok" ]; then
            echo "OK       $(basename "$f") (signed by $ok)"
        else
            echo "UNTRUSTED $(basename "$f"): no key in $KEYS made this signature" >&2
            fail=1
        fi
    done
    exit "$fail"
    ;;
pubkey)
    [ $# -eq 1 ] || { echo "usage: $0 pubkey KEY.pem" >&2; exit 2; }
    printf '<name> %s\n' "$(openssl pkey -in "$1" -pubout -outform DER | tail -c 32 | base64 -w0)"
    ;;
*)
    sed -n '5,21p' "$0" | sed 's/^# \{0,1\}//'
    exit 2
    ;;
esac
