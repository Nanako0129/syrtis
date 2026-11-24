#!/bin/bash
# Assemble Syrtis.app from the SwiftPM release build.
#
#   scripts/bundle.sh [marketing-version] [build-number]
#
# Run from the repo root. Since v1.0.0 there is a single app identity
# (Syrtis.app / com.nyanako.tokenbar) — prereleases ship through the same
# bundle on the Sparkle "beta" channel instead of a side-by-side app. The
# retired beta identity (com.nyanako.tokenbar.beta / "TokenBar Beta.app")
# is migrated from on first launch.
set -euo pipefail

VERSION="${1:-1.0.0}"
BUILD_NUMBER="${2:-100}"
BUNDLE_ID="${BUNDLE_ID:-com.nyanako.tokenbar}"
APP_NAME="${APP_DISPLAY:-Syrtis}"
# Overridable so a build can sit somewhere other than beside the release
# artifact without changing its identity. `make selftest-bundled` uses it to
# assemble a real `Syrtis.app` — same identifier, same CFBundleName — in a
# subdirectory, rather than renaming the app to avoid the collision.
OUT_DIR="${OUT_DIR:-dist}"
APP="$OUT_DIR/$APP_NAME.app"

echo "==> building release binaries"
cargo build --release
swift build -c release

echo "==> assembling $APP ($VERSION, build $BUILD_NUMBER, $BUNDLE_ID)"
rm -rf "$APP"
mkdir -p "$OUT_DIR"
# Keep Spotlight from indexing local builds — otherwise every dist/ app
# shows up beside the installed one in search results.
touch "$OUT_DIR/.metadata_never_index"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources" "$APP/Contents/Frameworks"

