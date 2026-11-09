//! Property and adversarial tests for the fingerprint module.
//!
//! AST mode assumes the caller's command is insensitive to discarded trivia.
//! These tests check selected token and trivia properties, not semantic equivalence.

use std::path::Path;

use kioku::fingerprint::{Mode, fingerprint};
use proptest::prelude::*;

/// Fingerprint of a source in AST mode.
fn ast_key(path: &str, source: &str) -> String {
    fingerprint(Path::new(path), source.as_bytes(), Mode::Ast)
}

/// Fingerprint of raw bytes, independent of the path.
fn bytes_key(source: &[u8]) -> String {
    fingerprint(Path::new("a.js"), source, Mode::Bytes)
}

/// Program shapes used by the whitespace properties.
#[derive(Clone, Debug)]
enum Shape {
    /// One JavaScript call.
    JsCall,
    /// JavaScript call at the top level.
    JsTopLevel,
    /// Typed TypeScript call.
    TsTyped,
    /// Rust call.
    RustCall,
}

impl Shape {
    /// File extension the source is written with.
    fn path(&self) -> &'static str {
        match self {
            Shape::JsCall | Shape::JsTopLevel => "a.js",
            Shape::TsTyped => "a.ts",
            Shape::RustCall => "a.rs",
        }
    }
}

fn shape() -> impl Strategy<Value = Shape> {
    prop_oneof![
        Just(Shape::JsCall),
        Just(Shape::JsTopLevel),
        Just(Shape::TsTyped),
        Just(Shape::RustCall),
    ]
}

/// Ordinary comment texts. They are fixed so that no generated comment can
/// look like a directive, which would keep it in the fingerprint.
fn ordinary_note() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("a plain note".to_string()),
        Just("nothing to see here".to_string()),
        Just("TODO later".to_string()),
        Just("keep this line".to_string()),
    ]
}

/// Whitespace that cannot change the meaning of the generated programs.
fn spacing() -> impl Strategy<Value = String> {
    prop_oneof![
        Just(" ".to_string()),
        Just("  ".to_string()),
        Just("\t".to_string()),
        Just("\n".to_string()),
        Just("\n\n".to_string()),
        Just(" \n ".to_string()),
    ]
}

/// Builds a program with the given whitespace between the call arguments.
fn program(shape: &Shape, spacing: &str, name: &str) -> String {
    match shape {
        Shape::JsCall => {
            format!("const value = compute({spacing}{name}{spacing},{spacing}beta{spacing});\n")
        }
        Shape::JsTopLevel => format!("compute({spacing}{name}{spacing},{spacing}beta{spacing});\n"),
        Shape::TsTyped => format!(
            "const value: number = compute({spacing}{name}{spacing},{spacing}beta{spacing});\n"
        ),
        Shape::RustCall => format!(
            "fn run() {{\n    let value = compute({spacing}{name}{spacing},{spacing}beta{spacing});\n}}\n"
        ),
    }
}

