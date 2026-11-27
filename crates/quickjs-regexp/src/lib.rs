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
//! Lookbehind, named groups, and the `u`/`v` flag semantics (surrogate-pair handling,
//! full case folding). These are rejected at parse time or documented here rather than
//! silently mis-matched. The engine works on `char`s, so it is already code-point
//! granular, which covers the common cases.
//!
//! Unicode property escapes (`\p{…}`) *are* implemented, from tables generated out of the
//! Unicode Character Database by `scripts/gen-unicode-tables.py`.

mod matcher;
mod parser;
pub mod unicode;
mod unicode_tables;

pub use matcher::{Anchoring, Captures, Match};
pub use parser::parse;

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
    /// `v`: Unicode-aware matching with the full character-class set grammar.
    ///
    /// Mutually exclusive with `u`, and strict about the same things — plus it accepts
    /// nested classes, `&&`/`--` and `\q{…}` string sets, which `u` does not.
    pub unicode_sets: bool,
}

impl Flags {
    /// Parse a flag string, rejecting anything unknown or repeated.
    ///
    /// Returns the flags and the canonical order the spec fixes for `.flags`.
    pub fn parse(text: &str) -> Option<Flags> {
        let mut flags = Flags::default();
        let mut seen = [false; 8];
        for c in text.chars() {
            let (slot, target) = match c {
                'g' => (0, &mut flags.global),
                'i' => (1, &mut flags.ignore_case),
                'm' => (2, &mut flags.multiline),
                's' => (3, &mut flags.dot_all),
                'y' => (4, &mut flags.sticky),
                'u' => (5, &mut flags.unicode),
                'd' => (6, &mut flags.has_indices),
                'v' => (7, &mut flags.unicode_sets),
                _ => return None,
            };
            if seen[slot] {
                return None;
            }
            seen[slot] = true;
            *target = true;
        }
        // `u` and `v` are two dialects of the same strict grammar, so a pattern cannot ask
        // for both.
        if flags.unicode && flags.unicode_sets {
            return None;
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
            (self.unicode_sets, 'v'),
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
/// The tables live in [`unicode_tables`] and the grammar in [`unicode`], both generated or
/// derived from the Unicode Character Database. This alias is what the syntax tree carries,
/// so a pattern's properties are resolved exactly once, at parse time.
pub use unicode::Property as PropertyKind;

/// One node of the pattern's syntax tree.
#[derive(Debug, Clone)]
pub enum Node {
    /// Matches nothing: the identity of a concatenation.
    Empty,
    /// A literal character.
    ///
    /// A `u32` rather than a `char`: an unpaired surrogate is a legal code point in a JS
    /// string, and `\p{Cs}` has to be able to match it, but `char` cannot hold one. The
    /// value is a code point, always in `0..=0x10FFFF`.
    Char(u32),
    /// `.`
    Any,
    /// `[...]`, or a bare `\d`-style escape. The flag says whether the set is inverted.
    Class(bool, Vec<(u32, u32)>),
    /// A `v`-mode character class: sets, nesting, `&&`, `--` and `\q{…}` strings.
    Set(Box<ClassSet>),
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
            | Node::Set(_)
            | Node::Backref(_) => false,
            Node::Group { node, .. } => node.nullable(),
            Node::Concat(nodes) => nodes.iter().all(Node::nullable),
            Node::Alt(branches) => branches.iter().any(Node::nullable),
            Node::Repeat { min, node, .. } => *min == 0 || node.nullable(),
        }
    }
}

/// A character class as the `v` flag defines it.
///
/// `v` replaces the flat "list of ranges" of the older grammar with a real set algebra, so
/// `[[a-c]&&B]` is the intersection of a nested class with a letter and `[\q{BC|A}]` is a set
/// of *strings* rather than characters. That last part is why this cannot collapse into
/// [`Node::Class`]: a member of a set may be two characters long, so matching it consumes a
/// variable number of them.
#[derive(Debug, Clone)]
pub enum ClassSet {
    /// One character.
    Char(u32),
    /// An inclusive range.
    Range(u32, u32),
    /// A Unicode property escape.
    Property {
        /// Which property.
        kind: PropertyKind,
        /// `\P{…}` rather than `\p{…}`.
        negated: bool,
    },
    /// A `\d`-style predefined set, with its own inversion.
    Predefined {
        /// Whether the set is inverted (`\D`).
        negated: bool,
        /// The ranges the plain form stands for.
        ranges: Vec<(u32, u32)>,
    },
    /// One of several strings, from `\q{…}`. A member may be longer than one character.
    Strings(Vec<Vec<u32>>),
    /// Any member of any operand.
    Union(Vec<ClassSet>),
    /// Members present in every operand.
    Intersection(Vec<ClassSet>),
    /// Members of the first operand that the second does not have.
    Difference(Box<ClassSet>, Box<ClassSet>),
    /// `[^…]`.
    Negated(Box<ClassSet>),
}

/// Whether a `v`-mode set contains a member longer than one character.
///
/// A negated set is a complement of characters, so such a member cannot be expressed and the
/// grammar rejects the pattern instead of matching one character of a longer string. The
/// matcher also needs this: a set with a multi-character member cannot take the
/// single-character repetition fast path.
fn set_has_strings(set: &ClassSet) -> bool {
    match set {
        ClassSet::Strings(members) => members.iter().any(|member| member.len() > 1),
        ClassSet::Union(operands) | ClassSet::Intersection(operands) => {
            operands.iter().any(set_has_strings)
        }
        ClassSet::Difference(left, right) => set_has_strings(left) || set_has_strings(right),
        ClassSet::Negated(inner) => set_has_strings(inner),
        ClassSet::Char(_) | ClassSet::Range(..) | ClassSet::Property { .. } => false,
        ClassSet::Predefined { .. } => false,
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
    pub fn exec_at(&self, input: &[u32], from: usize) -> Option<Match> {
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
        let input: Vec<u32> = text.chars().map(|c| c as u32).collect();
        regex.exec_at(&input, 0).map(|m| {
            input[m.start..m.end]
                .iter()
                .map(|cp| char::from_u32(*cp).unwrap_or('\u{fffd}'))
                .collect()
        })
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
        let input: Vec<u32> = vec!['a' as u32];
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
        let input: Vec<u32> = "abc".chars().map(|c| c as u32).collect();
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

    /// A code point list for a `&str`, which is what the engine matches against.
    fn points(text: &str) -> Vec<u32> {
        text.chars().map(|c| c as u32).collect()
    }

    #[test]
    fn unicode_property_escapes_resolve() {
        let cases = [
            (r"\p{Lu}", 'A', true),
            (r"\p{Lu}", 'a', false),
            (r"\p{Ll}", 'a', true),
            // The group value `L` covers every letter.
            (r"\p{L}", 'A', true),
            // Aliases name the same property.
            (r"\p{Alpha}", 'A', true),
            (r"\p{Alphabetic}", 'A', true),
            (r"\p{sc=Grek}", '\u{3b1}', true),
            (r"\p{Script=Greek}", '\u{3b1}', true),
            (r"\p{Emoji}", '\u{1f600}', true),
            (r"\p{Zs}", ' ', true),
            // The negation.
            (r"\P{Lu}", 'a', true),
            (r"\P{Lu}", 'A', false),
        ];
        for (pattern, input, expected) in cases {
            let regex = Regex::compile(pattern, Flags::parse("u").expect("flags")).expect(pattern);
            let input = points(&input.to_string());
            assert_eq!(
                regex.exec_at(&input, 0).is_some(),
                expected,
                "{pattern} against {input:?}"
            );
        }
    }

    #[test]
    fn unknown_and_special_property_names_are_refused() {
        // The grammar's name set is closed, so a name it does not list is an error rather
        // than a property that matches nothing.
        for pattern in [
            r"\p{Nope}",
            // A bare script name is not allowed; `Script=` is required.
            r"\p{Greek}",
            // A binary property takes no value.
            r"\p{ASCII=Yes}",
            // Deprecated by Unicode and absent from the grammar's list.
            r"\p{Expands_On_NFC}",
            r"\p{Hyphen}",
            // Not a Unicode property at all.
            r"\p{Block=Adlam}",
            r"\p{General_Category}",
            r"\p{}",
            r"\p{Upper=Lower}",
        ] {
            assert!(
                Regex::compile(pattern, Flags::parse("u").expect("flags")).is_err(),
                "{pattern} should not compile"
            );
        }
    }

    #[test]
    fn a_lone_surrogate_is_a_code_point() {
        // `\p{Cs}` has to match an unpaired surrogate, which is why the input is `u32` and
        // not `char`.
        let regex = Regex::compile(r"\p{Cs}", Flags::parse("u").expect("flags")).expect("pattern");
        let input = vec![0xD800];
        assert_eq!(
            regex.exec_at(&input, 0).map(|m| (m.start, m.end)),
            Some((0, 1))
        );
        // A paired surrogate is one code point, so it is not `Cs`.
        let astral = points("\u{1f600}");
        assert!(regex.exec_at(&astral, 0).is_none());
    }

    #[test]
    fn a_long_repeat_does_not_exhaust_the_stack() {
        // A backtracking matcher recurses once per repetition; the fast path has to consume
        // a single-character body in a loop or this overflows the native stack.
        let regex =
            Regex::compile(r"^\p{ASCII}+$", Flags::parse("u").expect("flags")).expect("pattern");
        let input: Vec<u32> = (0u32..=0x7F).cycle().take(200_000).collect();
        assert!(regex.exec_at(&input, 0).is_some());
    }

    #[test]
    fn empty_matches_do_not_loop() {
        let regex = Regex::compile("(?:)", Flags::default()).expect("pattern");
        assert!(regex.nullable);
        let input: Vec<u32> = "ab".chars().map(|c| c as u32).collect();
        assert_eq!(
            regex.exec_at(&input, 0).map(|m| (m.start, m.end)),
            Some((0, 0))
        );
    }
}
