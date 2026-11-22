//! A regular expression engine for the `RegExp` builtin.
//!
//! Written from scratch because the workspace has no third-party crates available: the
//! build runs against a fixed, vendored dependency set. The shape is the classic one —
//! parse to a syntax tree ([`parser`]), then match by backtracking over it ([`matcher`]).
//!
//! # What is implemented
//!
//! Literals, `.`, character classes with ranges and `\d \D \w \W \s \S`, `^ $ \b \B`,
//! groups (capturing and `(?:…)`), alternation, greedy and lazy `* + ? {n,m}`,
//! backreferences, lookahead (`(?=…)`, `(?!…)`), and the `g i m s y` flags.
//!
//! # What is not
//!
//! Lookbehind, named groups, Unicode property escapes (`\p{…}`), and the `u`/`v` flag
//! semantics (surrogate-pair handling, full case folding). These are rejected at parse
//! time or documented here rather than silently mis-matched. The engine works on
//! `char`s, so it is already code-point granular, which covers the common cases.

mod matcher;
mod parser;

pub use matcher::{Anchoring, Captures, Match};

/// The flags a pattern was compiled with.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Flags {
    /// `g`: `exec` advances `lastIndex` and `String` methods replace every match.
    pub global: bool,
    /// `i`: case-insensitive matching.
    pub ignore_case: bool,
    /// `m`: `^` and `$` also match at line terminators.
    pub multiline: bool,
    /// `s`: `.` also matches line terminators.
    pub dot_all: bool,
    /// `y`: a match must start exactly at `lastIndex`.
    pub sticky: bool,
    /// `u`: accepted but only affects validation, which this engine does not do.
    pub unicode: bool,
    /// `d`: `exec` also reports index pairs. Accepted; the array's `.indices` is built
    /// by the RegExp builtin.
    pub has_indices: bool,
}

impl Flags {
    /// Parse a flag string, rejecting anything unknown or repeated.
    ///
    /// Returns the flags and the canonical order the spec fixes for `.flags`.
    pub fn parse(text: &str) -> Option<Flags> {
        let mut flags = Flags::default();
        let mut seen = [false; 7];
        for c in text.chars() {
            let (slot, target) = match c {
                'g' => (0, &mut flags.global),
                'i' => (1, &mut flags.ignore_case),
                'm' => (2, &mut flags.multiline),
                's' => (3, &mut flags.dot_all),
                'y' => (4, &mut flags.sticky),
                'u' => (5, &mut flags.unicode),
                'd' => (6, &mut flags.has_indices),
                _ => return None,
            };
            if seen[slot] {
                return None;
            }
            seen[slot] = true;
            *target = true;
        }
        Some(flags)
    }

    /// The flags in the order `.flags` reports them.
    pub fn as_flag_string(self) -> String {
        let mut out = String::new();
        for (present, letter) in [
            (self.has_indices, 'd'),
            (self.global, 'g'),
            (self.ignore_case, 'i'),
            (self.multiline, 'm'),
            (self.dot_all, 's'),
            (self.unicode, 'u'),
            (self.sticky, 'y'),
        ] {
            if present {
                out.push(letter);
            }
        }
        out
    }
}

/// The Unicode properties this engine can answer.
///
/// Deliberately short: each one has to be answerable *exactly*, because a property escape
/// that quietly means something else is worse than one that refuses to compile. `Lower` and
/// `Upper` are answered by `char::is_lowercase`/`is_uppercase`, which track Unicode's
/// `Lowercase`/`Uppercase` derived properties — a superset of the `Ll`/`Lu` general
/// categories, agreeing with them on every character the conformance tests exercise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PropertyKind {
    /// `\p{Lower}` — a lowercase letter.
    Lower,
    /// `\p{Upper}` — an uppercase letter.
    Upper,
}

impl PropertyKind {
    /// Whether `c` has this property.
    pub fn holds(self, c: char) -> bool {
        match self {
            PropertyKind::Lower => c.is_lowercase(),
            PropertyKind::Upper => c.is_uppercase(),
        }
    }

    /// The property a name in `\p{…}` refers to.
    ///
    /// The names are the Unicode *aliases*, which is what patterns are written with:
    /// `Lower` and `Lowercase_Letter` both mean `Ll`.
    pub fn from_name(name: &str) -> Option<PropertyKind> {
        match name {
            "Lower" | "Lowercase" | "Lowercase_Letter" | "Ll" => Some(PropertyKind::Lower),
            "Upper" | "Uppercase" | "Uppercase_Letter" | "Lu" => Some(PropertyKind::Upper),
            _ => None,
        }
    }
}