proptest! {
    /// Whitespace between arguments does not change the fingerprint.
    #[test]
    fn ast_ignores_whitespace(
        shape in shape(),
        first in spacing(),
        second in spacing(),
    ) {
        let one = program(&shape, &first, "alpha");
        let two = program(&shape, &second, "alpha");
        prop_assert_eq!(ast_key(shape.path(), &one), ast_key(shape.path(), &two));
    }

    /// Ordinary comments are noise, wherever they sit.
    #[test]
    fn ast_ignores_ordinary_comments(
        shape in shape(),
        space in spacing(),
        note in ordinary_note(),
    ) {
        let plain = program(&shape, &space, "alpha");
        let before = format!("// {note}\n{plain}");
        let after = format!("{plain}// {note}\n");
        let key = ast_key(shape.path(), &plain);
        prop_assert_eq!(&key, &ast_key(shape.path(), &before));
        prop_assert_eq!(&key, &ast_key(shape.path(), &after));
    }

    /// Two ordinary comments with different payloads stay equal.
    #[test]
    fn ast_ignores_ordinary_comment_payload(
        shape in shape(),
        space in spacing(),
        note in ordinary_note(),
    ) {
        let one = format!("// {note}\n{}", program(&shape, &space, "alpha"));
        let two = format!("// other {note}\n{}", program(&shape, &space, "alpha"));
        prop_assert_eq!(ast_key(shape.path(), &one), ast_key(shape.path(), &two));
    }

    /// A renamed identifier changes the fingerprint.
    #[test]
    fn ast_sees_token_changes(
        shape in shape(),
        space in spacing(),
        name in "[a-z]{1,8}",
        other in "[a-z]{1,8}",
    ) {
        prop_assume!(name != other);
        let one = program(&shape, &space, &name);
        let two = program(&shape, &space, &other);
        prop_assert_ne!(ast_key(shape.path(), &one), ast_key(shape.path(), &two));
    }

    /// A changed literal changes the fingerprint.
    #[test]
    fn ast_sees_literal_changes(
        shape in shape(),
        space in spacing(),
        number in 0u32..1000,
        other in 0u32..1000,
    ) {
        prop_assume!(number != other);
        let one = program(&shape, &space, &number.to_string());
        let two = program(&shape, &space, &other.to_string());
        prop_assert_ne!(ast_key(shape.path(), &one), ast_key(shape.path(), &two));
    }

    /// A meaningful comment, and its payload, change the fingerprint.
    #[test]
    fn ast_sees_meaningful_comments(
        shape in shape(),
        space in spacing(),
        reason in "[a-z]{1,12}",
        other in "[a-z]{1,12}",
    ) {
        prop_assume!(reason != other);
        let plain = program(&shape, &space, "alpha");
        let marked = format!("// @ts-expect-error {reason}\n{plain}");
        let marked_other = format!("// @ts-expect-error {other}\n{plain}");
        let key = ast_key(shape.path(), &plain);
        prop_assert_ne!(key, ast_key(shape.path(), &marked));
        prop_assert_ne!(
            ast_key(shape.path(), &marked),
            ast_key(shape.path(), &marked_other)
        );
    }

    /// In byte mode every changed byte changes the fingerprint.
    #[test]
    fn bytes_mode_sees_every_byte(
        bytes in prop::collection::vec(any::<u8>(), 1..64),
        index in 0usize..64,
        value in any::<u8>(),
    ) {
        let mut one = bytes.clone();
        let mut two = bytes.clone();
        let index = index % one.len();
        let previous = two[index];
        two[index] = value;
        prop_assume!(previous != value);
        one.truncate(one.len());
        prop_assert_ne!(bytes_key(&one), bytes_key(&two));
    }
}

/// A line terminator after `throw` is a syntax error, so the two spellings
/// must not share a fingerprint.
#[test]
fn throw_after_newline_is_not_the_same_source() {
    let invalid = ast_key("a.js", "throw\nx");
    let valid = ast_key("a.js", "throw x");
    assert_ne!(invalid, valid);
}

/// `async` followed by a line terminator is a syntax error before an arrow
/// function.
#[test]
fn async_before_newline_is_not_the_same_source() {
    let invalid = ast_key("a.js", "async\n(x)=>x");
    let valid = ast_key("a.js", "async(x)=>x");
    assert_ne!(invalid, valid);
}

/// Automatic semicolon insertion changes what `return` means.
#[test]
fn return_before_newline_is_not_the_same_source() {
    let inserted = ast_key("a.js", "function f() {\n    return\n    value\n}\n");
    let same_line = ast_key("a.js", "function f() {\n    return value\n}\n");
    assert_ne!(inserted, same_line);
}

/// A line terminator before `=>` is a syntax error, so the two spellings must
/// not share a fingerprint.
#[test]
fn arrow_before_newline_is_not_the_same_source() {
    let invalid = ast_key("a.js", "const g = ()\n=> 1;");
    let valid = ast_key("a.js", "const g = () => 1;");
    assert_ne!(invalid, valid);
}

/// TypeScript reads a few declaration headers on one line only.
#[test]
fn typescript_declaration_headers_stay_on_one_line() {
    let pairs = [
        (
            "interface\nI { a: number }\n",
            "interface I { a: number }\n",
        ),
        ("namespace\nN { }\n", "namespace N { }\n"),
        ("declare namespace\nN { }\n", "declare namespace N { }\n"),
        ("export namespace\nN { }\n", "export namespace N { }\n"),
        ("declare module\n'm' { }\n", "declare module 'm' { }\n"),
        (
            "export declare\nconst n: number;\n",
            "export declare const n: number;\n",
        ),
        (
            "function g<const\nT>(x: T) {}\n",
            "function g<const T>(x: T) {}\n",
        ),
        (
            "function r(): asserts\ns is string {}\n",
            "function r(): asserts s is string {}\n",
        ),
    ];
    for (split, together) in pairs {
        assert_ne!(
            ast_key("a.ts", split),
            ast_key("a.ts", together),
            "split header {split:?} was merged with {together:?}"
        );
    }
}

