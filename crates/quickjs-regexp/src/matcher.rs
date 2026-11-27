//! A backtracking matcher over the tree [`parser`](super::parser) produces.
//!
//! The matcher is written in continuation-passing style: `seq` walks a list of nodes and
//! calls `k` with the position it reached, and `k` returns `false` to ask for another
//! possibility. That is what makes greedy quantifiers work — a repeat tries one more
//! iteration first and falls back to the continuation — and it is why captures are
//! passed by shared reference and copied when a group is entered, rather than mutated in
//! place: a branch that fails must not leave its captures behind.

use super::{ClassSet, Flags, Node, PropertyKind, set_has_strings};

/// Capture register: `caps[0]` is the whole match, `caps[n]` is group `n`.
pub type Captures = Vec<Option<(usize, usize)>>;

/// One successful match.
pub struct Match {
    /// Byte-independent character offsets; the caller turns them into string slices.
    pub start: usize,
    /// Exclusive end of the match.
    pub end: usize,
    /// Group captures, indexed like [`Captures`].
    pub captures: Captures,
}

/// Where a match may begin.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Anchoring {
    /// Try every starting position, as `exec` does.
    Search,
    /// Only at the given position, as a sticky (`y`) or global-scan step does.
    At(usize),
}

/// Try to match `node` against `input`, returning the leftmost match.
pub fn exec(
    node: &Node,
    groups: usize,
    flags: Flags,
    input: &[u32],
    from: usize,
    anchoring: Anchoring,
) -> Option<Match> {
    let matcher = Matcher {
        input,
        flags,
        budget: std::cell::Cell::new(input.len().saturating_mul(BUDGET_PER_CHAR) + 4096),
    };
    let starts: Box<dyn Iterator<Item = usize>> = match anchoring {
        Anchoring::At(position) => Box::new(std::iter::once(position)),
        Anchoring::Search => Box::new(from..=input.len()),
    };
    for start in starts {
        if start > input.len() {
            break;
        }
        let caps: Captures = vec![None; groups + 1];
        let mut found = None;
        let mut record = |end: usize, caps: &Captures| {
            let mut caps = caps.clone();
            caps[0] = Some((start, end));
            found = Some(Match {
                start,
                end,
                captures: caps,
            });
            // `true` stops the search: the first end position reached for this start is
            // the match, because alternatives and greedy repeats are tried in order.
            true
        };
        if matcher.seq(std::slice::from_ref(node), start, &caps, &mut record) {
            return found;
        }
    }
    None
}

struct Matcher<'a> {
    input: &'a [u32],
    flags: Flags,
    /// How deep the backtracking recursion may go.
    ///
    /// A backtracking matcher recurses once per repetition, so `/^\p{ASCII}+$/u` over a long
    /// string is a stack overflow rather than a wrong answer — the harness builds exactly
    /// such a string out of every ASCII code point. Rather than guess a depth that is both
    /// safe and sufficient, the budget is spent per input byte: an unbounded repeat can
    /// only recurse as deep as the input is long, so this bounds the recursion by the data
    /// instead of by a constant.
    ///
    /// Running out of budget answers "no match" rather than aborting. That is a wrong answer
    /// on a pattern that needed more, but it is a *contained* wrong answer: the alternative
    /// is a process that dies mid-test-suite.
    budget: std::cell::Cell<usize>,
}

/// How much backtracking recursion one input character may cost.
///
/// Sized so that the deepest legitimate pattern still fits: a nested quantifier recurses a
/// few times per character, and the threads the suite runs already have multi-megabyte
/// stacks. This is a guard rail, not the primary limit.
const BUDGET_PER_CHAR: usize = 256;

