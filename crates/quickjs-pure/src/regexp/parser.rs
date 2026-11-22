//! Parse an ECMAScript regular expression source into [`Node`]s.
//!
//! The grammar here is the classic (Annex-B-free) pattern syntax: alternation,
//! concatenation, quantifiers, groups, character classes, escapes, assertions and
//! backreferences. Constructs this engine does not implement are rejected with a
//! `SyntaxError` rather than silently matching the wrong thing — a pattern that
//! cannot work is better reported than half-run.
//!
//! Nothing here is Unicode-mode aware: `u` and `v` change how a pattern is read
//! (surrogate pairs, property escapes, case folding), and this engine works on
//! `char`s, which is already code-point granular. That matches the common cases and is
//! documented as a limitation rather than pretended away.

use super::Node;

/// Why a pattern could not be parsed.
#[derive(Debug, Clone)]
pub struct PatternError {
    /// What went wrong, in the shape the reference reports it.
    pub message: String,
    /// Byte offset in the pattern source.
    pub position: usize,
}

/// A parsed pattern: the root node plus how many capture groups it has.
pub struct Parsed {
    /// Root of the syntax tree.
    pub node: Node,
    /// Number of capturing groups, so the engine can size its capture array.
    pub groups: usize,
    /// `true` when the pattern can match an empty string, which `String.prototype.split`
    /// and `RegExp.prototype[Symbol.replace]` both have to guard against.
    pub nullable: bool,
}

struct Parser {
    chars: Vec<char>,
    pos: usize,
    groups: usize,
    /// Whether the `u` flag is set, which turns several Annex B liberties into syntax
    /// errors. See [`Parser::annex_b_char`].
    unicode: bool,
}

/// Parse `source` with `flags`.
pub fn parse(source: &str, flags: crate::regexp::Flags) -> Result<Parsed, PatternError> {
    // `u` is the one flag the *syntax* depends on: it decides whether the Annex B
    // liberties below are allowed at all.
    let mut parser = Parser {
        chars: source.chars().collect(),
        pos: 0,
        groups: 0,
        unicode: flags.unicode,
    };
    let node = parser.disjunction(0)?;
    if parser.pos < parser.chars.len() {
        // Only a stray `)` gets here, since everything else consumes its own tokens.
        return Err(parser.error("unmatched ')'"));
    }
    let nullable = node.nullable();
    Ok(Parsed {
        node,
        groups: parser.groups,
        nullable,
    })
}

