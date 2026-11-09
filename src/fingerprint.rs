//! Content fingerprints of the files a cached command depends on.
//!
//! Two modes, both BLAKE3 hex digests:
//!
//! * [`Mode::Bytes`] hashes the raw file bytes. Always exact, never clever.
//! * [`Mode::Ast`] hashes a canonical serialization of the concrete syntax tree
//!   (tree-sitter). Ordinary comments and whitespace are dropped, every leaf
//!   token keeps its exact text, and comments that carry meaning for tools are
//!   kept.
//!
//! AST mode requires the caller to assert that the command is insensitive to
//! discarded trivia. It is not a proof of semantic equivalence: a program can
//! read its own source or another file's source, including top-level constants.
//! Unknown languages, binary files, oversized files, parse errors, Rust macro
//! calls, certain script directives, and syntax trees deeper than 128 nodes
//! fall back to byte hashes. Some other source-sensitive regions are hashed
//! as written.

use std::path::Path;

use serde::{Deserialize, Serialize};
use tree_sitter::{Language, Node, Parser, Tree};

/// Files larger than this are not parsed (2 MiB).
const MAX_AST_BYTES: usize = 2 * 1024 * 1024;

/// Domain separator for AST digests.
const TAG_AST: &[u8] = b"kioku/fingerprint/v1/ast\0";

/// Domain separator for byte digests.
const TAG_BYTES: &[u8] = b"kioku/fingerprint/v1/bytes\0";

/// Marker written before a node kind.
const TAG_NODE: u8 = 0x01;

/// Marker written when a line terminator separates two tokens in a position
/// where JavaScript semantics depend on it (automatic semicolon insertion).
const FLAG_LINE_TERMINATOR: u8 = 0x02;

/// Fingerprint mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Structural fingerprint of the syntax tree.
    Ast,
    /// Fingerprint of the raw bytes.
    Bytes,
}

/// Comment prefixes that carry meaning for tools and must stay in the
/// fingerprint. Matched against the comment body (markers removed), lowercased.
pub const MEANINGFUL_COMMENT_DIRECTIVES: &[&str] = &[
    "@ts-expect-error",
    "@ts-ignore",
    "@ts-nocheck",
    "@ts-check",
    "eslint-disable",
    "eslint-enable",
    "prettier-ignore",
    "istanbul ignore",
    "c8 ignore",
    "@vitest-environment",
    "@jsx",
];

/// Comment bodies that look like a tool directive we do not know yet. They are
/// kept, because dropping an unknown directive could hide a real change.
const UNKNOWN_DIRECTIVE_PREFIXES: &[&str] = &[
    "@",
    "#",
    "::",
    "eslint",
    "prettier",
    "istanbul",
    "c8",
    "vitest",
    "jest",
    "flow",
    "biome",
    "stylelint",
    "markdownlint",
    "noqa",
    "nolint",
    "clippy",
    "rustfmt",
    "ruff",
    "black",
    "mypy",
    "pyright",
    "ts-",
    "type:",
];

/// Computes the fingerprint of one file.
///
/// `path` selects the language only; it is never hashed, so the digest stays
/// identical on two machines.
#[must_use]
pub fn fingerprint(path: &Path, bytes: &[u8], mode: Mode) -> String {
    match mode {
        Mode::Bytes => hash_bytes(bytes),
        Mode::Ast => ast_fingerprint(path, bytes).unwrap_or_else(|| hash_bytes(bytes)),
    }
}

/// Hashes raw bytes.
fn hash_bytes(bytes: &[u8]) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(TAG_BYTES);
    hasher.update(bytes);
    hasher.finalize().to_hex().to_string()
}

/// Hashes the syntax tree, or returns `None` when the file must fall back to
/// bytes.
fn ast_fingerprint(path: &Path, bytes: &[u8]) -> Option<String> {
    let (language, dialect) = language_for(path)?;
    if bytes.len() > MAX_AST_BYTES || bytes.contains(&0) {
        return None;
    }
    let source = std::str::from_utf8(bytes).ok()?;
    let mut parser = Parser::new();
    parser.set_language(&language).ok()?;
    let tree = parser.parse(source, None)?;
    if exceeds_tree_depth(&tree) || tree.root_node().has_error() {
        return None;
    }
    if is_opaque(tree.root_node(), dialect, source) {
        return None;
    }
    Some(hash_tree(&tree, source, dialect))
}