impl Matcher<'_> {
    fn at(&self, position: usize) -> Option<u32> {
        self.input.get(position).copied()
    }

    /// The simple lowercase mapping of a code point.
    ///
    /// A code point that is not a `char` — an unpaired surrogate — has no case mapping, so
    /// it folds to itself rather than to U+FFFD.
    fn fold(&self, cp: u32) -> u32 {
        if !self.flags.ignore_case {
            return cp;
        }
        // Full Unicode case folding needs the case tables; the simple mapping covers
        // the ASCII and Latin-1 ranges scripts actually use.
        match char::from_u32(cp) {
            Some(c) => c.to_lowercase().next().unwrap_or(c) as u32,
            None => cp,
        }
    }

    fn chars_equal(&self, a: u32, b: u32) -> bool {
        if self.flags.ignore_case {
            self.fold(a) == self.fold(b)
        } else {
            a == b
        }
    }

    /// How many characters a set can consume here, longest first.
    ///
    /// A set of single characters answers `0` or `1`; `\q{BC|A}` answers `2` or `1`. Returning
    /// the lengths rather than the strings is what makes intersection and subtraction
    /// expressible: two sets agree on a string exactly when they agree on a length at the
    /// same position, because the string is the input either way.
    fn set_lengths(&self, set: &ClassSet, position: usize) -> Vec<usize> {
        let mut lengths = self.set_lengths_unsorted(set, position);
        lengths.sort_unstable_by(|a, b| b.cmp(a));
        lengths.dedup();
        lengths
    }

    fn set_lengths_unsorted(&self, set: &ClassSet, position: usize) -> Vec<usize> {
        match set {
            ClassSet::Char(expected) => match self.at(position) {
                Some(actual) if self.chars_equal(actual, *expected) => vec![1],
                _ => Vec::new(),
            },
            ClassSet::Range(lo, hi) => match self.at(position) {
                Some(c) if self.in_range(c, *lo, *hi) => vec![1],
                _ => Vec::new(),
            },
            ClassSet::Property { kind, negated } => match self.at(position) {
                Some(c) if self.property_holds(*kind, *negated, c) => vec![1],
                _ => Vec::new(),
            },
            ClassSet::Predefined { negated, ranges } => match self.at(position) {
                Some(c) => {
                    let inside = ranges.iter().any(|(lo, hi)| self.in_range(c, *lo, *hi));
                    if inside != *negated {
                        vec![1]
                    } else {
                        Vec::new()
                    }
                }
                None => Vec::new(),
            },
            ClassSet::Strings(members) => members
                .iter()
                .filter(|member| self.matches_sequence(member, position))
                .map(|member| member.len())
                .collect(),
            ClassSet::Union(operands) => operands
                .iter()
                .flat_map(|operand| self.set_lengths_unsorted(operand, position))
                .collect(),
            ClassSet::Intersection(operands) => {
                let mut iter = operands.iter();
                let Some(first) = iter.next() else {
                    return Vec::new();
                };
                let mut lengths = self.set_lengths(first, position);
                for operand in iter {
                    let other = self.set_lengths(operand, position);
                    lengths.retain(|length| other.contains(length));
                }
                lengths
            }
            ClassSet::Difference(left, right) => {
                let mut lengths = self.set_lengths(left, position);
                let excluded = self.set_lengths(right, position);
                lengths.retain(|length| !excluded.contains(length));
                lengths
            }
            // A negated set is a complement of *characters*: it consumes exactly one, and
            // only when the inner set does not accept that one character.
            ClassSet::Negated(inner) => match self.at(position) {
                Some(c) if !self.set_lengths(inner, position).contains(&1) => {
                    let _ = c;
                    vec![1]
                }
                _ => Vec::new(),
            },
        }
    }

    /// Whether `member` matches the input starting at `position`.
    fn matches_sequence(&self, member: &[u32], position: usize) -> bool {
        member
            .iter()
            .enumerate()
            .all(|(index, expected)| match self.at(position + index) {
                Some(actual) => self.chars_equal(actual, *expected),
                None => false,
            })
    }

    /// Whether `c` lies in a range, honouring case-insensitivity.
    fn in_range(&self, c: u32, lo: u32, hi: u32) -> bool {
        self.in_class(c, &[(lo, hi)])
    }

    /// Whether `c` has a property, honouring `\P` and case-insensitivity.
    ///
    /// `u` and `v` disagree about *where* the negation goes under `i`, and the difference is
    /// observable: `\P{Lower}/ui` matches `a` (the negation is applied per case variant, and
    /// `a` canonicalises to the non-lowercase `A`), while `\P{Lower}/vi` does not (the set is
    /// case-closed first and only then complemented, so `a` stays inside). The test suite
    /// pins both, so the dialect decides.
    fn property_holds(&self, kind: PropertyKind, negated: bool, c: u32) -> bool {
        let mut candidates = vec![c];
        if self.flags.ignore_case {
            candidates.push(self.fold(c));
            if let Some(ch) = char::from_u32(c) {
                if let Some(upper) = ch.to_uppercase().next() {
                    candidates.push(upper as u32);
                }
                if let Some(lower) = ch.to_lowercase().next() {
                    candidates.push(lower as u32);
                }
            }
        }
        if self.flags.unicode_sets {
            // `v`: close over case, then negate.
            let holds = candidates
                .into_iter()
                .any(|candidate| kind.holds(candidate));
            holds != negated
        } else {
            // `u`: negate each variant, then ask whether any answers yes.
            candidates
                .into_iter()
                .any(|candidate| kind.holds(candidate) != negated)
        }
    }

    /// Whether `c` is in a character class's ranges.
    ///
    /// Case-insensitive matching has to fold *both* sides of the comparison, which
    /// `chars_equal` does for literals. A range is not a
    /// single character, so it is folded at its bounds: `[A-Z]` folds to `a`..`z` and
    /// therefore matches `a`. The folded form is only used while it is still ordered —
    /// `[A-_]` has a non-letter upper bound, and folding `A` to `a` would put the bound
    /// *below* `_`, so the unfolded comparison stays authoritative for that range.
    fn in_class(&self, c: u32, ranges: &[(u32, u32)]) -> bool {
        let folded = self.fold(c);
        ranges.iter().any(|(lo, hi)| {
            (*lo..=*hi).contains(&c) || (*lo..=*hi).contains(&folded) || {
                let (folded_lo, folded_hi) = (self.fold(*lo), self.fold(*hi));
                folded_lo <= folded_hi && (folded_lo..=folded_hi).contains(&folded)
            }
        })
    }

    /// Match a sequence of nodes, calling `k` at each position the whole sequence can end.
    fn seq(
        &self,
        nodes: &[Node],
        position: usize,
        caps: &Captures,
        k: &mut dyn FnMut(usize, &Captures) -> bool,
    ) -> bool {
        let Some((first, rest)) = nodes.split_first() else {
            return k(position, caps);
        };
        self.node(first, position, caps, &mut |next, caps| {
            self.seq(rest, next, caps, k)
        })
    }

    /// Match one node, calling `k` for each position it can end at.
    fn node(
        &self,
        node: &Node,
        position: usize,
        caps: &Captures,
        k: &mut dyn FnMut(usize, &Captures) -> bool,
    ) -> bool {
        match node {
            Node::Empty => k(position, caps),
            Node::Char(expected) => match self.at(position) {
                Some(actual) if self.chars_equal(actual, *expected) => k(position + 1, caps),
                _ => false,
            },
            Node::Any => match self.at(position) {
                Some(c) if self.flags.dot_all || c != LINE_FEED => k(position + 1, caps),
                _ => false,
            },
            Node::Class(negated, ranges) => match self.at(position) {
                Some(c) if self.in_class(c, ranges) != *negated => k(position + 1, caps),
                _ => false,
            },
            // A property escape outside a class. `property_holds` decides where the negation
            // goes for the dialect in force, which is the one place `u` and `v` disagree.
            Node::Property { kind, negated } => match self.at(position) {
                Some(c) if self.property_holds(*kind, *negated, c) => k(position + 1, caps),
                _ => false,
            },
            // A `v`-mode set may consume one character or several (`\q{BC}`), so matching it
            // yields a *set of lengths* rather than a single one. The continuation is tried
            // longest-first, which is the greedy reading every engine gives an alternation
            // of literals.
            Node::Set(set) => {
                for length in self.set_lengths(set, position) {
                    if k(position + length, caps) {
                        return true;
                    }
                }
                false
            }
            Node::Start => {
                let at_start =
                    position == 0 || (self.flags.multiline && self.after_newline(position));
                if at_start { k(position, caps) } else { false }
            }
            Node::End => {
                let at_end = position == self.input.len()
                    || (self.flags.multiline && self.at_newline(position));
                if at_end { k(position, caps) } else { false }
            }
            Node::WordBoundary(want) => {
                let before = position
                    .checked_sub(1)
                    .and_then(|p| self.at(p))
                    .is_some_and(is_word);
                let after = self.at(position).is_some_and(is_word);
                if (before != after) == *want {
                    k(position, caps)
                } else {
                    false
                }
            }
            Node::Group { index, node } => match index {
                // A non-capturing group only scopes its contents.
                None => self.node(node, position, caps, k),
                Some(index) => {
                    // The capture is committed only if the continuation accepts, so a
                    // branch that later fails leaves the register untouched.
                    self.node(node, position, caps, &mut |end, inner| {
                        let mut inner = inner.clone();
                        if let Some(slot) = inner.get_mut(*index) {
                            *slot = Some((position, end));
                        }
                        k(end, &inner)
                    })
                }
            },
            Node::Concat(nodes) => self.seq(nodes, position, caps, k),
            Node::Alt(branches) => {
                for branch in branches {
                    if self.node(branch, position, caps, k) {
                        return true;
                    }
                }
                false
            }
            Node::Repeat {
                node,
                min,
                max,
                greedy,
            } => self.repeat(node, *min, *max, *greedy, 0, position, caps, k),
            Node::Backref(index) => {
                let Some(Some((start, end))) = caps.get(*index) else {
                    // A backreference to a group that did not participate matches the
                    // empty string, which is what the spec says.
                    return k(position, caps);
                };
                let (start, end) = (*start, *end);
                if end - start > self.input.len() - position {
                    return false;
                }
                let ok = (0..end - start).all(|offset| {
                    self.chars_equal(self.input[start + offset], self.input[position + offset])
                });
                if ok {
                    k(position + (end - start), caps)
                } else {
                    false
                }
            }
            Node::Look {
                node,
                ahead,
                negated,
            } => {
                if !*ahead {
                    // Lookbehind is rejected by the parser, so this arm is unreachable.
                    return false;
                }
                // A lookahead matches the empty string: `position` does not move, and the
                // captures it makes are kept when it succeeds.
                let mut captured = None;
                let found = self.node(node, position, caps, &mut |_, inner| {
                    captured = Some(inner.clone());
                    true
                });
                match (found, negated) {
                    (true, false) => {
                        let inner = captured.unwrap_or_else(|| caps.clone());
                        k(position, &inner)
                    }
                    (true, true) => false,
                    (false, _) => {
                        if *negated {
                            k(position, caps)
                        } else {
                            false
                        }
                    }
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn repeat(
        &self,
        node: &Node,
        min: u32,
        max: Option<u32>,
        greedy: bool,
        count: u32,
        position: usize,
        caps: &Captures,
        k: &mut dyn FnMut(usize, &Captures) -> bool,
    ) -> bool {
        // The fast path: a single-character body with nothing to capture.
        //
        // A backtracking matcher recurses once per repetition, so `/^\P{ASCII}+$/u` over the
        // ~1.1 M code points the conformance harness concatenates would be a million native
        // frames. There is no backtracking decision to make in this shape — a one-character
        // body either matches at a position or does not — so it is consumed by a loop, and
        // only the *final* position is handed to the continuation after the minimum is met.
        //
        // The greedy answer has to be tried first (the longest run), then shorter ones from
        // the end backwards, so the continuation sees the same choices recursion would give
        // it. Lazy walks forwards from the minimum instead.
        if count == 0
            && inner_groups(node).is_none()
            && match node {
                Node::Char(_) | Node::Any | Node::Class(..) | Node::Property { .. } => true,
                Node::Set(set) => !set_has_strings(set),
                _ => false,
            }
        {
            // How many single characters this body matches starting at `position`.
            let mut end = position;
            // `end` is exclusive, so the run may grow while it is still shorter than `max`.
            while max.is_none_or(|max| ((end - position) as u32) < max) && end < self.input.len() {
                if !self.single_char_matches(node, end) {
                    break;
                }
                end += 1;
            }
            let available = (end - position) as u32;
            if available < min {
                return false;
            }
            let mut count = |taken: u32| -> bool {
                let at = position + taken as usize;
                k(at, caps)
            };
            if greedy {
                for taken in (min..=available).rev() {
                    if count(taken) {
                        return true;
                    }
                }
                return false;
            }
            for taken in min..=available {
                if count(taken) {
                    return true;
                }
            }
            return false;
        }
        // Spend one unit of recursion budget per iteration. Exhausting it stops the repeat
        // rather than overflowing the native stack.
        let budget = self.budget.get();
        if budget == 0 {
            return false;
        }
        self.budget.set(budget - 1);
        let may_iterate = max.is_none_or(|max| count < max);
        // Greedy tries one more iteration first; lazy tries the continuation first.
        let try_iterate = |k: &mut dyn FnMut(usize, &Captures) -> bool| {
            if !may_iterate {
                return false;
            }
            // Every iteration starts from a clean slate for the groups inside it. Without
            // this, a group that matched in an earlier iteration survives into one where it
            // did not participate: `/(z)((a+)?(b+)?(c))*/.exec("zaacbbbcac")` left group 4
            // as `bbb` from iteration two instead of resetting it to `undefined` in the
            // final `ac`.
            let mut entry = caps.clone();
            if let Some((low, high)) = inner_groups(node) {
                for slot in entry.iter_mut().take(high + 1).skip(low) {
                    *slot = None;
                }
            }
            let caps = &entry;
            self.node(node, position, caps, &mut |end, inner| {
                // A repeat that can match the empty string would loop forever, so an
                // iteration that consumed nothing is not taken unless the minimum still
                // has to be met.
                if end == position && count >= min {
                    return false;
                }
                self.repeat(node, min, max, greedy, count + 1, end, inner, k)
            })
        };
        if greedy {
            if try_iterate(k) {
                return true;
            }
            if count >= min {
                k(position, caps)
            } else {
                false
            }
        } else {
            if count >= min && k(position, caps) {
                return true;
            }
            try_iterate(k)
        }
    }

    /// Whether a one-character node matches at `at`.
    ///
    /// Only the nodes the repeat fast path accepts are handled; the caller has already
    /// narrowed the type, so anything else is a programming error rather than a pattern.
    fn single_char_matches(&self, node: &Node, at: usize) -> bool {
        match node {
            Node::Char(c) => self.at(at).is_some_and(|input| self.chars_equal(*c, input)),
            Node::Any => match self.at(at) {
                Some(c) => self.flags.dot_all || !is_line_terminator(c),
                None => false,
            },
            Node::Class(negated, ranges) => match self.at(at) {
                Some(c) => self.in_class(c, ranges) != *negated,
                None => false,
            },
            // A `v`-mode set can contain multi-character members, so it only belongs on this
            // path when every member is one character.
            // Only a set whose members are all one code point belongs on the fast path. A
            // `v`-mode set can hold multi-character members from `\q{…}`, and consuming one
            // character of a longer member would leave the repetition misaligned.
            Node::Set(set) => {
                !set_has_strings(set)
                    && self
                        .at(at)
                        .is_some_and(|_| self.set_lengths(set, at).first() == Some(&1))
            }
            Node::Property { kind, negated } => match self.at(at) {
                Some(c) => self.property_holds(*kind, *negated, c),
                None => false,
            },
            _ => false,
        }
    }

    fn after_newline(&self, position: usize) -> bool {
        position > 0 && self.at(position - 1) == Some(LINE_FEED)
    }

    fn at_newline(&self, position: usize) -> bool {
        self.at(position) == Some(LINE_FEED)
    }
}

/// The range of capture indices a node can fill, if it contains any group.
fn inner_groups(node: &Node) -> Option<(usize, usize)> {
    let mut low = usize::MAX;
    let mut high = 0usize;
    fn walk(node: &Node, low: &mut usize, high: &mut usize) {
        match node {
            Node::Group { index, node } => {
                if let Some(index) = index {
                    *low = (*low).min(*index);
                    *high = (*high).max(*index);
                }
                walk(node, low, high);
            }
            Node::Concat(nodes) | Node::Alt(nodes) => {
                for node in nodes {
                    walk(node, low, high);
                }
            }
            Node::Repeat { node, .. } | Node::Look { node, .. } => walk(node, low, high),
            _ => {}
        }
    }
    walk(node, &mut low, &mut high);
    (low != usize::MAX).then_some((low, high))
}

/// Whether `c` is a word character, as `\b` and `\w` define it.
/// U+000A, the code point `\n` spells.
const LINE_FEED: u32 = 0x0A;

/// The four line terminators `.` does not match without the `s` flag.
fn is_line_terminator(cp: u32) -> bool {
    matches!(cp, 0x0A | 0x0D | 0x2028 | 0x2029)
}

fn is_word(cp: u32) -> bool {
    match char::from_u32(cp) {
        Some(c) => c.is_alphanumeric() || c == '_',
        // A surrogate is a word character: `\w` is `[\p{Alphabetic}\p{M}\p{Nd}\p{Pc}\u200C\u200D]`,
        // and the reference treats a lone surrogate as belonging to no category, which for
        // `\b` means "not a word boundary" on both sides.
        None => false,
    }
}