impl Parser {
    fn error(&self, message: &str) -> PatternError {
        PatternError {
            message: message.to_string(),
            position: self.pos,
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn eat(&mut self, c: char) -> bool {
        if self.peek() == Some(c) {
            self.pos += 1;
            return true;
        }
        false
    }

    /// `alternative ('|' alternative)*`
    fn disjunction(&mut self, depth: usize) -> Result<Node, PatternError> {
        let mut branches = vec![self.alternative(depth)?];
        while self.eat('|') {
            branches.push(self.alternative(depth)?);
        }
        if branches.len() == 1 {
            Ok(branches.pop().expect("one branch"))
        } else {
            Ok(Node::Alt(branches))
        }
    }

    fn alternative(&mut self, depth: usize) -> Result<Node, PatternError> {
        let mut items = Vec::new();
        while let Some(c) = self.peek() {
            if c == '|' || c == ')' {
                break;
            }
            let term = self.term(depth)?;
            // A quantifier binds to the term just parsed, not to the whole alternative.
            let term = self.quantifier(term)?;
            items.push(term);
        }
        Ok(Node::Concat(items))
    }

    fn term(&mut self, depth: usize) -> Result<Node, PatternError> {
        let c = self
            .peek()
            .ok_or_else(|| self.error("unexpected end of pattern"))?;
        match c {
            '^' => {
                self.pos += 1;
                Ok(Node::Start)
            }
            '$' => {
                self.pos += 1;
                Ok(Node::End)
            }
            '\\' => self.atom_escape(),
            '.' => {
                self.pos += 1;
                Ok(Node::Any)
            }
            '[' => self.character_class(),
            '(' => self.group(depth),
            '*' | '+' | '?' => Err(self.error("nothing to repeat")),
            ')' => Err(self.error("unmatched ')'")),
            _ => {
                self.pos += 1;
                Ok(Node::Char(self.annex_b_char(c)?))
            }
        }
    }

    /// A character that only the Annex B grammar lets stand alone.
    ///
    /// Outside a character class, `{`, `}` and `]` are ordinary literals in a sloppy
    /// pattern — that is the web-compatibility grammar, and `/a]/` matches `a]`. Under `u`
    /// (or `v`) the grammar is the strict one, where those three must be escaped or be
    /// part of a real construct, so `/a]/u` is a `SyntaxError`. Reporting it is what the
    /// reference does, and a pattern that silently means something different under `u` than
    /// the spec says is worse than one that refuses to compile.
    fn annex_b_char(&self, c: char) -> Result<char, PatternError> {
        if self.unicode && matches!(c, '{' | '}' | ']') {
            return Err(self.error(&format!("lone '{c}' in unicode mode")));
        }
        Ok(c)
    }

    fn quantifier(&mut self, term: Node) -> Result<Node, PatternError> {
        let (min, max) = match self.peek() {
            Some('*') => {
                self.pos += 1;
                (0, None)
            }
            Some('+') => {
                self.pos += 1;
                (1, None)
            }
            Some('?') => {
                self.pos += 1;
                (0, Some(1))
            }
            Some('{') => match self.counted_quantifier()? {
                Some(bounds) => bounds,
                None => return Ok(term),
            },
            _ => return Ok(term),
        };
        // `?` right after a quantifier makes it lazy.
        let greedy = !self.eat('?');
        // An assertion cannot be repeated; the spec calls that a SyntaxError and so does
        // the reference.
        if matches!(term, Node::Start | Node::End | Node::WordBoundary(_)) {
            return Err(self.error("nothing to repeat"));
        }
        Ok(Node::Repeat {
            node: Box::new(term),
            min,
            max,
            greedy,
        })
    }

    /// `{n}`, `{n,}` or `{n,m}`, or `None` when the brace is not a quantifier.
    fn counted_quantifier(&mut self) -> Result<Option<(u32, Option<u32>)>, PatternError> {
        let start = self.pos;
        self.pos += 1;
        let min = self.number()?;
        let Some(min) = min else {
            // Not a quantifier: `{` is a literal `{`.
            self.pos = start;
            return Ok(None);
        };
        if self.eat('}') {
            return Ok(Some((min, Some(min))));
        }
        if !self.eat(',') {
            self.pos = start;
            return Ok(None);
        }
        if self.eat('}') {
            return Ok(Some((min, None)));
        }
        let max = self.number()?;
        let Some(max) = max else {
            self.pos = start;
            return Ok(None);
        };
        if !self.eat('}') {
            self.pos = start;
            return Ok(None);
        }
        if max < min {
            return Err(self.error("numbers out of order in {} quantifier"));
        }
        Ok(Some((min, Some(max))))
    }

    fn number(&mut self) -> Result<Option<u32>, PatternError> {
        let start = self.pos;
        while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
            self.pos += 1;
        }
        if self.pos == start {
            return Ok(None);
        }
        let text: String = self.chars[start..self.pos].iter().collect();
        text.parse().map(Some).map_err(|_| {
            self.pos = start;
            self.error("quantifier is too large")
        })
    }

    fn group(&mut self, depth: usize) -> Result<Node, PatternError> {
        self.pos += 1; // '('
        if self.eat('?') {
            match self.peek() {
                Some(':') => {
                    self.pos += 1;
                    let inner = self.disjunction(depth + 1)?;
                    self.expect_close()?;
                    return Ok(Node::Group {
                        index: None,
                        node: Box::new(inner),
                    });
                }
                Some('=') => {
                    self.pos += 1;
                    let inner = self.disjunction(depth + 1)?;
                    self.expect_close()?;
                    return Ok(Node::Look {
                        node: Box::new(inner),
                        ahead: true,
                        negated: false,
                    });
                }
                Some('!') => {
                    self.pos += 1;
                    let inner = self.disjunction(depth + 1)?;
                    self.expect_close()?;
                    return Ok(Node::Look {
                        node: Box::new(inner),
                        ahead: true,
                        negated: true,
                    });
                }
                Some('<') => {
                    // `(?<=` / `(?<!` are lookbehind, `(?<name>` is a named group; this
                    // engine implements neither, and guessing would be worse than saying
                    // so.
                    self.pos += 1;
                    return Err(self.error("lookbehind and named groups are not supported"));
                }
                _ => return Err(self.error("invalid group")),
            }
        }
        self.groups += 1;
        let index = self.groups;
        let inner = self.disjunction(depth + 1)?;
        self.expect_close()?;
        Ok(Node::Group {
            index: Some(index),
            node: Box::new(inner),
        })
    }

    fn expect_close(&mut self) -> Result<(), PatternError> {
        if self.eat(')') {
            return Ok(());
        }
        // The message the reference's engine reports, so a failing script reads the same.
        Err(self.error("expecting ')'"))
    }

    /// An escape outside a character class.
    fn atom_escape(&mut self) -> Result<Node, PatternError> {
        self.pos += 1; // '\'
        let c = self
            .peek()
            .ok_or_else(|| self.error("\\ at end of pattern"))?;
        match c {
            'b' => {
                self.pos += 1;
                Ok(Node::WordBoundary(true))
            }
            'B' => {
                self.pos += 1;
                Ok(Node::WordBoundary(false))
            }
            // Inside an alternative these are just the characters.
            'd' | 'D' | 'w' | 'W' | 's' | 'S' => {
                self.pos += 1;
                Ok(Node::Class(class_for(c).0, class_for(c).1))
            }
            'p' | 'P' => {
                self.pos += 1;
                self.property_escape(c == 'P')
            }
            '1'..='9' => {
                let start = self.pos;
                while matches!(self.peek(), Some(d) if d.is_ascii_digit()) {
                    self.pos += 1;
                }
                let text: String = self.chars[start..self.pos].iter().collect();
                let index: usize = text.parse().map_err(|_| self.error("bad backreference"))?;
                Ok(Node::Backref(index))
            }
            'c' => {
                // Annex B: `\c` followed by anything that is not an ASCII letter is a
                // literal backslash, and the `c` is read as an ordinary character. Only
                // the `c` is left unconsumed here; `escape_char` would have taken it.
                let letter = self.chars.get(self.pos + 1).copied();
                match letter {
                    Some(letter) if letter.is_ascii_alphabetic() => {
                        self.pos += 2;
                        Ok(Node::Char(
                            char::from_u32((letter as u32) % 32).unwrap_or('\0'),
                        ))
                    }
                    _ => Ok(Node::Char('\\')),
                }
            }
            _ => {
                self.pos += 1;
                Ok(Node::Char(self.escape_char(c)?))
            }
        }
    }

    /// `\p{Name}` or `\P{Name}`, with the `p`/`P` consumed.
    ///
    /// The property escapes are part of the strict (`u`) grammar: without `u`, `\p` is an
    /// identity escape for the letter `p` and the `{…}` that follows is ordinary text. That
    /// is why the flag is checked here rather than the construct being rejected outright.
    fn property_escape(&mut self, negated: bool) -> Result<Node, PatternError> {
        if !self.unicode {
            // Annex B: `\p` is an identity escape for `p`, and the `{…}` that follows is
            // read as ordinary text. The `p` is already consumed.
            return Ok(Node::Char(if negated { 'P' } else { 'p' }));
        }
        if !self.eat('{') {
            return Err(self.error("expecting '{' after \\p"));
        }
        let start = self.pos;
        while matches!(self.peek(), Some(c) if c != '}') {
            self.pos += 1;
        }
        if !self.eat('}') {
            return Err(self.error("expecting '}' after property name"));
        }
        let name: String = self.chars[start..self.pos - 1].iter().collect();
        let kind = super::PropertyKind::from_name(&name)
            .ok_or_else(|| self.error(&format!("unsupported property name '{name}'")))?;
        Ok(Node::Property { kind, negated })
    }

    /// The character a `\x` escape stands for, with `c` already consumed.
    ///
    /// `escape_char` is only reached from inside a character class and from
    /// [`Parser::atom_escape`]'s fallback, which is why `\b` means backspace here: outside a
    /// class it never gets this far, because `atom_escape` reads it as a word boundary
    /// first.
    fn escape_char(&mut self, c: char) -> Result<char, PatternError> {
        Ok(match c {
            'b' => '\u{8}',
            'n' => '\n',
            'r' => '\r',
            't' => '\t',
            'f' => '\u{c}',
            'v' => '\u{b}',
            '0' => '\0',
            'x' => self.hex(2)?,
            'u' => self.hex(4)?,
            'c' => {
                // Inside a class the same Annex B rule applies: a non-letter after `\c`
                // is a literal backslash, and the caller reads the letter itself.
                let Some(letter) = self.peek() else {
                    return Ok('\\');
                };
                if !letter.is_ascii_alphabetic() {
                    return Ok('\\');
                }
                self.pos += 1;
                char::from_u32((letter as u32) % 32).unwrap_or('\0')
            }
            other => other,
        })
    }

    fn hex(&mut self, digits: usize) -> Result<char, PatternError> {
        let start = self.pos;
        let mut value = 0u32;
        for _ in 0..digits {
            let c = self.peek().ok_or_else(|| self.error("incomplete escape"))?;
            let digit = c.to_digit(16).ok_or_else(|| self.error("invalid escape"))?;
            value = value * 16 + digit;
            self.pos += 1;
        }
        char::from_u32(value).ok_or_else(|| {
            self.pos = start;
            self.error("invalid escape")
        })
    }

    fn character_class(&mut self) -> Result<Node, PatternError> {
        self.pos += 1; // '['
        let negated = self.eat('^');
        let mut ranges: Vec<(char, char)> = Vec::new();
        let mut first = true;
        loop {
            let Some(c) = self.peek() else {
                return Err(self.error("unterminated character class"));
            };
            if c == ']' {
                if first {
                    // A `]` in the first position is a literal.
                    self.pos += 1;
                    ranges.push((']', ']'));
                    first = false;
                    continue;
                }
                self.pos += 1;
                break;
            }
            first = false;
            let mut atom = self.class_atom()?;
            // A range needs a single character on both sides: `[\d-z]` is a SyntaxError.
            if let Some((lo, _)) = atom.single()
                && self.peek() == Some('-')
            {
                let save = self.pos;
                self.pos += 1;
                if matches!(self.peek(), Some(']') | None) {
                    // The `-` is the last thing in the class, so it is a literal.
                    self.pos = save;
                } else {
                    let high = self.class_atom()?;
                    match high.single() {
                        Some((hi, _)) if hi >= lo => {
                            atom = ClassAtom::Range(lo, hi);
                        }
                        Some(_) => {
                            return Err(self.error("range out of order in character class"));
                        }
                        None => return Err(self.error("invalid character class range")),
                    }
                }
            }
            atom.push_into(&mut ranges);
        }
        Ok(Node::Class(negated, ranges))
    }

    /// One class atom: a single character, a range, or a whole built-in class.
    fn class_atom(&mut self) -> Result<ClassAtom, PatternError> {
        let c = self
            .peek()
            .ok_or_else(|| self.error("unterminated character class"))?;
        if c != '\\' {
            self.pos += 1;
            return Ok(ClassAtom::Range(c, c));
        }
        self.pos += 1;
        let e = self
            .peek()
            .ok_or_else(|| self.error("\\ at end of pattern"))?;
        match e {
            'd' | 'D' | 'w' | 'W' | 's' | 'S' => {
                self.pos += 1;
                let (negated, ranges) = class_for(e);
                if negated {
                    // `[\D]` means "not a digit", which cannot be expressed as a range
                    // list. Saying so beats matching the wrong set.
                    return Err(self.error("negated class escape inside [...] is not supported"));
                }
                Ok(ClassAtom::Set(ranges))
            }
            // A property escape is a set, not a range, and a class atom can only be one or
            // the other. Refusing beats reading `\p` as the letter `p`, which is what the
            // Annex B fallback below would otherwise do — silently, and with the wrong
            // meaning.
            'p' | 'P' => Err(self.error("property escapes inside [...] are not supported")),
            _ => {
                self.pos += 1;
                let ch = self.escape_char(e)?;
                Ok(ClassAtom::Range(ch, ch))
            }
        }
    }
}

/// What one atom of a character class contributed.
enum ClassAtom {
    /// A single character (which a following `-` may turn into a range).
    Range(char, char),
    /// Several ranges, from a `\d`-style escape.
    Set(Vec<(char, char)>),
}

impl ClassAtom {
    /// The bounds, when this atom is exactly one character.
    fn single(&self) -> Option<(char, char)> {
        match self {
            ClassAtom::Range(lo, hi) if lo == hi => Some((*lo, *hi)),
            _ => None,
        }
    }