/// Limits recursive syntax processing on user-controlled input.
fn exceeds_tree_depth(tree: &Tree) -> bool {
    const MAX_DEPTH: usize = 128;
    let mut cursor = tree.walk();
    let mut depth = 0;
    loop {
        if cursor.goto_first_child() {
            depth += 1;
            if depth > MAX_DEPTH {
                return true;
            }
        } else {
            while !cursor.goto_next_sibling() {
                if !cursor.goto_parent() {
                    return false;
                }
                depth -= 1;
            }
        }
    }
}

/// True when the file cannot be fingerprinted by its tree.
///
/// A Rust macro can be a position macro, and it can be reached through an
/// alias, a proc macro or a macro of another file, so a file that calls a
/// macro is fingerprinted byte accurately. A TypeScript directive applies to
/// the line that follows it, so moving it changes what the compiler does with
/// the file; an ambient declaration is parsed by TypeScript in ways the
/// grammar does not reproduce.
fn is_opaque(node: Node, dialect: Dialect, source: &str) -> bool {
    match dialect {
        Dialect::Rust => has_macro_invocation(node),
        Dialect::Script => has_ambient_declaration(node) || has_directive(node, source),
    }
}

/// True when the tree holds a macro call.
fn has_macro_invocation(node: Node) -> bool {
    if node.kind() == "macro_invocation" {
        return true;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if has_macro_invocation(child) {
            return true;
        }
    }
    false
}

/// True when the tree holds a comment that carries a directive.
fn has_directive(node: Node, source: &str) -> bool {
    if is_comment_kind(node.kind()) {
        let text = &source[node.byte_range()];
        if is_meaningful_comment(node.kind(), text) {
            return true;
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if has_directive(child, source) {
            return true;
        }
    }
    false
}

/// True when the tree holds an ambient declaration (`declare ...`).
fn has_ambient_declaration(node: Node) -> bool {
    if node.kind() == "ambient_declaration" {
        return true;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if has_ambient_declaration(child) {
            return true;
        }
    }
    false
}

/// Picks the grammar from the file extension, and the dialect of the language.
fn language_for(path: &Path) -> Option<(Language, Dialect)> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    match extension.as_str() {
        "ts" | "mts" | "cts" => Some((
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Dialect::Script,
        )),
        "tsx" => Some((tree_sitter_typescript::LANGUAGE_TSX.into(), Dialect::Script)),
        "js" | "mjs" | "cjs" | "jsx" => {
            Some((tree_sitter_javascript::LANGUAGE.into(), Dialect::Script))
        }
        "rs" => Some((tree_sitter_rust::LANGUAGE.into(), Dialect::Rust)),
        _ => None,
    }
}

/// What the grammar of a file does with a line terminator.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Dialect {
    /// JavaScript and TypeScript: automatic semicolon insertion.
    Script,
    /// Rust: no automatic semicolon insertion, macros keep their spacing.
    Rust,
}

/// Writes the canonical serialization of the tree into the digest.
fn hash_tree(tree: &Tree, source: &str, dialect: Dialect) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(TAG_AST);
    let mut state = WalkState {
        dialect,
        previous_leaf_end: None,
        previous_is_restricted: false,
    };
    walk(tree.root_node(), source, &mut hasher, &mut state);
    hasher.finalize().to_hex().to_string()
}

/// State carried while walking the tree.
struct WalkState {
    /// The dialect of the language.
    dialect: Dialect,
    /// End offset of the previous kept leaf.
    previous_leaf_end: Option<usize>,
    /// Whether the previous kept leaf forbids a line terminator after it.
    previous_is_restricted: bool,
}

