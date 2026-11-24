#!/bin/bash
# Stage the Linux agent that Syrtis uploads to SSH machines.
#
#   scripts/stage_remote_agent.sh                  # download the release pinned in scripts/remote-agent.lock
#   REMOTE_AGENT_BIN=<file> scripts/...            # development: use this file as is
#
# The trust root is the committed lock (repo, tag, target, sha256): the
# downloaded binary must hash to the lock's sha256, or the run fails and
# stages nothing. The release's own .sha256 asset is only an extra
# cross-check, never the pin, because whoever can replace the binary in a
# release can replace that file too.
#
# Using another release (for example a fork): either edit the lock, or set
# REMOTE_AGENT_REPO, REMOTE_AGENT_TAG and REMOTE_AGENT_SHA256 together (the
# sha256 of that release's asset; all three or none, since the lock's sha256
# belongs to the lock's release).
# REMOTE_AGENT_OUT changes the output directory.
#
# Writes <OUT>/syrtis-agent and <OUT>/syrtis-agent.sha256 (the checksum of that
# file, written last, so a half-staged directory has no pin and the app treats
# it as "no agent"). scripts/bundle.sh copies <OUT> into the app's
# Contents/Resources/remote-agent/<triple>/; the app reads the checksum from
# there. With REMOTE_AGENT_BIN the checksum is computed from that file.
set -euo pipefail

TRIPLE="x86_64-unknown-linux-gnu"
LOCK="$(cd "$(dirname "$0")" && pwd)/remote-agent.lock"
OUT="${REMOTE_AGENT_OUT:-.build/remote-agent/$TRIPLE}"
ASSET="syrtis-agent-$TRIPLE"

# Anything left from an earlier run is gone first: a failure below must not
# leave an old binary that a later bundle would pick up.
rm -rf "$OUT"
mkdir -p "$OUT"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/syrtis-remote-agent.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT
fail() { echo "remote-agent: $*" >&2; rm -rf "$OUT" "$WORK"; exit 1; }

lock_value() {
  [ -f "$LOCK" ] || return 0
  awk -F= -v k="$1" '$1 == k { sub(/^[^=]*=/, ""); gsub(/^[ \t]+|[ \t\r]+$/, ""); print; exit }' "$LOCK"
}

if [ -n "${REMOTE_AGENT_BIN:-}" ]; then
  [ -f "$REMOTE_AGENT_BIN" ] || fail "REMOTE_AGENT_BIN is not a file: $REMOTE_AGENT_BIN"
  echo "==> remote agent: local override $REMOTE_AGENT_BIN"
  cp "$REMOTE_AGENT_BIN" "$WORK/$ASSET"
else
  LOCK_TARGET="$(lock_value target)"
  REPO="${REMOTE_AGENT_REPO:-$(lock_value repo)}"
  TAG="${REMOTE_AGENT_TAG:-$(lock_value tag)}"
  if [ -n "${REMOTE_AGENT_REPO:-}${REMOTE_AGENT_TAG:-}${REMOTE_AGENT_SHA256:-}" ]; then
    [ -n "${REMOTE_AGENT_REPO:-}" ] && [ -n "${REMOTE_AGENT_TAG:-}" ] && [ -n "${REMOTE_AGENT_SHA256:-}" ] \
      || fail "REMOTE_AGENT_REPO, REMOTE_AGENT_TAG and REMOTE_AGENT_SHA256 (the sha256 of $ASSET in that release) must be set together"
    PIN="$REMOTE_AGENT_SHA256"
  else
    PIN="$(lock_value sha256)"
    [ -z "$LOCK_TARGET" ] || [ "$LOCK_TARGET" = "$TRIPLE" ] || fail "$LOCK has target '$LOCK_TARGET', expected $TRIPLE"
  fi
  if [ -z "$TAG" ] || [ -z "$PIN" ]; then
    fail "no pinned Linux agent release: $LOCK has no tag and sha256 yet (the upstream agent has not published one). Set REMOTE_AGENT_REPO, REMOTE_AGENT_TAG and REMOTE_AGENT_SHA256 for another release, or REMOTE_AGENT_BIN=<file> to use a local build"
  fi
  [[ "$REPO" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]] || fail "invalid repo: $REPO"
  [[ "$TAG" =~ ^[A-Za-z0-9][A-Za-z0-9_.+-]*$ ]] || fail "invalid tag: $TAG"
  [[ "$PIN" =~ ^[0-9a-f]{64}$ ]] || fail "the pinned sha256 must be 64 lowercase hex characters"
  BASE="https://github.com/$REPO/releases/download/$TAG"
  echo "==> remote agent: $REPO $TAG"
  curl --proto '=https' --proto-redir '=https' --tlsv1.2 -fL --retry 2 -sS \
    -o "$WORK/$ASSET" "$BASE/$ASSET" || fail "download failed: $BASE/$ASSET"
  ACTUAL="$(shasum -a 256 "$WORK/$ASSET" | awk '{ print $1 }')"
  [ "$ACTUAL" = "$PIN" ] || fail "checksum mismatch: pinned $PIN, got $ACTUAL"
  # Extra cross-check only; the pin above is what is trusted.
  if curl --proto '=https' --proto-redir '=https' --tlsv1.2 -fL -sS \
       -o "$WORK/$ASSET.sha256" "$BASE/$ASSET.sha256" 2>/dev/null; then
    PUBLISHED="$(awk 'NR == 1 { print $1 }' "$WORK/$ASSET.sha256")"
    [ "$PUBLISHED" = "$PIN" ] || fail "the release's .sha256 ($PUBLISHED) disagrees with the pin ($PIN)"
  fi
fi

SUM="$(shasum -a 256 "$WORK/$ASSET" | awk '{ print $1 }')"
install -m 0755 "$WORK/$ASSET" "$OUT/syrtis-agent"
printf '%s  syrtis-agent\n' "$SUM" > "$OUT/syrtis-agent.sha256"
echo "==> staged $OUT/syrtis-agent (sha256 $SUM)"
