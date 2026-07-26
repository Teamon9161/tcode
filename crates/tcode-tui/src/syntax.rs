//! Shared syntax definitions for source previews and fenced Markdown code.
//!
//! Syntect's built-in set covers common languages. Project-vendored grammars
//! extend it for languages missing from that set, so each TUI surface resolves
//! a file extension or fence token against identical definitions.

use std::sync::OnceLock;

use syntect::parsing::{SyntaxDefinition, SyntaxSet};

const ZIG_SYNTAX: &str = include_str!("../syntaxes/Zig.sublime-syntax");

pub(crate) fn syntaxes() -> &'static SyntaxSet {
    static SYNTAXES: OnceLock<SyntaxSet> = OnceLock::new();
    SYNTAXES.get_or_init(|| {
        let zig = SyntaxDefinition::load_from_str(ZIG_SYNTAX, true, Some("Zig.sublime-syntax"))
            .expect("vendored Zig syntax must parse");
        let mut builder = SyntaxSet::load_defaults_newlines().into_builder();
        builder.add(zig);
        builder.build()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn includes_zig_extension() {
        let syntaxes = syntaxes();
        assert_eq!(
            syntaxes.find_syntax_by_extension("zig").unwrap().name,
            "Zig"
        );
    }
}
