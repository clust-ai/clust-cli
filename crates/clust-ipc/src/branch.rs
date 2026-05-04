/// Sanitize arbitrary user input into a valid git branch name that is also
/// safe for use as a filesystem directory name.
///
/// Transformations applied (in order):
/// - Trim whitespace
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
/// - Empty result → `"branch"`
pub fn sanitize_branch_name(input: &str) -> String {
    let mut s = String::with_capacity(input.len());

    // Step 1: trim
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return "branch".to_string();
    }

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
        return "branch".to_string();
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
        // `{` is already replaced by char-level step, so `@{` won't appear,
        // but verify the output is clean
        assert_eq!(sanitize_branch_name("test@{0}"), "test@-0");
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
}
