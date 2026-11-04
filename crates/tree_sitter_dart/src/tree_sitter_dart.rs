//! The Dart grammar from github.com/UserNobody14/tree-sitter-dart at commit
//! 6922ad29141e819773d6ffb0c6d2c27adefb5acc, the revision the Dart queries in Sands were written
//! for. Upstream's Rust bindings target tree-sitter 0.22, so the generated parser is vendored and
//! exposed through `tree-sitter-language` like the other grammars.

use tree_sitter_language::LanguageFn;

unsafe extern "C" {
    fn tree_sitter_dart() -> *const ();
}

/// The tree-sitter [`LanguageFn`] for Dart.
pub const LANGUAGE: LanguageFn = unsafe { LanguageFn::from_raw(tree_sitter_dart) };
