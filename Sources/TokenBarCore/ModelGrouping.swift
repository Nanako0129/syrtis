/// Display-grouping identity for model ids that arrive raw.
///
/// The engine's model, monthly and hourly reports already group through
/// `normalize_model_for_grouping`, but the graph payload keys each row by the
/// raw `canonical_model_id`, so Swift surfaces that group graph rows by model
/// have to apply the same fold themselves or they disagree with the Models
/// view. This mirrors the engine's built-in rules (`builtin_grouping` in
/// `vendor/tokscale-core/src/model_alias.rs`): Grok Build keys turn usage by
/// `grok-<version>-build` while the session names `grok-<version>` (#118), and
/// an exact list of Gemini label variants folds onto the bare model
/// (`gemini-4-argon-{c,trial}`, `gemini-3.5-flash-{high,medium,low,extra-low}`).
///
/// Presentation only. Anything that matches a model id — quota scopes and
/// attribution records — keeps the raw id. `ModelColorMap` groups on both
/// sides, construction and lookup, so a raw entry and a raw lookup still meet
/// and the two ids share one shade. `Tests/fixtures/model-grouping-cases.json` is checked against both
/// this function (SelfTest) and the engine (tb_core_ffi test). That binds the
/// two on those inputs only: an engine rule that widens beyond the table, or
/// a user alias map once the app installs one, would not be caught by it.
public enum ModelGrouping {
    /// Gemini labels the engine folds onto their bare model, listed exactly as
    /// in `builtin_grouping` (maintainer decision 2026-11-08). An unlisted
    /// suffix may be a different model and is left alone.
    static let geminiFolds: [String: String] = [
        "gemini-4-argon-c": "gemini-4-argon",
        "gemini-4-argon-trial": "gemini-4-argon",
        "gemini-3.5-flash-high": "gemini-3.5-flash",
        "gemini-3.5-flash-medium": "gemini-3.5-flash",
        "gemini-3.5-flash-low": "gemini-3.5-flash",
        "gemini-3.5-flash-extra-low": "gemini-3.5-flash",
    ]

    /// A listed Gemini label → its bare model; `grok-<version>-build` →
    /// `grok-<version>`, where the version is one or more dot-separated runs of
    /// ASCII digits. Every other id is returned unchanged. Expects the lowercase `canonical_model_id` spelling the
    /// graph payload carries.
    public static func groupID(_ canonicalID: String) -> String {
        if let bare = geminiFolds[canonicalID] { return bare }
        // Longer than "grok--build" so the prefix and suffix cannot overlap
        // ("grok-build") and the version between them is non-empty.
        guard canonicalID.count > "grok--build".count,
              canonicalID.hasPrefix("grok-"), canonicalID.hasSuffix("-build")
        else { return canonicalID }
        let version = canonicalID.dropFirst("grok-".count).dropLast("-build".count)
        let isVersion = version.split(separator: ".", omittingEmptySubsequences: false)
            .allSatisfy { part in !part.isEmpty && part.allSatisfy { $0.isASCII && $0.isNumber } }
        return isVersion ? "grok-\(version)" : canonicalID
    }
}