/// A prefix increment on a new line is not a postfix increment.
#[test]
fn increment_before_newline_is_not_the_same_source() {
    let prefixed = ast_key("a.js", "a\n++b");
    let erroring = ast_key("a.js", "a++b");
    assert_ne!(prefixed, erroring);
}

/// Rust macros see token boundaries: `a b` is not `ab`.
#[test]
fn rust_macro_token_boundaries_matter() {
    let split = ast_key("a.rs", "fn f() { foo!(a b); }");
    let joined = ast_key("a.rs", "fn f() { foo!(ab); }");
    assert_ne!(split, joined);
}

/// A comment between two Rust tokens does not join them.
#[test]
fn rust_macro_comment_does_not_join_tokens() {
    let commented = ast_key("a.rs", "fn f() { foo!(a/* c */b); }");
    let joined = ast_key("a.rs", "fn f() { foo!(ab); }");
    assert_ne!(commented, joined);
}

/// JSX text keeps its spaces.
#[test]
fn jsx_text_keeps_spaces() {
    let spaced = ast_key("a.tsx", "const view = <div> text </div>;");
    let tight = ast_key("a.tsx", "const view = <div>text</div>;");
    assert_ne!(spaced, tight);
}

/// Rust doc comments are kept, including their payload.
#[test]
fn rust_doc_comments_are_kept() {
    let plain = ast_key("a.rs", "fn f() {}\n");
    let documented = ast_key("a.rs", "/// doc\nfn f() {}\n");
    let other = ast_key("a.rs", "/// other\nfn f() {}\n");
    assert_ne!(plain, documented);
    assert_ne!(documented, other);
}

/// Inner Rust doc comments are kept too.
#[test]
fn rust_inner_doc_comments_are_kept() {
    let plain = ast_key("a.rs", "fn f() {}\n");
    let documented = ast_key("a.rs", "//! module doc\nfn f() {}\n");
    assert_ne!(plain, documented);
}

/// The shebang is part of the file.
#[test]
fn shebang_is_kept() {
    let node = ast_key("a.js", "#!/usr/bin/env node\nconst a = 1;\n");
    let deno = ast_key("a.js", "#!/usr/bin/env deno\nconst a = 1;\n");
    assert_ne!(node, deno);
}

/// Every documented directive changes the fingerprint.
#[test]
fn directive_files_fall_back_to_bytes() {
    // A directive applies to the line that follows it, so a file that carries
    // one is fingerprinted byte accurately.
    for directive in [
        "// @ts-expect-error",
        "// @ts-ignore",
        "// @ts-nocheck",
        "// eslint-disable no-console",
        "// eslint-disable-next-line no-console",
        "// prettier-ignore",
        "// istanbul ignore next",
        "// c8 ignore next",
        "// @vitest-environment jsdom",
        "/** @jsx h */",
        "// @some-unknown-directive",
        "// #__PURE__",
        "/*:: type A = string */",
    ] {
        let source = format!("{directive}\nconst a = 1;\n");
        assert_eq!(
            ast_key("a.ts", &source),
            bytes_key(source.as_bytes()),
            "directive {directive} was not treated as opaque"
        );
    }
    // Moving a directive changes the line it applies to.
    let one = "// @ts-expect-error\nconst a: number = 'x';\n";
    let two = "// @ts-expect-error\n\nconst a: number = 'x';\n";
    assert_ne!(ast_key("a.ts", one), ast_key("a.ts", two));
}

/// A directive we do not know yet is kept as well.
#[test]
fn unknown_directives_are_opaque() {
    for directive in ["// @some-tool-directive", "// #__PURE__"] {
        let source = format!("{directive}\nconst a = 1;\n");
        assert_eq!(ast_key("a.ts", &source), bytes_key(source.as_bytes()));
    }
}