    fn push_into(&self, out: &mut Vec<(char, char)>) {
        match self {
            ClassAtom::Range(lo, hi) => out.push((*lo, *hi)),
            ClassAtom::Set(ranges) => out.extend_from_slice(ranges),
        }
    }
}

/// The built-in character class a `\d`-style escape stands for.
fn class_for(c: char) -> (bool, Vec<(char, char)>) {
    match c {
        'd' => (false, vec![('0', '9')]),
        'D' => (true, vec![('0', '9')]),
        'w' => (false, vec![('0', '9'), ('A', 'Z'), ('_', '_'), ('a', 'z')]),
        'W' => (true, vec![('0', '9'), ('A', 'Z'), ('_', '_'), ('a', 'z')]),
        's' => (
            false,
            vec![
                ('\t', '\t'),
                ('\n', '\n'),
                ('\u{b}', '\u{c}'),
                ('\r', '\r'),
                (' ', ' '),
                ('\u{a0}', '\u{a0}'),
                ('\u{1680}', '\u{1680}'),
                ('\u{2000}', '\u{200a}'),
                ('\u{2028}', '\u{2029}'),
                ('\u{202f}', '\u{202f}'),
                ('\u{205f}', '\u{205f}'),
                ('\u{3000}', '\u{3000}'),
                ('\u{feff}', '\u{feff}'),
            ],
        ),
        _ => (
            true,
            vec![
                ('\t', '\t'),
                ('\n', '\n'),
                ('\u{b}', '\u{c}'),
                ('\r', '\r'),
                (' ', ' '),
                ('\u{a0}', '\u{a0}'),
                ('\u{1680}', '\u{1680}'),
                ('\u{2000}', '\u{200a}'),
                ('\u{2028}', '\u{2029}'),
                ('\u{202f}', '\u{202f}'),
                ('\u{205f}', '\u{205f}'),
                ('\u{3000}', '\u{3000}'),
                ('\u{feff}', '\u{feff}'),
            ],
        ),
    }
}
