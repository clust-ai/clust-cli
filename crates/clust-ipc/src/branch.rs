use unicode_normalization::UnicodeNormalization;

/// Fallback branch name used whenever sanitization produces an invalid or
/// rejected result. Centralised so all rejection paths return the same value.
const FALLBACK: &str = "branch";

/// Sanitize arbitrary user input into a valid git branch name that is also
/// safe for use as a filesystem directory name.
///
/// Transformations applied (in order):
/// - NFC-normalize the input (so visually identical inputs compare equal)
/// - Trim whitespace
/// - Reject inputs that look like ref paths (`refs/heads/…`, `refs/remotes/…`)
/// - Reject inputs containing reflog form `@{`
/// - Spaces → `-`
/// - `/` and `\` → `__`
/// - Git-invalid chars (`~ ^ : ? * [ ] { }`) → `-`
/// - ASCII control characters stripped
/// - `..` collapsed to `.`
/// - `@{` → `@-`
/// - Consecutive `-` collapsed
/// - Consecutive `_` (3+) collapsed to `__`
/// - Leading/trailing `.`, `-`, `_` stripped
/// - Trailing `.lock` stripped
/// - Bare `@` → `"at"`
/// - After all rules, anything starting with `.` or with the literal directory
///   form of `refs__heads__`/`refs__remotes__` is rejected
/// - Empty / rejected result → `"branch"`
pub fn sanitize_branch_name(input: &str) -> String {
    // Step 0: NFC-normalize so e.g. precomposed and decomposed forms collapse.
    let normalized: String = input.nfc().collect();

    // Step 1: trim
    let trimmed = normalized.trim();
    if trimmed.is_empty() {
        return FALLBACK.to_string();
    }

    // Pre-sanitization rejections — these protect against users typing the
    // literal git ref form, which would silently land on disk as a working
    // branch name and confuse refspec resolution.
    if trimmed.starts_with("refs/heads/") || trimmed.starts_with("refs/remotes/") {
        return FALLBACK.to_string();
    }
    // Reflog form `name@{n}` is never a legal branch name.
    if trimmed.contains("@{") {
        return FALLBACK.to_string();
    }

    let mut s = String::with_capacity(trimmed.len());

    // Steps 2-5: character-level replacements
    for ch in trimmed.chars() {
        match ch {
            ' ' => s.push('-'),
            '/' | '\\' => s.push_str("__"),
            '~' | '^' | ':' | '?' | '*' | '[' | ']' | '{' | '}' => s.push('-'),
            c if c.is_ascii_control() => {} // strip
            c => s.push(c),
        }
    }

    // Step 6: collapse `..` → `.`
    while s.contains("..") {
        s = s.replace("..", ".");
    }

    // Step 7: replace `@{` → `@-` (already replaced `{` → `-` above,
    // but handle in case input is pre-processed or rules change)
    while s.contains("@{") {
        s = s.replace("@{", "@-");
    }

    // Step 8: collapse consecutive `-`
    while s.contains("--") {
        s = s.replace("--", "-");
    }

    // Step 9: collapse 3+ consecutive `_` to `__`
    while s.contains("___") {
        s = s.replace("___", "__");
    }

    // Step 10-11: strip leading/trailing `.`, `-`, `_`
    let trimmed = s.trim_matches(&['.', '-', '_'][..]);
    s = trimmed.to_string();

    // Step 12: strip trailing `.lock`
    while s.ends_with(".lock") {
        s.truncate(s.len() - 5);
    }
    // Re-strip trailing junk that `.lock` removal may expose
    let trimmed = s.trim_end_matches(&['.', '-', '_'][..]);
    s = trimmed.to_string();

    // Step 13-14: edge cases
    if s == "@" {
        return "at".to_string();
    }
    if s.is_empty() {
        return FALLBACK.to_string();
    }

    // Post-sanitization rejections.
    //
    // Git refuses ref components that begin with a `.`. Our trim above strips
    // leading `.`s, so a leading `.` here means the rule was bypassed by some
    // composition we did not anticipate — be conservative and reject.
    if s.starts_with('.') {
        return FALLBACK.to_string();
    }
    // The `/` → `__` replacement means a ref-path input (already rejected
    // above) cannot reach this point, but a user typing the directory form
    // literally (`refs__heads__main`) should still be rejected: the on-disk
    // form would shadow the actual ref namespace.
    if s.starts_with("refs__heads__") || s.starts_with("refs__remotes__") {
        return FALLBACK.to_string();
    }

    s
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Spaces ──────────────────────────────────────────────────────

    #[test]
    fn spaces_become_hyphens() {
        assert_eq!(
            sanitize_branch_name("my feature branch"),
            "my-feature-branch"
        );
    }

    #[test]
    fn multiple_spaces_collapse() {
        assert_eq!(sanitize_branch_name("hello   world"), "hello-world");
    }

    #[test]
    fn leading_trailing_spaces_trimmed() {
        assert_eq!(sanitize_branch_name("  spaced  "), "spaced");
    }

    // ── Slashes ─────────────────────────────────────────────────────

    #[test]
    fn forward_slash_becomes_double_underscore() {
        assert_eq!(sanitize_branch_name("feature/auth"), "feature__auth");
    }

    #[test]
    fn backslash_becomes_double_underscore() {
        assert_eq!(sanitize_branch_name("path\\to\\branch"), "path__to__branch");
    }

    #[test]
    fn multiple_slashes_collapse() {
        assert_eq!(sanitize_branch_name("a//b"), "a__b");
    }

    #[test]
    fn leading_slash_stripped() {
        assert_eq!(sanitize_branch_name("/leading"), "leading");
    }

    #[test]
    fn trailing_slash_stripped() {
        assert_eq!(sanitize_branch_name("trailing/"), "trailing");
    }

    // ── Special characters ──────────────────────────────────────────

    #[test]
    fn tilde_replaced() {
        assert_eq!(sanitize_branch_name("fix~bug"), "fix-bug");
    }

    #[test]
    fn caret_replaced() {
        assert_eq!(sanitize_branch_name("test^2"), "test-2");
    }

    #[test]
    fn colon_replaced() {
        assert_eq!(sanitize_branch_name("a:b:c"), "a-b-c");
    }

    #[test]
    fn question_mark_replaced() {
        assert_eq!(sanitize_branch_name("what?"), "what");
    }

    #[test]
    fn asterisk_replaced() {
        assert_eq!(sanitize_branch_name("file*name"), "file-name");
    }

    #[test]
    fn brackets_replaced() {
        assert_eq!(sanitize_branch_name("arr[0]"), "arr-0");
    }

    #[test]
    fn braces_replaced() {
        assert_eq!(sanitize_branch_name("obj{key}"), "obj-key");
    }

    // ── Double dots ─────────────────────────────────────────────────

    #[test]
    fn double_dots_collapsed() {
        assert_eq!(sanitize_branch_name("a..b"), "a.b");
    }

    #[test]
    fn triple_dots_collapsed() {
        assert_eq!(sanitize_branch_name("a...b"), "a.b");
    }

    // ── @ handling ──────────────────────────────────────────────────

    #[test]
    fn bare_at_becomes_at_word() {
        assert_eq!(sanitize_branch_name("@"), "at");
    }

    #[test]
    fn embedded_at_preserved() {
        assert_eq!(sanitize_branch_name("user@feature"), "user@feature");
    }

    #[test]
    fn at_brace_replaced() {
        // `@{` is a reflog form — reject outright rather than mangling it.
        assert_eq!(sanitize_branch_name("test@{0}"), "branch");
    }

    // ── .lock suffix ────────────────────────────────────────────────

    #[test]
    fn lock_suffix_stripped() {
        assert_eq!(sanitize_branch_name("branch.lock"), "branch");
    }

    #[test]
    fn double_lock_suffix_stripped() {
        assert_eq!(sanitize_branch_name("branch.lock.lock"), "branch");
    }

    // ── Leading/trailing junk ───────────────────────────────────────

    #[test]
    fn leading_dot_stripped() {
        assert_eq!(sanitize_branch_name(".hidden"), "hidden");
    }

    #[test]
    fn leading_hyphen_stripped() {
        assert_eq!(sanitize_branch_name("-dashed"), "dashed");
    }

    #[test]
    fn trailing_dot_stripped() {
        assert_eq!(sanitize_branch_name("branch."), "branch");
    }

    #[test]
    fn trailing_hyphen_stripped() {
        assert_eq!(sanitize_branch_name("branch-"), "branch");
    }

    // ── Control characters ──────────────────────────────────────────

    #[test]
    fn control_chars_stripped() {
        assert_eq!(sanitize_branch_name("a\x00b\x1fc"), "abc");
    }

    // ── Edge cases ──────────────────────────────────────────────────

    #[test]
    fn empty_input_returns_fallback() {
        assert_eq!(sanitize_branch_name(""), "branch");
    }

    #[test]
    fn whitespace_only_returns_fallback() {
        assert_eq!(sanitize_branch_name("   "), "branch");
    }

    #[test]
    fn all_bad_chars_returns_fallback() {
        assert_eq!(sanitize_branch_name("~^:?*[]"), "branch");
    }

    #[test]
    fn all_dots_returns_fallback() {
        assert_eq!(sanitize_branch_name("..."), "branch");
    }

    // ── Already valid ───────────────────────────────────────────────

    #[test]
    fn valid_name_unchanged() {
        assert_eq!(sanitize_branch_name("my-branch"), "my-branch");
    }

    #[test]
    fn valid_name_with_dots_unchanged() {
        assert_eq!(sanitize_branch_name("v1.2.3"), "v1.2.3");
    }

    // ── Idempotency ─────────────────────────────────────────────────

    #[test]
    fn idempotent() {
        let inputs = [
            "my feature/branch",
            "fix~bug^2",
            "  hello  world  ",
            ".hidden.lock",
            "a..b",
            "@",
            "already-valid",
        ];
        for input in inputs {
            let once = sanitize_branch_name(input);
            let twice = sanitize_branch_name(&once);
            assert_eq!(once, twice, "not idempotent for input: {input:?}");
        }
    }

    // ── Rejection: refs paths ───────────────────────────────────────

    #[test]
    fn rejects_refs_heads_input() {
        assert_eq!(sanitize_branch_name("refs/heads/main"), "branch");
    }

    #[test]
    fn rejects_refs_remotes_input() {
        assert_eq!(sanitize_branch_name("refs/remotes/origin/main"), "branch");
    }

    #[test]
    fn rejects_refs_heads_with_extra_path() {
        assert_eq!(sanitize_branch_name("refs/heads/feature/auth"), "branch");
    }

    #[test]
    fn rejects_refs_heads_underscore_form() {
        // Even if the user types the directory form directly, reject it so
        // the on-disk worktree namespace can't be shadowed.
        assert_eq!(sanitize_branch_name("refs__heads__main"), "branch");
    }

    #[test]
    fn rejects_refs_remotes_underscore_form() {
        assert_eq!(sanitize_branch_name("refs__remotes__origin"), "branch");
    }

    #[test]
    fn allows_branch_named_refs() {
        // A bare component named "refs" without the heads/remotes suffix is
        // legal (uncommon but not malicious).
        assert_eq!(sanitize_branch_name("refs"), "refs");
    }

    // ── Rejection: reflog form ─────────────────────────────────────

    #[test]
    fn rejects_reflog_form() {
        assert_eq!(sanitize_branch_name("HEAD@{0}"), "branch");
    }

    #[test]
    fn rejects_reflog_form_with_text() {
        assert_eq!(sanitize_branch_name("main@{yesterday}"), "branch");
    }

    // ── Rejection: leading dot survives ─────────────────────────────

    #[test]
    fn rejects_only_dots_after_sanitize() {
        // `..foo` would have leading dots stripped to `foo`, which is fine.
        // The leading-dot rejection guards against future regressions.
        assert_eq!(sanitize_branch_name("..foo"), "foo");
    }

    // ── NFC normalization ───────────────────────────────────────────

    #[test]
    fn nfc_normalizes_combining_form() {
        // "é" can be encoded as a single codepoint (U+00E9, NFC) or as
        // "e" + combining acute (U+0065 U+0301, NFD). Both should produce
        // the same sanitized output (NFC form).
        let nfc = "caf\u{00e9}"; // café (precomposed)
        let nfd = "cafe\u{0301}"; // café (decomposed)
        assert_eq!(sanitize_branch_name(nfc), sanitize_branch_name(nfd));
        // And the result should be in NFC (single codepoint).
        let result = sanitize_branch_name(nfd);
        assert!(result.contains('\u{00e9}'));
        assert!(!result.contains('\u{0301}'));
    }
}