/// Writes one node and its children.
fn walk(node: Node, source: &str, hasher: &mut blake3::Hasher, state: &mut WalkState) {
    match state.dialect {
        Dialect::Script if is_reflectable_node(node.kind()) => {
            // The source of a function or a class can be read back at runtime
            // (`f.toString()`, `String(f)`, a computed name, or from another
            // file), so its text is significant.
            write_raw(node, source, hasher);
            return;
        }
        Dialect::Rust if is_spacing_sensitive_rust_node(node.kind()) => {
            // Rust keeps the spacing of a macro input and the adjacency of the
            // brackets of a type: rustc prints "a + b" for `stringify!(a + b)`
            // and "a+b" for `stringify!(a+b)`, a macro_rules capture keeps the
            // spacing too, and `Vec<Vec<u8>>` gives a proc macro two joined
            // brackets where `Vec<Vec<u8> >` gives two separate ones. These
            // regions are hashed as they are written.
            write_raw(node, source, hasher);
            return;
        }
        _ => {}
    }
    if is_comment_kind(node.kind()) {
        // A comment carries its text in the node itself for JavaScript, and in
        // hidden tokens for Rust, so the text is written here and the marker
        // children are skipped.
        let text = &source[node.byte_range()];
        if !is_meaningful_comment(node.kind(), text) {
            return;
        }
        hasher.update(&[TAG_NODE]);
        write_bytes(hasher, node.kind().as_bytes());
        write_bytes(hasher, text.trim_end_matches(['\r', '\n']).as_bytes());
        return;
    }
    hasher.update(&[TAG_NODE]);
    write_bytes(hasher, node.kind().as_bytes());
    if node.child_count() == 0 {
        write_leaf(node, source, hasher, state);
    } else {
        let mut cursor = node.walk();
        let children: Vec<Node> = node.children(&mut cursor).collect();
        if state.dialect == Dialect::Rust
            && children
                .iter()
                .any(|child| child.kind() == "attribute_item")
        {
            // An attribute macro receives the item it decorates as a token
            // stream, and it can tell a joined punctuation from a separate
            // one. The node that holds the attribute is therefore hashed as it
            // is written.
            write_raw(node, source, hasher);
            return;
        }
        for child in children {
            walk(child, source, hasher, state);
        }
    }
}

/// Writes a node as it is written in the file.
fn write_raw(node: Node, source: &str, hasher: &mut blake3::Hasher) {
    hasher.update(&[TAG_NODE]);
    write_bytes(hasher, node.kind().as_bytes());
    write_bytes(hasher, source[node.byte_range()].as_bytes());
}

/// Nodes whose source text a tool can read back at runtime.
///
/// The text of a function or a class is observable (`Function.prototype
/// .toString`, `String(f)`, a computed property name, or another file
/// reflecting it), so it is kept as it is written.
fn is_reflectable_node(kind: &str) -> bool {
    matches!(
        kind,
        "function_declaration"
            | "function_expression"
            | "generator_function"
            | "generator_function_declaration"
            | "arrow_function"
            | "method_definition"
            | "class"
            | "class_declaration"
            | "abstract_class_declaration"
    )
}

/// Writes a leaf token: its text plus the line terminator marker when the
/// position is semantically relevant.
fn write_leaf(node: Node, source: &str, hasher: &mut blake3::Hasher, state: &mut WalkState) {
    if let Some(end) = state.previous_leaf_end
        && state.dialect == Dialect::Script
        && state.previous_is_restricted
        && let Some(between) = source.get(end..node.start_byte())
        && has_line_terminator(between)
    {
        hasher.update(&[FLAG_LINE_TERMINATOR]);
    }
    let text = &source[node.byte_range()];
    write_bytes(hasher, text.as_bytes());
    state.previous_leaf_end = Some(node.end_byte());
    state.previous_is_restricted = forbids_line_terminator_after(node);
}