/// Ordinary comments stay out of the fingerprint.
#[test]
fn ordinary_comments_are_dropped() {
    let plain = ast_key("a.ts", "const a = 1;\n");
    assert_eq!(plain, ast_key("a.ts", "// just a note\nconst a = 1;\n"));
    assert_eq!(plain, ast_key("a.ts", "/* just a note */\nconst a = 1;\n"));
}

/// A file the parser cannot read falls back to the byte fingerprint.
#[test]
fn unreadable_sources_fall_back_to_bytes() {
    let broken = b"const a = ;\n";
    assert_eq!(
        fingerprint(Path::new("a.js"), broken, Mode::Ast),
        bytes_key(broken)
    );
    let binary = b"const a = 1;\x00\x01";
    assert_eq!(
        fingerprint(Path::new("a.js"), binary, Mode::Ast),
        bytes_key(binary)
    );
    let unknown_language = b"{}";
    assert_eq!(
        fingerprint(Path::new("a.json"), unknown_language, Mode::Ast),
        bytes_key(unknown_language)
    );
}

/// Files above the size limit are not parsed.
#[test]
fn oversized_sources_fall_back_to_bytes() {
    let big = vec![b'a'; 2 * 1024 * 1024 + 1];
    assert_eq!(
        fingerprint(Path::new("a.js"), &big, Mode::Ast),
        bytes_key(&big)
    );
}

/// The fingerprint never depends on the path, so two machines agree.
#[test]
fn fingerprint_is_path_independent() {
    let source = b"const a = 1;\n";
    let local = fingerprint(Path::new("a.js"), source, Mode::Ast);
    let remote = fingerprint(Path::new("/home/somewhere/else/b.js"), source, Mode::Ast);
    assert_eq!(local, remote);
    let bytes_local = fingerprint(Path::new("a.js"), source, Mode::Bytes);
    let bytes_remote = fingerprint(Path::new("/home/somewhere/else/b.js"), source, Mode::Bytes);
    assert_eq!(bytes_local, bytes_remote);
}

/// Byte mode and AST mode never collide on the same content.
#[test]
fn modes_use_separate_domains() {
    let source = b"const a = 1;\n";
    assert_ne!(
        fingerprint(Path::new("a.js"), source, Mode::Ast),
        fingerprint(Path::new("a.js"), source, Mode::Bytes)
    );
}

/// The size limit is exactly two mebibytes.
#[test]
fn the_size_limit_is_exact() {
    const LIMIT: usize = 2 * 1024 * 1024;
    let prefix = "const a = 1;\n";
    let padding = LIMIT - prefix.len() - "//\n".len();
    let exact = format!("{prefix}//{}\n", "x".repeat(padding));
    assert_eq!(exact.len(), LIMIT);
    let over = format!("{exact} ");
    assert_eq!(over.len(), LIMIT + 1);
    assert_ne!(ast_key("a.js", &exact), bytes_key(exact.as_bytes()));
    assert_eq!(ast_key("a.js", &over), bytes_key(over.as_bytes()));
}

/// Every supported language is parsed, not fingerprinted byte accurately.
#[test]
fn supported_languages_are_parsed() {
    let cases = [
        ("a.js", "const a = 1;\n"),
        ("a.ts", "const a: number = 1;\n"),
        ("a.tsx", "const a = <div />;\n"),
        ("a.rs", "fn f() {}\n"),
    ];
    for (path, source) in cases {
        assert_ne!(
            ast_key(path, source),
            bytes_key(source.as_bytes()),
            "{path} was fingerprinted byte accurately"
        );
    }
}

/// An ordinary keyword followed by a line terminator changes nothing.
#[test]
fn an_ordinary_keyword_is_not_marked() {
    assert_eq!(
        ast_key("a.js", "const x = 1;\n"),
        ast_key("a.js", "const\nx = 1;\n")
    );
    assert_eq!(
        ast_key("a.ts", "const x: number = 1;\n"),
        ast_key("a.ts", "const\nx: number = 1;\n")
    );
}

/// Rust block doc comments are kept too.
#[test]
fn rust_block_doc_comments_are_kept() {
    let plain = ast_key("a.rs", "fn f() {}\n");
    let documented = ast_key("a.rs", "/** doc */\nfn f() {}\n");
    assert_ne!(plain, documented);
}