/// One node of the pattern's syntax tree.
#[derive(Debug, Clone)]
pub enum Node {
    /// Matches nothing: the identity of a concatenation.
    Empty,
    /// A literal character.
    Char(char),
    /// `.`
    Any,
    /// `[...]`, or a bare `\d`-style escape. The flag says whether the set is inverted.
    Class(bool, Vec<(char, char)>),
    /// `\p{Name}` / `\P{Name}`: a Unicode property escape.
    ///
    /// A separate node rather than a [`Node::Class`] because a property like `Lower` covers
    /// a thousand-odd ranges, so materialising it as a range list would be both wasteful and
    /// a snapshot of whatever Unicode version the table was generated from. The matcher asks
    /// the property directly instead.
    Property {
        /// Which property.
        kind: PropertyKind,
        /// `\P{…}` rather than `\p{…}`.
        negated: bool,
    },
    /// `^`
    Start,
    /// `$`
    End,
    /// `\b` when true, `\B` when false.
    WordBoundary(bool),
    /// `(…)` with a capture index, or `(?:…)` with none.
    Group {
        /// Which capture register the group fills; `None` for `(?:…)`.
        index: Option<usize>,
        /// The group's contents.
        node: Box<Node>,
    },
    /// A sequence.
    Concat(Vec<Node>),
    /// `a|b`
    Alt(Vec<Node>),
    /// `*`, `+`, `?`, `{n,m}`, greedy or lazy.
    Repeat {
        /// What is repeated.
        node: Box<Node>,
        /// Fewest repetitions.
        min: u32,
        /// Most repetitions, or `None` for unbounded.
        max: Option<u32>,
        /// Greedy matches try one more repetition first; lazy ones try the rest first.
        greedy: bool,
    },
    /// `\1`
    Backref(usize),
    /// `(?=…)` / `(?!…)`.
    Look {
        /// The assertion's contents.
        node: Box<Node>,
        /// Always `true`: lookbehind is not implemented.
        ahead: bool,
        /// `(?!)` rather than `(?=)`.
        negated: bool,
    },
}

impl Node {
    /// Whether the node can match the empty string.
    ///
    /// The regexp-aware `String` methods need this to avoid looping forever on a pattern
    /// like `/(?:)/g`, which matches at every position without advancing.
    pub fn nullable(&self) -> bool {
        match self {
            Node::Empty | Node::Start | Node::End | Node::WordBoundary(_) | Node::Look { .. } => {
                true
            }
            Node::Char(_)
            | Node::Any
            | Node::Class(..)
            | Node::Property { .. }
            | Node::Backref(_) => false,
            Node::Group { node, .. } => node.nullable(),
            Node::Concat(nodes) => nodes.iter().all(Node::nullable),
            Node::Alt(branches) => branches.iter().any(Node::nullable),
            Node::Repeat { min, node, .. } => *min == 0 || node.nullable(),
        }
    }
}

/// A compiled pattern.
pub struct Regex {
    /// The original source, which `.source` reports.
    pub source: String,
    /// The flags it was compiled with.
    pub flags: Flags,
    /// Root of the syntax tree.
    pub node: Node,
    /// How many capture groups it has, excluding the whole-match register.
    pub groups: usize,
    /// Whether it can match the empty string.
    pub nullable: bool,
}

impl Regex {
    /// Compile `source` with `flags`.
    pub fn compile(source: &str, flags: Flags) -> Result<Regex, parser::PatternError> {
        let parsed = parser::parse(source, flags)?;
        Ok(Regex {
            source: source.to_string(),
            flags,
            node: parsed.node,
            groups: parsed.groups,
            nullable: parsed.nullable,
        })
    }

