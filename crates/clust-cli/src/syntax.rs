use std::sync::LazyLock;

use ratatui::{
    style::{Color, Style},
    text::Span,
};
use syntect::easy::HighlightLines;
use syntect::highlighting::{
    Color as SynColor, ScopeSelectors, StyleModifier, Theme, ThemeItem, ThemeSettings,
};
use syntect::parsing::{SyntaxReference, SyntaxSet};

static SYNTAX_SET: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);
static THEME: LazyLock<Theme> = LazyLock::new(graphite_theme);

/// Look up a syntax definition by file name (uses extension matching).
/// Returns `None` for unknown file types.
pub fn syntax_for_file(filename: &str) -> Option<&'static SyntaxReference> {
    let ext = filename.rsplit('.').next()?;
    SYNTAX_SET.find_syntax_by_extension(ext)
}

/// Highlight a single line of code, returning styled spans.
///
/// Each span gets the syntax-derived foreground color layered over the provided
/// `bg` (diff background). Tokens with no specific scope coloring use `default_fg`.
pub fn highlight_line(
    line: &str,
    syntax: &SyntaxReference,
    bg: Color,
    default_fg: Color,
) -> Vec<Span<'static>> {
    if line.is_empty() {
        return vec![];
    }

    let mut h = HighlightLines::new(syntax, &THEME);
    let ranges = match h.highlight_line(line, &SYNTAX_SET) {
        Ok(r) => r,
        Err(_) => {
            return vec![Span::styled(
                line.to_string(),
                Style::default().fg(default_fg).bg(bg),
            )]
        }
    };

    ranges
        .into_iter()
        .map(|(style, text)| {
            let fg = if style.foreground.a == 0 {
                default_fg
            } else {
                Color::Rgb(style.foreground.r, style.foreground.g, style.foreground.b)
            };
            Span::styled(text.to_string(), Style::default().fg(fg).bg(bg))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Graphite syntax theme — maps TextMate scopes to the Graphite palette
// ---------------------------------------------------------------------------

fn graphite_theme() -> Theme {
    let c = |r, g, b| SynColor { r, g, b, a: 255 };

    Theme {
        name: Some("Graphite".into()),
        author: None,
        settings: ThemeSettings {
            foreground: Some(c(220, 221, 224)), // textPrimary
            background: Some(c(27, 29, 32)),    // bgBase
            ..Default::default()
        },
        scopes: vec![
            // Comments — dimmed
            rule("comment", c(108, 110, 116)), // textTertiary
            // Keywords — accent blue
            rule("keyword", c(94, 154, 191)),           // accent
            rule("keyword.operator", c(160, 162, 168)), // textSecondary
            // Storage — accent blue (let, const, fn, struct, impl, …)
            rule("storage.type", c(94, 154, 191)),     // accent
            rule("storage.modifier", c(94, 154, 191)), // accent
            // Strings — green
            rule("string", c(91, 184, 114)), // success
            // Numeric literals — orange
            rule("constant.numeric", c(240, 160, 48)), // repo orange
            // Language constants (true, false, nil, …) — coral
            rule("constant.language", c(240, 128, 112)), // repo coral
            // Other constants — coral
            rule("constant.other", c(240, 128, 112)), // repo coral
            // Function names — bright accent
            rule("entity.name.function", c(114, 174, 208)), // accentBright
            // Type / class names — yellow
            rule("entity.name.type", c(224, 208, 64)), // repo yellow
            rule("entity.name.class", c(224, 208, 64)), // repo yellow
            rule("entity.other.inherited-class", c(224, 208, 64)),
            // HTML/XML tags — accent
            rule("entity.name.tag", c(94, 154, 191)), // accent
            // Attributes — yellow
            rule("entity.other.attribute-name", c(224, 208, 64)),
            // Library / built-in functions — bright accent
            rule("support.function", c(114, 174, 208)), // accentBright
            // Library / built-in types — teal
            rule("support.type", c(64, 208, 208)),  // repo teal
            rule("support.class", c(64, 208, 208)), // repo teal
            // Punctuation — subtle
            rule("punctuation", c(160, 162, 168)), // textSecondary
            // Variables — default text
            rule("variable", c(220, 221, 224)), // textPrimary
            rule("variable.parameter", c(220, 221, 224)), // textPrimary
            // Decorators / annotations — purple
            rule("meta.decorator", c(160, 112, 240)), // repo purple
            rule("entity.name.function.decorator", c(160, 112, 240)),
            // Markup headings — bold accent bright
            rule("markup.heading", c(114, 174, 208)), // accentBright
            // Markup bold/italic — primary text (modifiers applied by syntect)
            rule("markup.bold", c(220, 221, 224)),
            rule("markup.italic", c(220, 221, 224)),
            // Diff meta (if syntect parses diff syntax itself)
            rule("markup.inserted", c(91, 184, 114)), // success
            rule("markup.deleted", c(204, 80, 72)),   // error
        ],
    }
}

fn rule(scope: &str, fg: SynColor) -> ThemeItem {
    ThemeItem {
        scope: scope.parse::<ScopeSelectors>().unwrap(),
        style: StyleModifier {
            foreground: Some(fg),
            background: None,
            font_style: None,
        },
    }
}
