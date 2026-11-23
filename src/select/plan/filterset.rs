//! Lot L4e : évaluateur local d'un **sous-ensemble strict** des filtersets nextest 0.9.151.
//!
//! Sert à savoir, sans lancer nextest, quels tests de l'inventaire un `-E` utilisateur
//! sélectionne (voir [`super::restrict_to_user_filter`]). Tout ce qui n'est pas dans le
//! sous-ensemble donne `None` : l'appelant garde alors son comportement d'avant (aucune
//! restriction). Une erreur de lecture n'est donc jamais un faux « ne matche pas ».
//!
//! Grammaire reconnue (sémantique vérifiée contre `cargo nextest list -E … ` 0.9.151 ;
//! l'équivalence est rejouée par `tests/select_l4e_filter.rs`) :
//!
//! ```text
//! or      := and ( ('|' | '+' | 'or') and )*
//! and     := unary ( ('&' | 'and' | '-') unary )*      // '-' : a - b = a & not b
//! unary   := ('not' | '!') unary | primary
//! primary := '(' or ')' | 'all' '(' ')' | 'none' '(' ')'
//!          | 'test' '(' ['=' | '~'] NOM ')' | 'binary_id' '(' '=' ID ')'
//! ```
//!
//! Priorités (mesurées) : `not`/`!` > `&`/`and`/`-` > `|`/`+`/`or`, associativité à gauche.
//! `-` a le niveau de `&` (`a | b - c` = `a | (b - c)`). Les blancs (espace ASCII seulement :
//! la tabulation est refusée par nextest) sont libres entre les éléments. `test(x)` sans
//! préfixe est « contient », sensible à la casse, sur le nom du test (sans l'identifiant du
//! binaire) ; `test(=x)` est l'égalité ; `test(~x)` « contient ».
//!
//! Hors sous-ensemble (→ `None`) : regex `/…/`, glob `#…`, `binary_id(x)` sans `=` (glob par
//! défaut) ou `binary_id(~x)`, `package()`, `kind()`, `platform()`, `deps()`, `rdeps()`,
//! arguments avec blanc, guillemet, antislash, virgule ou tout caractère hors
//! `[A-Za-z0-9_:]` (`-` et `/` en plus pour un identifiant de binaire), chaîne non terminée,
//! expression non ASCII, de plus de 4 096 octets ou imbriquée sur plus de 32 niveaux.

use std::collections::BTreeSet;

/// Taille maximale d'une expression évaluée (au-delà : non supporté).
const MAX_EXPR_BYTES: usize = 4096;
/// Profondeur maximale de parenthèses et de `not` (pas de débordement de pile).
const MAX_DEPTH: usize = 32;

/// Comment `test(…)` compare le texte au nom du test.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Matcher {
    /// `test(=x)` : égalité.
    Equals(String),
    /// `test(x)` ou `test(~x)` : contient (sensible à la casse).
    Contains(String),
}

/// Expression de filterset du sous-ensemble supporté.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterSet(Node);

#[derive(Debug, Clone, PartialEq, Eq)]
enum Node {
    All,
    Empty,
    Test(Matcher),
    BinaryId(String),
    Not(Box<Node>),
    And(Vec<Node>),
    Or(Vec<Node>),
}

impl FilterSet {
    /// Analyse `expr`. `None` si l'expression sort du sous-ensemble supporté (voir le
    /// module) : l'appelant ne doit alors rien restreindre.
    #[must_use]
    pub fn parse(expr: &str) -> Option<Self> {
        if expr.len() > MAX_EXPR_BYTES || !expr.is_ascii() {
            return None;
        }
        let mut parser = Parser {
            src: expr.as_bytes(),
            pos: 0,
        };
        let node = parser.or(0)?;
        parser.skip_blanks();
        (parser.pos == parser.src.len()).then_some(Self(node))
    }

    /// Identifiants de binaire cités par `binary_id(=…)`. nextest refuse (`operator didn't
    /// match any binary IDs`) un identifiant inconnu de l'espace de travail, même sous `not` :
    /// l'appelant doit s'en assurer avant de conclure « aucun test ne matche ».
    #[must_use]
    pub fn binary_ids(&self) -> BTreeSet<&str> {
        let mut ids = BTreeSet::new();
        self.0.collect_binary_ids(&mut ids);
        ids
    }

    /// Le test `name` du binaire `binary_id` est-il dans l'ensemble ?
    #[must_use]
    pub fn matches(&self, binary_id: &str, name: &str) -> bool {
        self.0.eval(binary_id, name)
    }
}

