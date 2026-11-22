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

use super::{ClassSet, Node, PropertyKind};

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
    /// Whether the `u` or `v` flag is set, which turns several Annex B liberties into
    /// syntax errors. See [`Parser::annex_b_char`].
    unicode: bool,
    /// Whether the `v` flag is set, which additionally enables the character-class set
    /// algebra. Implies `unicode`.
    sets: bool,
}

/// Parse `source` with `flags`.
pub fn parse(source: &str, flags: crate::regexp::Flags) -> Result<Parsed, PatternError> {
    // `u` is the one flag the *syntax* depends on: it decides whether the Annex B
    // liberties below are allowed at all.
    let mut parser = Parser {
        chars: source.chars().collect(),
        pos: 0,
        groups: 0,
        unicode: flags.unicode || flags.unicode_sets,
        sets: flags.unicode_sets,
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
            // `v` swaps in the set algebra; every other dialect keeps the flat form.
            '[' if self.sets => self.set_class(),
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
            // Annex B: an incomplete `\x`/`\u` is an *identity* escape for the letter, so
            // `/\u{2}/` is `u` repeated twice while `/\u{2}/u` is the code point 2. The
            // strict grammars keep the error.
            'x' => match self.try_hex(2) {
                Some(value) => value,
                None if !self.unicode => 'x',
                None => return Err(self.error("invalid escape")),
            },
            // `\u{…}` spells a whole code point; `\uXXXX` spells one code unit. The brace
            // form exists only in the strict grammars, which is why plain `/\u{2}/` is the
            // letter `u` repeated — the Annex B reading, and what the flag decides.
            'u' if self.unicode && self.peek() == Some('{') => {
                self.pos += 1; // the '{'
                let start = self.pos;
                while matches!(self.peek(), Some(c) if c != '}') {
                    self.pos += 1;
                }
                let digits: String = self.chars[start..self.pos].iter().collect();
                if !self.eat('}') {
                    return Err(self.error("expecting '}' after \\u{"));
                }
                if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_hexdigit()) {
                    return Err(self.error("invalid code point escape"));
                }
                let code = u32::from_str_radix(&digits, 16)
                    .map_err(|_| self.error("invalid code point escape"))?;
                if code > 0x10FFFF {
                    return Err(self.error("code point out of range"));
                }
                // A surrogate is not a code point, so the strict grammar refuses it outright.
                if (0xD800..0xE000).contains(&code) {
                    return Err(self.error("code point escape names a surrogate"));
                }
                char::from_u32(code).ok_or_else(|| self.error("invalid code point escape"))?
            }
            'u' => match self.try_hex(4) {
                Some(value) => value,
                None if !self.unicode => 'u',
                None => return Err(self.error("invalid escape")),
            },
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

    /// Exactly `digits` hex digits, or `None` with the position restored.
    ///
    /// Restoring matters: on a failure the caller falls back to reading the letter as an
    /// identity escape, and a half-consumed `\u12` would then leave the `12` behind.
    fn try_hex(&mut self, digits: usize) -> Option<char> {
        let start = self.pos;
        let mut value = 0u32;
        for _ in 0..digits {
            match self.peek().and_then(|c| c.to_digit(16)) {
                Some(digit) => {
                    value = value * 16 + digit;
                    self.pos += 1;
                }
                None => {
                    self.pos = start;
                    return None;
                }
            }
        }
        match char::from_u32(value) {
            Some(character) => Some(character),
            None => {
                self.pos = start;
                None
            }
        }
    }

    /// A `v`-mode character class: `[...]` with the set algebra.
    ///
    /// The grammar is a list of operands joined either by juxtaposition (union) or by `&&`
    /// / `--`. Mixing the two operators at one level is a `SyntaxError`, which is why the
    /// operator is remembered and checked rather than folded left to right.
    fn set_class(&mut self) -> Result<Node, PatternError> {
        self.pos += 1; // '['
        let negated = self.eat('^');
        let mut operands: Vec<ClassSet> = Vec::new();
        let mut operator: Option<u8> = None;
        loop {
            let Some(c) = self.peek() else {
                return Err(self.error("unterminated character class"));
            };
            if c == ']' {
                self.pos += 1;
                break;
            }
            operands.push(self.set_operand()?);
            // `&&` and `--` are the only operators; anything else continues the union.
            let next_op = if self.starts_with("&&") {
                self.pos += 2;
                Some(b'&')
            } else if self.starts_with("--") {
                self.pos += 2;
                Some(b'-')
            } else {
                None
            };
            if let (Some(seen), Some(found)) = (operator, next_op)
                && seen != found
            {
                return Err(self.error("cannot mix '&&' and '--' in one character class"));
            }
            if next_op.is_some() {
                operator = next_op;
                // An operator needs a right-hand operand.
                if matches!(self.peek(), Some(']') | None) {
                    return Err(self.error("missing operand after set operator"));
                }
            }
        }
        let set = match operator {
            None => ClassSet::Union(operands),
            Some(b'&') => ClassSet::Intersection(operands),
            Some(_) => {
                // Subtraction is left-associative: `a -- b -- c` is `(a -- b) -- c`.
                let mut iter = operands.into_iter();
                let Some(first) = iter.next() else {
                    return Err(self.error("empty character class"));
                };
                iter.fold(first, |left, right| {
                    ClassSet::Difference(Box::new(left), Box::new(right))
                })
            }
        };
        let set = if negated {
            // A negated set is a complement of characters, so a string member has no
            // meaning in it; refusing beats silently matching one character of a pair.
            if set_has_strings(&set) {
                return Err(self.error(
                    "negated character class may not contain strings longer than one character",
                ));
            }
            ClassSet::Negated(Box::new(set))
        } else {
            set
        };
        Ok(Node::Set(Box::new(set)))
    }

    /// One operand of a `v`-mode class: a nested class, an escape, a `\q{…}` set, or a
    /// character (possibly the start of a range).
    fn set_operand(&mut self) -> Result<ClassSet, PatternError> {
        let c = self
            .peek()
            .ok_or_else(|| self.error("unterminated character class"))?;
        if c == '[' {
            // A nested class unions into its parent unless it is negated, in which case it
            // is a complement of its own.
            return self.set_class().map(|node| match node {
                Node::Set(set) => *set,
                // `set_class` only ever builds a `Set`, so the alternative is unreachable;
                // treating it as an empty union keeps the function total.
                _ => ClassSet::Union(Vec::new()),
            });
        }
        if c == '\\' {
            return self.set_escape();
        }
        // A single character may open a range. `-` itself has to be escaped in `v` mode,
        // which is what makes `--` unambiguous.
        self.pos += 1;
        if c == '-' {
            return Err(self.error("a literal '-' must be escaped inside a character class"));
        }
        self.set_range_from(c)
    }

    /// Continue a `v`-mode operand after its first character, turning it into a range when
    /// a single `-` follows.
    fn set_range_from(&mut self, lo: char) -> Result<ClassSet, PatternError> {
        // `-` starts a range only when it is a single dash: `--` is the subtraction
        // operator and belongs to the caller.
        if self.peek() == Some('-') && !self.starts_with("--") {
            self.pos += 1;
            let hi = match self.peek() {
                Some(']') | None => return Err(self.error("unterminated range")),
                Some('\\') => {
                    self.pos += 1;
                    let escape = self
                        .peek()
                        .ok_or_else(|| self.error("\\ at end of pattern"))?;
                    self.pos += 1;
                    self.escape_char(escape)?
                }
                Some(character) => {
                    self.pos += 1;
                    character
                }
            };
            if hi < lo {
                return Err(self.error("range out of order in character class"));
            }
            return Ok(ClassSet::Range(lo, hi));
        }
        Ok(ClassSet::Char(lo))
    }

    /// An escape inside a `v`-mode class.
    fn set_escape(&mut self) -> Result<ClassSet, PatternError> {
        self.pos += 1; // '\\'
        let c = self
            .peek()
            .ok_or_else(|| self.error("\\ at end of pattern"))?;
        match c {
            'p' | 'P' => {
                self.pos += 1;
                let node = self.property_escape(c == 'P')?;
                Ok(match node {
                    Node::Property { kind, negated } => ClassSet::Property { kind, negated },
                    Node::Char(character) => ClassSet::Char(character),
                    _ => return Err(self.error("invalid property escape")),
                })
            }
            'q' => {
                self.pos += 1;
                self.string_disjunction()
            }
            'd' | 'D' | 'w' | 'W' | 's' | 'S' => {
                self.pos += 1;
                let (negated, ranges) = class_for(c);
                Ok(ClassSet::Predefined { negated, ranges })
            }
            _ => {
                self.pos += 1;
                let character = self.escape_char(c)?;
                // An escaped character may still open a range: `[\--0]` is a range from
                // `-` to `0`.
                self.set_range_from(character)
            }
        }
    }

    /// `\q{a|bc}`: a set of strings, each of which may be several characters long.
    fn string_disjunction(&mut self) -> Result<ClassSet, PatternError> {
        if !self.eat('{') {
            return Err(self.error("expecting '{' after \\q"));
        }
        let mut members: Vec<Vec<char>> = Vec::new();
        loop {
            let mut member: Vec<char> = Vec::new();
            loop {
                match self.peek() {
                    None => return Err(self.error("unterminated \\q{…}")),
                    Some('|') | Some('}') => break,
                    Some('\\') => {
                        self.pos += 1;
                        let escape = self
                            .peek()
                            .ok_or_else(|| self.error("\\ at end of pattern"))?;
                        self.pos += 1;
                        member.push(self.escape_char(escape)?);
                    }
                    Some(character) => {
                        self.pos += 1;
                        member.push(character);
                    }
                }
            }
            if member.is_empty() {
                return Err(self.error("empty string in \\q{…}"));
            }
            members.push(member);
            if self.eat('|') {
                continue;
            }
            if self.eat('}') {
                break;
            }
            return Err(self.error("expecting '}' in \\q{…}"));
        }
        Ok(ClassSet::Strings(members))
    }

    fn starts_with(&self, text: &str) -> bool {
        let mut at = self.pos;
        for expected in text.chars() {
            match self.chars.get(at) {
                Some(actual) if *actual == expected => at += 1,
                _ => return false,
            }
        }
        true
    }

    fn character_class(&mut self) -> Result<Node, PatternError> {
        self.pos += 1; // '['
        let negated = self.eat('^');
        let mut ranges: Vec<(char, char)> = Vec::new();
        // A property escape cannot be written as a range list, so its presence switches the
        // whole class over to the set representation. Everything else stays in `ranges`,
        // which is the cheaper form.
        let mut properties: Vec<ClassSet> = Vec::new();
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
            if let ClassAtom::Property { .. } = atom {
                properties.push(atom.as_set());
            } else {
                atom.push_into(&mut ranges);
            }
        }
        if properties.is_empty() {
            return Ok(Node::Class(negated, ranges));
        }
        let mut members: Vec<ClassSet> = ranges
            .into_iter()
            .map(|(lo, hi)| {
                if lo == hi {
                    ClassSet::Char(lo)
                } else {
                    ClassSet::Range(lo, hi)
                }
            })
            .collect();
        members.extend(properties);
        let set = ClassSet::Union(members);
        Ok(Node::Set(Box::new(if negated {
            ClassSet::Negated(Box::new(set))
        } else {
            set
        })))
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
            // A property escape is a set, not a range; the caller collects these separately
            // and builds a `ClassSet`. Outside the strict grammars `\p` is the letter `p`,
            // which the Annex B fallback below gives.
            'p' | 'P' if self.unicode => {
                self.pos += 1;
                let node = self.property_escape(e == 'P')?;
                match node {
                    Node::Property { kind, negated } => Ok(ClassAtom::Property { kind, negated }),
                    Node::Char(character) => Ok(ClassAtom::Range(character, character)),
                    _ => Err(self.error("invalid property escape")),
                }
            }
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
    /// A Unicode property escape, which only the strict grammars allow inside a class.
    Property {
        /// Which property.
        kind: PropertyKind,
        /// `\P{…}` rather than `\p{…}`.
        negated: bool,
    },
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
            // A property is not a range, so it never lands in the range list; the caller
            // notices it separately and builds a [`ClassSet`] instead.
            ClassAtom::Property { .. } => {}
        }
    }

    /// This atom as a set member.
    fn as_set(&self) -> ClassSet {
        match self {
            ClassAtom::Range(lo, hi) if lo == hi => ClassSet::Char(*lo),
            ClassAtom::Range(lo, hi) => ClassSet::Range(*lo, *hi),
            ClassAtom::Set(ranges) => ClassSet::Union(
                ranges
                    .iter()
                    .map(|(lo, hi)| ClassSet::Range(*lo, *hi))
                    .collect(),
            ),
            ClassAtom::Property { kind, negated } => ClassSet::Property {
                kind: *kind,
                negated: *negated,
            },
        }
    }
}

/// The built-in character class a `\d`-style escape stands for.
/// Whether a set contains a member longer than one character.
///
/// A negated set is a complement of characters, so such a member cannot be expressed and the
/// grammar rejects the pattern instead of matching one character of a longer string.
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
