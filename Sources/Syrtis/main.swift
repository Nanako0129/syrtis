import AppKit

// Entry point. `--smoke` keeps the Phase 1 CLI bridge check available for CI,
// `--selftest` runs the TokenBarCore logic checks; anything else boots the
// menu-bar app (no storyboard, no .app bundle yet).

AppLanguage.prepareDirectRunResources()

if CommandLine.arguments.contains("--smoke") {
    exit(Smoke.run())
}
// Measurement lane, not a prototype: prints the current quota window's
// attributed totals and the scan timings the feature's performance comments
// quote. See `WindowProbe`.
if CommandLine.arguments.contains("--window-probe") {
    WindowProbe.run()
}
// Same kind of lane: times the synchronous quota refresh a window switch
// runs on the main actor. See `RefreshTimingProbe`.
if CommandLine.arguments.contains("--refresh-timing") {
    RefreshTimingProbe.start()
}
// Same kind of lane: when each part of the dashboard becomes drawable after
// a cold start, with the popover's tasks contending. See `LaunchTimelineProbe`.
if CommandLine.arguments.contains("--launch-timeline") {
    LaunchTimelineProbe.start()
}
if CommandLine.arguments.contains("--selftest") {
    // Some assertions compare against English UI copy, so on a non-English Mac
    // they would fail for the wrong reason. Say so instead of looking broken.
    if Bundle.main.preferredLocalizations.first != "en" {
        FileHandle.standardError.write(
            Data("warning: re-run with -AppleLanguages \"(en)\" (or `make selftest`) — string assertions expect English\n".utf8))
    }
    SelfTest.run()
}

if DemoData.ignoresLocalVisibility {
    DemoData.ignoreLocalVisibility()
}

// First launch after the Syrtis rename: move TokenBar.app to Syrtis.app and
// relaunch from there (exits on success). Before NSApplication, so nothing has
// loaded a resource through the old path yet.
BundleRename.runIfNeeded()

let app = NSApplication.shared
let delegate = AppDelegate()
app.delegate = delegate
app.run()