/// A Rust macro sees the spacing of its own input, so it is part of the
/// fingerprint.
#[test]
fn rust_macro_spacing_changes_the_fingerprint() {
    let spaced = "fn f() { foo!(a + b); }";
    let tight = "fn f() { foo!(a+b); }";
    let wider = "fn f() { foo!(a  +  b); }";
    assert_ne!(ast_key("a.rs", spaced), ast_key("a.rs", tight));
    assert_ne!(ast_key("a.rs", spaced), ast_key("a.rs", wider));
}

/// The adjacency of the brackets of a type is visible to a proc macro.
#[test]
fn rust_type_adjacency_is_part_of_the_fingerprint() {
    let joined = ast_key("a.rs", "type X = Vec<Vec<u8>>;");
    let separated = ast_key("a.rs", "type X = Vec<Vec<u8> >;");
    assert_ne!(joined, separated);
}

/// The source of a function can be read back at runtime, with any spelling
/// of the accessor, so its text is significant.
#[test]
fn javascript_callable_source_is_significant() {
    let one =
        "function f() { return 1; }\nprocess.exit(f['toString']().includes('return 1') ? 0 : 1);\n";
    let two = "function f() { return  1; }\nprocess.exit(f['toString']().includes('return 1') ? 0 : 1);\n";
    assert_ne!(ast_key("a.js", one), ast_key("a.js", two));
    let dotted = "function f() { return 1; }\nconsole.log(f.toString());\n";
    let dotted_changed = "function f() { return  1; }\nconsole.log(f.toString());\n";
    assert_ne!(ast_key("a.js", dotted), ast_key("a.js", dotted_changed));
    let stringified = "function f() { return 1; }\nconsole.log(String(f));\n";
    let stringified_changed = "function f() { return  1; }\nconsole.log(String(f));\n";
    assert_ne!(
        ast_key("a.js", stringified),
        ast_key("a.js", stringified_changed)
    );
}

/// The source of a function can be read back from another file, so the file
/// that defines it keeps its text significant as well.
#[test]
fn javascript_callable_source_is_significant_across_files() {
    let one = "export function f() { return 1; }\n";
    let two = "export function f() { return  1; }\n";
    assert_ne!(ast_key("a.js", one), ast_key("a.js", two));
    let arrow = "export const f = () => { return 1; };\n";
    let arrow_changed = "export const f = () => { return  1; };\n";
    assert_ne!(ast_key("a.js", arrow), ast_key("a.js", arrow_changed));
    let klass = "export class C { m() { return 1; } }\n";
    let klass_changed = "export class C { m() { return  1; } }\n";
    assert_ne!(ast_key("a.js", klass), ast_key("a.js", klass_changed));
}

/// The Rust position macros are found in the parse, whatever the trivia
/// between the name and the `!`.
#[test]
fn rust_position_macros_fall_back_to_bytes() {
    for source in [
        "fn main() { assert_eq!(line !(), 1); }\n",
        "fn main() { assert_eq!(line!(), 1); }\n",
        "fn main() { assert_eq!(column !(), 1); }\n",
        "fn main() { assert_eq!(column!(), 1); }\n",
    ] {
        assert_eq!(ast_key("a.rs", source), bytes_key(source.as_bytes()));
    }
}

/// An attribute macro receives the item it decorates as a token stream, so the
/// punctuation of that item is observable.
#[test]
fn rust_attributed_items_keep_their_punctuation() {
    let spaced = "#[probe::check]\nfn main() { let _ = 1+ -1; }\n";
    let tight = "#[probe::check]\nfn main() { let _ = 1+-1; }\n";
    assert_ne!(ast_key("a.rs", spaced), ast_key("a.rs", tight));
    let derived = "#[derive(Clone)]\nstruct S { a: u8 }\n";
    let derived_changed = "#[derive(Clone)]\nstruct S { a:u8 }\n";
    assert_ne!(ast_key("a.rs", derived), ast_key("a.rs", derived_changed));
}