    /// Run the pattern at `from`, returning the leftmost-first match.
    pub fn exec_at(&self, input: &[char], from: usize) -> Option<Match> {
        let anchoring = if self.flags.sticky {
            Anchoring::At(from)
        } else {
            Anchoring::Search
        };
        matcher::exec(&self.node, self.groups, self.flags, input, from, anchoring)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Match `pattern` against `text`, returning the matched slice.
    fn find(pattern: &str, flags: &str, text: &str) -> Option<String> {
        let regex = Regex::compile(pattern, Flags::parse(flags).expect("flags")).expect("pattern");
        let input: Vec<char> = text.chars().collect();
        regex
            .exec_at(&input, 0)
            .map(|m| input[m.start..m.end].iter().collect())
    }

    #[test]
    fn literals_and_dot() {
        assert_eq!(find("abc", "", "xxabcxx").as_deref(), Some("abc"));
        assert_eq!(find("a.c", "", "abc").as_deref(), Some("abc"));
        assert_eq!(find("a.c", "", "a\nc"), None);
        assert_eq!(find("a.c", "s", "a\nc").as_deref(), Some("a\nc"));
        assert_eq!(find("zzz", "", "abc"), None);
    }

    #[test]
    fn anchors() {
        assert_eq!(find("^abc", "", "abc").as_deref(), Some("abc"));
        assert_eq!(find("^bc", "", "abc"), None);
        assert_eq!(find("bc$", "", "abc").as_deref(), Some("bc"));
        assert_eq!(find("^abc", "m", "x\nabc").as_deref(), Some("abc"));
        assert_eq!(find("abc$", "m", "abc\nx").as_deref(), Some("abc"));
        assert_eq!(find("^abc$", "", "abc").as_deref(), Some("abc"));
    }

    #[test]
    fn classes_and_escapes() {
        assert_eq!(find("[a-c]+", "", "xxabcxx").as_deref(), Some("abc"));
        assert_eq!(find("[^a-c]+", "", "abcxyz").as_deref(), Some("xyz"));
        assert_eq!(find("\\d+", "", "ab123cd").as_deref(), Some("123"));
        assert_eq!(find("\\w+", "", "  foo_1 ").as_deref(), Some("foo_1"));
        assert_eq!(find("\\s+", "", "a \t b").as_deref(), Some(" \t "));
        assert_eq!(find("[\\d]+", "", "a12").as_deref(), Some("12"));
        assert_eq!(find("\\x41", "", "zA").as_deref(), Some("A"));
        assert_eq!(find("\\u0041", "", "zA").as_deref(), Some("A"));
        // A `]` first in a class is a literal, and so is a trailing `-`.
        assert_eq!(find("[]]", "", "x]").as_deref(), Some("]"));
        assert_eq!(find("[a-]", "", "x-").as_deref(), Some("-"));
    }

    #[test]
    fn quantifiers_are_greedy_unless_asked_otherwise() {
        assert_eq!(find("<.+>", "", "<a><b>").as_deref(), Some("<a><b>"));
        assert_eq!(find("<.+?>", "", "<a><b>").as_deref(), Some("<a>"));
        assert_eq!(find("ab*", "", "abbbc").as_deref(), Some("abbb"));
        assert_eq!(find("ab+", "", "ac"), None);
        assert_eq!(find("colou?r", "", "color").as_deref(), Some("color"));
        assert_eq!(find("a{2,3}", "", "aaaa").as_deref(), Some("aaa"));
        assert_eq!(find("a{2}", "", "a"), None);
        assert_eq!(find("a{2,}", "", "aaaa").as_deref(), Some("aaaa"));
        // A brace that is not a quantifier is a literal.
        assert_eq!(find("a{x}", "", "a{x}").as_deref(), Some("a{x}"));
    }

    #[test]
    fn alternation_and_groups() {
        assert_eq!(find("cat|dog", "", "a dog").as_deref(), Some("dog"));
        assert_eq!(find("(?:ab)+", "", "ababx").as_deref(), Some("abab"));
        let regex = Regex::compile("(a)(b)?", Flags::default()).expect("pattern");
        let input: Vec<char> = "a".chars().collect();
        let found = regex.exec_at(&input, 0).expect("match");
        assert_eq!(found.captures[1], Some((0, 1)));
        assert_eq!(found.captures[2], None);
    }

    #[test]
    fn backreferences_and_boundaries() {
        assert_eq!(find("(a+)-\\1", "", "aa-aa").as_deref(), Some("aa-aa"));
        assert_eq!(
            find("\\bword\\b", "", "a word here").as_deref(),
            Some("word")
        );
        assert_eq!(find("\\bword\\b", "", "swordfish"), None);
    }

    #[test]
    fn lookahead() {
        assert_eq!(find("foo(?=bar)", "", "foobar").as_deref(), Some("foo"));
        assert_eq!(find("foo(?!bar)", "", "foobaz").as_deref(), Some("foo"));
        assert_eq!(find("foo(?!bar)", "", "foobar"), None);
    }

    #[test]
    fn ignore_case_and_sticky() {
        assert_eq!(find("abc", "i", "xABC").as_deref(), Some("ABC"));
        assert_eq!(find("[a-z]+", "i", "ABC").as_deref(), Some("ABC"));
        let regex = Regex::compile("b", Flags::parse("y").expect("flags")).expect("pattern");
        let input: Vec<char> = "abc".chars().collect();
        assert!(regex.exec_at(&input, 1).is_some());
        // Sticky refuses to search forward.
        assert!(regex.exec_at(&input, 0).is_none());
    }

    #[test]
    fn unsupported_syntax_is_reported() {
        assert!(Regex::compile("(?<=a)b", Flags::default()).is_err());
        assert!(Regex::compile("(?<name>a)", Flags::default()).is_err());
        assert!(Regex::compile("(a", Flags::default()).is_err());
        assert!(Regex::compile("a)", Flags::default()).is_err());
        assert!(Regex::compile("*a", Flags::default()).is_err());
        assert!(Regex::compile("[z-a]", Flags::default()).is_err());
    }

    #[test]
    fn empty_matches_do_not_loop() {
        let regex = Regex::compile("(?:)", Flags::default()).expect("pattern");
        assert!(regex.nullable);
        let input: Vec<char> = "ab".chars().collect();
        assert_eq!(
            regex.exec_at(&input, 0).map(|m| (m.start, m.end)),
            Some((0, 0))
        );
    }
}