/// Writes a length prefixed byte string.
fn write_bytes(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

/// True when the text contains a JavaScript line terminator.
fn has_line_terminator(text: &str) -> bool {
    text.contains(['\n', '\r', '\u{2028}', '\u{2029}'])
}

/// Tokens after which a line terminator changes the meaning.
///
/// `throw` and `async` are the two the tree-sitter grammars do not model:
/// `throw\nx` is a syntax error in JavaScript but parses like `throw x`, and
/// `async\n(x) => x` is a syntax error but parses like `async(x) => x`. The
/// marker keeps both apart from their same-line spelling.
///
/// TypeScript has the same kind of rule for a few declaration headers:
/// `interface\nI {}`, `declare module\n'm' {}`, `export declare\nconst x`,
/// `function g<const\nT>()` and `function r(): asserts\ns is string` are
/// syntax errors for `tsc` while the grammar reads them as one declaration.
/// The parent node tells these keywords apart from the same words used as
/// ordinary identifiers, so the marker stays rare.
fn forbids_line_terminator_after(node: Node) -> bool {
    match node.kind() {
        "throw" | "async" => true,
        "interface" => parent_is(node, "interface_declaration"),
        "namespace" => parent_is(node, "internal_module"),
        _ => false,
    }
}

/// True when the parent of the node has the given kind.
fn parent_is(node: Node, kind: &str) -> bool {
    node.parent().is_some_and(|parent| parent.kind() == kind)
}

/// Rust nodes whose text carries a spacing that a tool can observe.
fn is_spacing_sensitive_rust_node(kind: &str) -> bool {
    matches!(kind, "token_tree" | "type_arguments")
}

/// True for comment nodes of every supported grammar.
fn is_comment_kind(kind: &str) -> bool {
    matches!(
        kind,
        "comment" | "line_comment" | "block_comment" | "html_comment"
    )
}

/// True when a comment changes the meaning of the code for a tool.
#[must_use]
pub fn is_meaningful_comment(kind: &str, text: &str) -> bool {
    if matches!(kind, "line_comment" | "block_comment") && is_rust_doc_comment(text) {
        return true;
    }
    let body = comment_body(text).to_ascii_lowercase();
    let body = body.trim();
    if body.is_empty() {
        return false;
    }
    if MEANINGFUL_COMMENT_DIRECTIVES
        .iter()
        .any(|directive| body.starts_with(directive))
    {
        return true;
    }
    UNKNOWN_DIRECTIVE_PREFIXES
        .iter()
        .any(|prefix| body.starts_with(prefix))
}

/// True for Rust doc comments (`///`, `//!`, `/** */`, `/*! */`).
fn is_rust_doc_comment(text: &str) -> bool {
    text.starts_with("///")
        || text.starts_with("//!")
        || text.starts_with("/**")
        || text.starts_with("/*!")
}

/// Removes the comment markers and trims the body.
fn comment_body(text: &str) -> &str {
    let body = if let Some(rest) = text.strip_prefix("<!--") {
        rest.strip_suffix("-->").unwrap_or(rest)
    } else if let Some(rest) = text.strip_prefix("/*") {
        // JSDoc style comments start with extra stars.
        let rest = rest.strip_suffix("*/").unwrap_or(rest);
        rest.trim_start().trim_start_matches('*').trim_start()
    } else if let Some(rest) = text.strip_prefix("//") {
        rest
    } else {
        text
    };
    body.trim()
}

#[cfg(test)]
mod depth_tests {
    use super::{Parser, exceeds_tree_depth};

    #[test]
    fn valid_tree_depth_and_siblings() {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_javascript::LANGUAGE.into())
            .unwrap();
        let mut seen = [false; 3];
        for n in 100..140 {
            let source = format!(
                "const a=0; const x={}1{}; const z=2;",
                "(".repeat(n),
                ")".repeat(n)
            );
            let tree = parser.parse(&source, None).unwrap();
            assert!(!tree.root_node().has_error());
            let mut cursor = tree.walk();
            let (mut depth, mut max_depth) = (0, 0);
            loop {
                if cursor.goto_first_child() {
                    depth += 1;
                    max_depth = max_depth.max(depth);
                } else {
                    while !cursor.goto_next_sibling() {
                        if !cursor.goto_parent() {
                            break;
                        }
                        depth -= 1;
                    }
                    if depth == 0 {
                        break;
                    }
                }
            }
            if (127..=129).contains(&max_depth) {
                seen[max_depth - 127] = true;
                assert_eq!(exceeds_tree_depth(&tree), max_depth > 128);
            }
        }
        assert_eq!(seen, [true; 3]);
    }
}