/// The position macros are found in the parse, wherever they stand.
#[test]
fn rust_macro_files_fall_back_to_bytes() {
    // A macro can be a position macro, and it can be reached through an alias,
    // a proc macro or a macro of another file, so a file that calls a macro is
    // fingerprinted byte accurately.
    for source in [
        "fn main() { let l = line!(); }\n",
        "fn main() { let l = line !(); }\n",
        "fn main() { let c = column!(); }\n",
        "use std::line as here;\nfn main(){ assert_eq!(here!(),1); }\n",
        "fn main() { let s = stringify!(a + b); }\n",
        "fn main() { println!(\"x\"); }\n",
        "fn main() { foo!(a b); }\n",
        "macro_rules! m { () => { 1 } }\nfn main() { m!(); }\n",
    ] {
        assert_eq!(ast_key("a.rs", source), bytes_key(source.as_bytes()));
    }
}

/// An attribute does not hide the rest of the file, and every attribute is
/// kept.
#[test]
fn rust_attributed_items_do_not_hide_the_rest_of_the_file() {
    let one = "#[derive(Debug)]\nstruct S;\nfn other() { let _ = 1; }\n";
    let two = "#[derive(Debug)]\nstruct S;\nfn other() { let _ = 2; }\n";
    assert_ne!(ast_key("a.rs", one), ast_key("a.rs", two));
    let more = "#[derive(Debug)]\nstruct S;\nfn other() { let _ = 1; }\nfn more() {}\n";
    assert_ne!(ast_key("a.rs", one), ast_key("a.rs", more));
    let two_attributes = "#[derive(Debug)]\n#[allow(dead_code)]\nstruct S;\n";
    let other_attributes = "#[derive(Clone)]\n#[allow(dead_code)]\nstruct S;\n";
    assert_ne!(
        ast_key("a.rs", two_attributes),
        ast_key("a.rs", other_attributes)
    );
    let alone = "#[derive(Debug)]\n";
    let other_alone = "#[derive(Clone)]\n";
    assert_ne!(ast_key("a.rs", alone), ast_key("a.rs", other_alone));
}

/// TypeScript parses an ambient declaration in ways the grammar does not
/// reproduce, so the file is fingerprinted byte accurately.
#[test]
fn typescript_ambient_declarations_fall_back_to_bytes() {
    for source in [
        "declare namespace\nN { }\n",
        "declare namespace N { }\n",
        "declare module 'm' { }\n",
        "declare const w: number;\n",
    ] {
        assert_eq!(ast_key("a.ts", source), bytes_key(source.as_bytes()));
    }
}

/// Ordinary top level constants keep ignoring trivia.
#[test]
fn top_level_constants_ignore_trivia() {
    assert_eq!(
        ast_key("a.js", "const A = 1;\n"),
        ast_key("a.js", "const A =  1;\n")
    );
    assert_eq!(
        ast_key("a.ts", "const A: number = 1;\n"),
        ast_key("a.ts", "const A: number =  1;\n")
    );
    assert_eq!(
        ast_key("a.rs", "const A: u32 = 1;\n"),
        ast_key("a.rs", "const A: u32 =  1;\n")
    );
}

/// Deep valid input is hashed as bytes rather than recursing indefinitely.
#[test]
fn deeply_nested_source_falls_back_to_bytes() {
    let source = format!(
        "const x = {}1{};\n",
        "(".repeat(150_000),
        ")".repeat(150_000)
    );
    assert_eq!(ast_key("a.js", &source), bytes_key(source.as_bytes()));
}

/// The AST traversal handles nested siblings and enforces its depth boundary.
#[test]
fn depth_limit_has_exact_boundary_and_handles_siblings() {
    fn depth(source: &str) -> usize {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_javascript::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(source, None).unwrap();
        let mut cursor = tree.walk();
        let (mut current, mut max) = (0, 0);
        loop {
            if cursor.goto_first_child() {
                current += 1;
                max = max.max(current);
            } else {
                while !cursor.goto_next_sibling() {
                    if !cursor.goto_parent() {
                        return max;
                    }
                    current -= 1;
                }
            }
        }
    }
    let mut found = [false; 3];
    for n in 100..140 {
        let source = format!(
            "const a=0; const x={}1{}; const z=2;",
            "(".repeat(n),
            ")".repeat(n)
        );
        let d = depth(&source);
        if (127..=129).contains(&d) {
            found[d - 127] = true;
            let ast = ast_key("a.js", &source);
            let bytes = bytes_key(source.as_bytes());
            assert_eq!(ast == bytes, d > 128, "tree depth {d}");
        }
    }
    assert_eq!(found, [true; 3]);
}