cp .build/release/Syrtis "$APP/Contents/MacOS/Syrtis"
# SwiftPM resource bundle (animation frames, agent icons).
cp -R .build/release/Syrtis_Syrtis.bundle "$APP/Contents/Resources/"
# Localizations land in the *main* bundle for packaged runs. Bare `swift run`
# stages the same .lproj directories from the SwiftPM resource bundle before
# SwiftUI creates any views.
cp -R Sources/Syrtis/Resources/Localizations/*.lproj "$APP/Contents/Resources/"
# The Linux agent for SSH machines, when `make remote-agent` staged one (its
# checksum file is written last, so only a complete staging is copied). Before
# signing: the seal covers it.
REMOTE_AGENT_STAGE=".build/remote-agent/x86_64-unknown-linux-gnu"
if [ -f "$REMOTE_AGENT_STAGE/syrtis-agent" ] && [ -f "$REMOTE_AGENT_STAGE/syrtis-agent.sha256" ]; then
  echo "==> bundling the remote agent ($(cut -c1-12 "$REMOTE_AGENT_STAGE/syrtis-agent.sha256"))"
  mkdir -p "$APP/Contents/Resources/remote-agent/x86_64-unknown-linux-gnu"
  cp "$REMOTE_AGENT_STAGE/syrtis-agent" "$REMOTE_AGENT_STAGE/syrtis-agent.sha256" \
    "$APP/Contents/Resources/remote-agent/x86_64-unknown-linux-gnu/"
fi
# Brand icon, shared with the Tauri app.
if [ -f assets/icon.icns ]; then
  cp assets/icon.icns "$APP/Contents/Resources/icon.icns"
fi
# Sparkle framework, compiled here rather than taken from the SPM binary
# artifact — see scripts/build-sparkle.sh for why the official prebuilt one
# cannot rename the installed app. Not conditional: an app assembled without an
# updater looks fine and silently never updates again.
SPARKLE_FRAMEWORK="$(scripts/build-sparkle.sh)"
cp -R "$SPARKLE_FRAMEWORK" "$APP/Contents/Frameworks/"

# Assert on the binary that actually ships, not on the source that produced it.
# With the macro off the call is compiled out, so the stock artifact scores zero
# here — this check has been run against both and does discriminate. Without it,
# a build that quietly fell back to the official framework would reach users as
# an app whose next major version can never rename itself, and there is no
# second channel to correct that (the cask is auto_updates true).
#
# `grep -c`, not `grep -q`: under `set -o pipefail` a quiet grep closes the pipe
# on its first match, `strings` dies of SIGPIPE, and the pipeline reports
# failure exactly when the check passes. Counting reads all the input, and the
# count is the thing being asserted anyway.
NORMALIZE_REFS="$(strings -a "$APP/Contents/Frameworks/Sparkle.framework/Versions/Current/Autoupdate" \
                  | grep -c SUNormalizedInstallationPath || true)"
if [ "$NORMALIZE_REFS" -eq 0 ]; then
  echo "error: bundled Sparkle lacks installed-name normalization." >&2
  echo "  Built from: $SPARKLE_FRAMEWORK" >&2
  echo "  Remove .build/sparkle-normalized and re-run to rebuild it." >&2
  exit 1
fi

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key>
    <string>Syrtis</string>
    <key>CFBundleIdentifier</key>
    <string>$BUNDLE_ID</string>
    <key>CFBundleName</key>
    <string>$APP_NAME</string>
    <key>CFBundleDisplayName</key>
    <string>$APP_NAME</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>$VERSION</string>
    <key>CFBundleVersion</key>
    <string>$BUILD_NUMBER</string>
    <key>CFBundleIconFile</key>
    <string>icon</string>
    <key>CFBundleDevelopmentRegion</key>
    <string>en</string>
    <key>CFBundleLocalizations</key>
    <array>
        <string>en</string>
        <string>zh-Hans</string>
        <string>zh-Hant</string>
    </array>
    <key>LSMinimumSystemVersion</key>
    <string>14.0</string>
    <key>LSUIElement</key>
    <true/>
    <!-- Provider quota HTTPS can cross a VPN interface macOS treats as a
         local network. Conditional on purpose: not every VPN prompts, and
         the sentence does not claim the traffic stays on this machine. -->
    <key>NSLocalNetworkUsageDescription</key>
    <string>Syrtis connects to your providers to read quota information. When your VPN routes this traffic through a local network, macOS may ask for local network access.</string>
    <key>NSHumanReadableCopyright</key>
    <string>MIT License</string>
    <key>SUEnableInstallerLauncherService</key>
    <false/>
    <key>SUPublicEDKey</key>
    <string>EzyeEi0NEwYK/pYigOPVClXmbmnHXXBEHM7r2uy8GYs=</string>
    <!-- raw main-branch URL: GitHub's releases/latest/download excludes
         prerelease-flagged releases, which would break the beta channel.
         The old TokenBar-Native path keeps redirecting here for existing
         beta installs (the name is never reclaimed). -->
    <key>SUFeedURL</key>
    <string>https://raw.githubusercontent.com/Nanako0129/syrtis/main/appcast.xml</string>
</dict>
</plist>
PLIST

# Sparkle's XPC services exist for sandboxed apps. Syrtis is not sandboxed and
# leaves SUEnableInstallerLauncherService off, so they never run; Sparkle's
# docs allow removing them, which also spares signing Downloader.xpc with its
# own entitlements.
rm -rf "$APP/Contents/Frameworks/Sparkle.framework/Versions/B/XPCServices" \
       "$APP/Contents/Frameworks/Sparkle.framework/XPCServices"

# SIGN_IDENTITY set (e.g. "Developer ID Application: … (TEAMID)"): sign for
# notarization (see codesign_app.sh). Unset: ad-hoc, which is what local builds
# and the release workflow's `build` job use; the release signs in `sign`.
if [ -n "${SIGN_IDENTITY:-}" ]; then
  echo "==> codesign: $SIGN_IDENTITY"
  "$(dirname "$0")/codesign_app.sh" "$APP" "$SIGN_IDENTITY"
else
  echo "==> ad-hoc codesign"
  codesign --force --deep --sign - "$APP"
fi

echo "==> done: $APP"