impl Node {
    fn collect_binary_ids<'a>(&'a self, ids: &mut BTreeSet<&'a str>) {
        match self {
            Node::BinaryId(id) => {
                ids.insert(id.as_str());
            }
            Node::Not(inner) => inner.collect_binary_ids(ids),
            Node::And(items) | Node::Or(items) => {
                for item in items {
                    item.collect_binary_ids(ids);
                }
            }
            Node::All | Node::Empty | Node::Test(_) => {}
        }
    }

    fn eval(&self, binary_id: &str, name: &str) -> bool {
        match self {
            Node::All => true,
            Node::Empty => false,
            Node::Test(Matcher::Equals(text)) => name == text,
            Node::Test(Matcher::Contains(text)) => name.contains(text.as_str()),
            Node::BinaryId(id) => binary_id == id,
            Node::Not(inner) => !inner.eval(binary_id, name),
            Node::And(items) => items.iter().all(|n| n.eval(binary_id, name)),
            Node::Or(items) => items.iter().any(|n| n.eval(binary_id, name)),
        }
    }
}

struct Parser<'a> {
    src: &'a [u8],
    pos: usize,
}

/// Premier niveau : caractère permis dans un nom de test.
fn test_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b':'
}

/// Caractère permis dans un identifiant de binaire.
fn binary_char(byte: u8) -> bool {
    test_char(byte) || byte == b'-' || byte == b'/'
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.src.get(self.pos).copied()
    }

    fn skip_blanks(&mut self) {
        while self.peek() == Some(b' ') {
            self.pos += 1;
        }
    }

    /// Mot en minuscules et `_` à la position courante (sans le consommer).
    fn word(&self) -> &str {
        let rest = &self.src[self.pos..];
        let len = rest
            .iter()
            .take_while(|b| b.is_ascii_lowercase() || **b == b'_')
            .count();
        // Les octets sont ASCII (vérifié par `parse`) : jamais d'erreur d'UTF-8.
        std::str::from_utf8(&rest[..len]).unwrap_or("")
    }

    fn eat(&mut self, byte: u8) -> bool {
        if self.peek() == Some(byte) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    /// `or := and ( ('|' | '+' | 'or') and )*`
    fn or(&mut self, depth: usize) -> Option<Node> {
        let mut items = vec![self.and(depth)?];
        loop {
            self.skip_blanks();
            if self.eat(b'|') || self.eat(b'+') || self.eat_word("or") {
                items.push(self.and(depth)?);
            } else {
                break;
            }
        }
        Some(if items.len() == 1 {
            items.remove(0)
        } else {
            Node::Or(items)
        })
    }

    /// `and := unary ( ('&' | 'and' | '-') unary )*`, `a - b` valant `a & not b`.
    fn and(&mut self, depth: usize) -> Option<Node> {
        let mut items = vec![self.unary(depth)?];
        loop {
            self.skip_blanks();
            if self.eat(b'&') || self.eat_word("and") {
                items.push(self.unary(depth)?);
            } else if self.eat(b'-') {
                items.push(Node::Not(Box::new(self.unary(depth)?)));
            } else {
                break;
            }
        }
        Some(if items.len() == 1 {
            items.remove(0)
        } else {
            Node::And(items)
        })
    }

    fn eat_word(&mut self, keyword: &str) -> bool {
        if self.word() == keyword {
            self.pos += keyword.len();
            true
        } else {
            false
        }
    }

    fn unary(&mut self, depth: usize) -> Option<Node> {
        self.skip_blanks();
        if self.eat(b'!') || self.eat_word("not") {
            if depth >= MAX_DEPTH {
                return None;
            }
            return Some(Node::Not(Box::new(self.unary(depth + 1)?)));
        }
        self.primary(depth)
    }

    fn primary(&mut self, depth: usize) -> Option<Node> {
        if self.eat(b'(') {
            if depth >= MAX_DEPTH {
                return None;
            }
            let inner = self.or(depth + 1)?;
            self.skip_blanks();
            return self.eat(b')').then_some(inner);
        }
        let name = self.word().to_owned();
        self.pos += name.len();
        self.skip_blanks();
        if !self.eat(b'(') {
            return None;
        }
        match name.as_str() {
            "all" | "none" => {
                self.skip_blanks();
                if !self.eat(b')') {
                    return None;
                }
                Some(if name == "all" {
                    Node::All
                } else {
                    Node::Empty
                })
            }
            "test" => {
                let matcher = self.test_argument()?;
                self.eat(b')').then_some(Node::Test(matcher))
            }
            "binary_id" => {
                if !self.eat(b'=') {
                    return None; // glob par défaut, `~`, regex… : hors sous-ensemble
                }
                let id = self.text(binary_char)?;
                self.eat(b')').then_some(Node::BinaryId(id))
            }
            _ => None,
        }
    }

    fn test_argument(&mut self) -> Option<Matcher> {
        if self.eat(b'=') {
            self.text(test_char).map(Matcher::Equals)
        } else {
            self.eat(b'~');
            self.text(test_char).map(Matcher::Contains)
        }
    }

    /// Texte non vide de caractères permis, collé à la position courante.
    fn text(&mut self, allowed: fn(u8) -> bool) -> Option<String> {
        let start = self.pos;
        while self.peek().is_some_and(allowed) {
            self.pos += 1;
        }
        if self.pos == start {
            return None;
        }
        std::str::from_utf8(&self.src[start..self.pos])
            .ok()
            .map(str::to_owned)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Corpus de la sonde nextest (mini-crate `pro-be`) : `(binary_id, nom)`.
    const CORPUS: &[(&str, &str)] = &[
        ("pro-be", "tests::foo"),
        ("pro-be", "tests::foobar"),
        ("pro-be", "tests::Foo_up"),
        ("pro-be", "other::t::foo"),
        ("pro-be", "other::t::deep_ci2"),
        ("pro-be::bin/tool", "tests::foo"),
        ("pro-be::bin/tool", "tests::tool_only"),
        ("pro-be::it_one", "foo"),
        ("pro-be::it_one", "sub::foo"),
        ("pro-be::it_one", "ci2_int"),
        ("pro-be::two", "bar"),
        ("pro-be::two", "foo"),
    ];

    fn eval(expr: &str) -> Vec<String> {
        let set = FilterSet::parse(expr).unwrap_or_else(|| panic!("non supporté : {expr:?}"));
        CORPUS
            .iter()
            .filter(|(b, n)| set.matches(b, n))
            .map(|(b, n)| format!("{b} {n}"))
            .collect()
    }

    fn count(expr: &str) -> usize {
        eval(expr).len()
    }

    #[test]
    fn test_matchers_default_contains_equals_and_tilde() {
        assert_eq!(count("test(foo)"), 7);
        assert_eq!(eval("test(foo)"), eval("test(~foo)"));
        assert_eq!(
            eval("test(=foo)"),
            ["pro-be::it_one foo", "pro-be::two foo"]
        );
        assert_eq!(
            eval("test(=tests::foo)"),
            ["pro-be tests::foo", "pro-be::bin/tool tests::foo"]
        );
        // Sensible à la casse ; le nom ne contient pas l'identifiant du binaire.
        assert_eq!(eval("test(Foo)"), ["pro-be tests::Foo_up"]);
        assert_eq!(count("test(it_one)"), 0);
        assert_eq!(count("test(ci2)"), 2);
    }

    #[test]
    fn binary_id_is_an_exact_match() {
        assert_eq!(count("binary_id(=pro-be)"), 5);
        assert_eq!(count("binary_id(=pro-be::it_one)"), 3);
        assert_eq!(count("binary_id(=pro-be::bin/tool)"), 2);
        assert_eq!(count("binary_id(=pro)"), 0);
    }

    #[test]
    fn binary_ids_lists_every_cited_binary_even_under_not() {
        let set =
            FilterSet::parse("binary_id(=a) | not (binary_id(=b::c) & test(x)) - binary_id(=a)")
                .expect("supported");
        assert_eq!(set.binary_ids(), BTreeSet::from(["a", "b::c"]));
        assert!(
            FilterSet::parse("test(x) | all()")
                .expect("ok")
                .binary_ids()
                .is_empty()
        );
    }

    #[test]
    fn all_and_none() {
        assert_eq!(count("all()"), CORPUS.len());
        assert_eq!(count("all( )"), CORPUS.len());
        assert_eq!(count("none()"), 0);
        assert_eq!(count("not none()"), CORPUS.len());
    }

    #[test]
    fn every_spelling_of_each_operator() {
        for (a, b) in [
            ("test(foo) & test(bar)", "test(foo) and test(bar)"),
            ("test(foo) | test(bar)", "test(foo) + test(bar)"),
            ("test(foo) | test(bar)", "test(foo) or test(bar)"),
            ("not test(foo)", "!test(foo)"),
            ("not test(foo)", "not(test(foo))"),
            ("test(~foo) - test(=foo)", "test(~foo) & not test(=foo)"),
            ("test(foo)&test(bar)", "test(foo)  and  test(bar)"),
            ("test(foo)|test(bar)", "(test(foo))|( test(bar) )"),
            ("not not test(foo)", "!!test(foo)"),
            ("not!test(foo)", "!not test(foo)"),
        ] {
            assert_eq!(eval(a), eval(b), "{a} ≠ {b}");
        }
        assert_eq!(eval("test(foo) & test(bar)"), ["pro-be tests::foobar"]);
        assert_eq!(count("test(foo) | test(bar)"), 8);
        assert_eq!(count("not test(foo)"), CORPUS.len() - 7);
        assert_eq!(count("test(~foo) - test(=foo)"), 5);
    }

    #[test]
    fn precedence_not_over_and_over_or() {
        // `not` plus fort que `&` : (not foo) & bar.
        assert_eq!(eval("not test(foo) & test(bar)"), ["pro-be::two bar"]);
        // `&` plus fort que `|` : foo | (bar & =bar), pas (foo | bar) & =bar.
        assert_eq!(count("test(foo) | test(bar) & test(=bar)"), 8);
        assert_eq!(
            eval("(test(foo) | test(bar)) & test(=bar)"),
            ["pro-be::two bar"]
        );
        // (foo & bar) | tool.
        assert_eq!(
            eval("test(foo) & test(bar) | test(tool)"),
            ["pro-be tests::foobar", "pro-be::bin/tool tests::tool_only"]
        );
        // `-` a le niveau de `&` (plus fort que `|`) : =foo | (~foo - =foo) = ~foo.
        assert_eq!(
            eval("test(=foo) | test(foo) - test(=foo)"),
            eval("test(foo)")
        );
        // Associativité à gauche de `-`.
        assert_eq!(
            eval("test(=foo) | test(~foo) - test(=foo) - test(~tests)"),
            [
                "pro-be other::t::foo",
                "pro-be::it_one foo",
                "pro-be::it_one sub::foo",
                "pro-be::two foo"
            ]
        );
        assert_eq!(eval("all() - not test(foo)"), eval("test(foo)"));
        assert_eq!(count("all() - test(=bar) & test(=bar)"), 0);
    }

    #[test]
    fn keywords_inside_test_names_are_just_text() {
        for word in ["not", "and", "or", "all", "none", "test", "binary_id"] {
            assert!(
                FilterSet::parse(&format!("test({word})")).is_some(),
                "{word}"
            );
        }
    }

    #[test]
    fn unsupported_or_malformed_expressions_give_none() {
        for expr in [
            // hors sous-ensemble
            "test(/foo/)",
            "test(#foo*)",
            "test(=a b)",
            "test( foo)",
            "test(foo )",
            "test(=foo,bar)",
            "test(\"foo\")",
            "test(a\\)b)",
            "test(=foo-bar)",
            "test(a.b)",
            "test(~=x)",
            "binary_id(pro-be)",
            "binary_id(~it)",
            "binary_id(/x/)",
            "binary_id( =pro-be)",
            "binary_id(=pro be)",
            "package(foo)",
            "kind(lib)",
            "platform(host)",
            "deps(foo)",
            "rdeps(foo)",
            "test(foo) & package(x)",
            "not kind(test)",
            // mal formé (nextest les refuse aussi)
            "",
            "   ",
            "test(foo",
            "test(foo))",
            "test()",
            "test(=)",
            "test(~)",
            "binary_id(=)",
            "()",
            "all",
            "all(x)",
            "all()none()",
            "ALL()",
            "NOT test(foo)",
            "TEST(foo)",
            "test(foo) AND test(bar)",
            "test(foo) test(bar)",
            "test(foo) &",
            "& test(foo)",
            "test(foo) && test(bar)",
            "test(foo) || test(bar)",
            "test(foo) & & test(bar)",
            "-test(foo)",
            "+test(foo)",
            "test(foo) andtest(bar)",
            "nottest(foo)",
            "(test(foo)",
            "test(foo))",
            "test(foo)\t&\ttest(bar)",
            "test(foo)\n& test(bar)",
            "tëst(foo)",
            "test(fóo)",
        ] {
            assert!(
                FilterSet::parse(expr).is_none(),
                "{expr:?} aurait dû être refusé"
            );
        }
    }

    #[test]
    fn nesting_and_size_are_bounded() {
        let ok_depth = format!("{}all(){}", "(".repeat(MAX_DEPTH), ")".repeat(MAX_DEPTH));
        assert!(FilterSet::parse(&ok_depth).is_some());
        let deep = format!("{}all(){}", "(".repeat(5000), ")".repeat(5000));
        assert!(FilterSet::parse(&deep).is_none());
        let nots = format!("{}all()", "not ".repeat(2000));
        assert!(FilterSet::parse(&nots).is_none());
        let long = vec!["test(a)"; 1000].join(" | ");
        assert!(long.len() > MAX_EXPR_BYTES);
        assert!(FilterSet::parse(&long).is_none());
        let many = vec!["test(a)"; 400].join(" | ");
        assert!(many.len() <= MAX_EXPR_BYTES);
        assert!(FilterSet::parse(&many).is_some());
    }

    #[test]
    fn blanks_are_free_between_tokens() {
        assert_eq!(eval("  not   (  test(foo)   )   "), eval("not test(foo)"));
        assert_eq!(eval("test (foo)"), eval("test(foo)"));
        assert_eq!(
            eval("binary_id (=pro-be::two)"),
            eval("binary_id(=pro-be::two)")
        );
    }
}
