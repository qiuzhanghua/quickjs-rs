//! Arbitrary-precision integers, the engine's `BigInt`.
//!
//! A `BigInt` is a sign and a magnitude in base 2^32, little-endian, with no trailing zero
//! words — so zero is the empty magnitude and every value has exactly one representation.
//! That last part is what lets `SameValueZero` compare two BigInts with `==` and what lets a
//! `Map` use one as a key.
//!
//! # Two's complement is the hard part
//!
//! `&`, `|`, `^` and `~` are defined on the *infinite* two's-complement form of the operand,
//! which is why `-1n & 3n` is `3n` and `~0n` is `-1n`. The implementation converts to a
//! fixed width of `max(len(a), len(b)) + 1` words, does the operation there, and converts
//! back; the extra word carries the sign extension so the result's sign is recoverable.
//!
//! `>>` is arithmetic: `-1n >> 1n` is `-1n`, not `0n`. The shift is defined as
//! `floor(a / 2^n)`, which for a negative value is *not* a right shift of the magnitude —
//! that rounds toward zero. The implementation does the magnitude shift and then corrects by
//! subtracting one when any shifted-out bit was set.
//!
//! # No dependency
//!
//! There is no bignum crate available offline, so this is the arithmetic. It is deliberately
//! the simplest correct algorithm at each step — schoolbook multiply, binary long division
//! for the general case, divide-by-10^9 for decimal output — rather than the fastest.

use std::fmt;

/// The largest exponent `**` will compute. An exponent that would need more bits than this
/// is a `RangeError` rather than an allocation the process cannot survive.
const MAX_POW_BITS: u64 = 1 << 24;

/// An arbitrary-precision integer.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct BigInt {
    /// Whether the value is below zero. Zero is never negative: `-0n` is `0n`.
    negative: bool,
    /// Base 2^32 magnitude, little-endian, with no trailing zero words.
    mag: Vec<u32>,
}

impl BigInt {
    /// Zero.
    pub fn zero() -> BigInt {
        BigInt {
            negative: false,
            mag: Vec::new(),
        }
    }

    /// One.
    pub fn one() -> BigInt {
        BigInt::from_i64(1)
    }

    /// The value of an `i64`.
    pub fn from_i64(value: i64) -> BigInt {
        let negative = value < 0;
        // `i64::MIN` has no positive counterpart, so the magnitude comes from the unsigned
        // form rather than from `abs`.
        let magnitude = value.unsigned_abs();
        let mut big = BigInt::from_u64_magnitude(magnitude);
        big.negative = negative && !big.is_zero();
        big
    }

    fn from_u64_magnitude(value: u64) -> BigInt {
        let mut mag = vec![value as u32, (value >> 32) as u32];
        bigint_trim(&mut mag);
        BigInt {
            negative: false,
            mag,
        }
    }

    /// The value of an `f64` truncated toward zero, which the relational comparison needs.
    ///
    /// Only called for a finite value whose magnitude is below `1.7e38`, so the exponent is
    /// bounded and the word count stays small.
    pub fn from_f64_truncated(value: f64) -> BigInt {
        let negative = value < 0.0;
        let magnitude = value.abs();
        if !magnitude.is_finite() || magnitude == 0.0 {
            return BigInt::zero();
        }
        let mut mag = Vec::new();
        // Peel 32 bits at a time off the top of the value.
        let mut remaining = magnitude;
        while remaining >= 1.0 {
            let low = remaining % 4294967296.0;
            mag.push(low as u32);
            remaining = (remaining - low) / 4294967296.0;
        }
        bigint_trim(&mut mag);
        BigInt {
            negative: negative && !mag.is_empty(),
            mag,
        }
    }

    /// Whether this is zero.
    pub fn is_zero(&self) -> bool {
        self.mag.is_empty()
    }

    /// Whether this is one.
    pub fn is_one(&self) -> bool {
        !self.negative && self.mag.len() == 1 && self.mag[0] == 1
    }

    /// Whether the value is below zero.
    pub fn is_negative(&self) -> bool {
        self.negative
    }

    /// The value as an `f64`, for the relational comparisons that accept a Number.
    ///
    /// Rounding is the platform's, which is what `BigInt < Number` needs: the comparison is
    /// on the mathematical values, and an `f64` cannot tell `2^60` from `2^60 + 1` anyway.
    pub fn to_f64(&self) -> f64 {
        let mut value = 0.0f64;
        for word in self.mag.iter().rev() {
            value = value * 4294967296.0 + *word as f64;
        }
        if self.negative { -value } else { value }
    }

    /// The value as an `i64` when it fits, which `BigInt.asIntN`-free paths use.
    pub fn to_i64(&self) -> Option<i64> {
        if self.mag.len() > 2 {
            return None;
        }
        let magnitude = self.mag.first().copied().unwrap_or(0) as u64
            | ((self.mag.get(1).copied().unwrap_or(0) as u64) << 32);
        if self.negative {
            // `-(i64::MIN as u64)` overflows, so the check is on the unsigned magnitude.
            if magnitude > i64::MIN.unsigned_abs() {
                return None;
            }
            Some((magnitude as i64).wrapping_neg())
        } else {
            i64::try_from(magnitude).ok()
        }
    }

    /// Parse `text` in `radix`, which is what a literal and `BigInt("…")` both need.
    ///
    /// Surrounding whitespace is not accepted here; the caller trims it, because the literal
    /// grammar and the string grammar differ in what they allow around the digits.
    pub fn from_radix(text: &str, radix: u32) -> Option<BigInt> {
        if !(2..=36).contains(&radix) || text.is_empty() {
            return None;
        }
        let mut result = BigInt::zero();
        let multiplier = BigInt::from_i64(radix as i64);
        for character in text.chars() {
            let digit = character.to_digit(radix)?;
            result = result.mul(&multiplier).add(&BigInt::from_i64(digit as i64));
        }
        Some(result)
    }

    /// Parse a JS `BigInt` string: optional sign, optional `0x`/`0o`/`0b`, no fraction.
    pub fn from_string(text: &str) -> Option<BigInt> {
        let text = text.trim();
        // `StringToBigInt` is a `StringNumericLiteral` without a fraction, and an empty
        // literal is zero — but only when it is empty outright: `"+"` and `"-"` carry a
        // sign and no digits, which stays a `SyntaxError`.
        if text.is_empty() {
            return Some(BigInt::zero());
        }
        let (negative, body) = match text.strip_prefix('-') {
            Some(rest) => (true, rest.trim_start()),
            None => (false, text.strip_prefix('+').unwrap_or(text).trim_start()),
        };
        let (radix, digits) =
            if let Some(rest) = body.strip_prefix("0x").or(body.strip_prefix("0X")) {
                (16, rest)
            } else if let Some(rest) = body.strip_prefix("0o").or(body.strip_prefix("0O")) {
                (8, rest)
            } else if let Some(rest) = body.strip_prefix("0b").or(body.strip_prefix("0B")) {
                (2, rest)
            } else {
                (10, body)
            };
        let value = BigInt::from_radix(digits, radix)?;
        Some(if negative { value.negate() } else { value })
    }

    /// The negation.
    pub fn negate(&self) -> BigInt {
        if self.is_zero() {
            // `-0n` is `0n`, not a negative zero.
            return BigInt::zero();
        }
        BigInt {
            negative: !self.negative,
            mag: self.mag.clone(),
        }
    }

    /// The absolute value.
    pub fn abs(&self) -> BigInt {
        BigInt {
            negative: false,
            mag: self.mag.clone(),
        }
    }

    /// Signed addition.
    pub fn add(&self, other: &BigInt) -> BigInt {
        if self.negative == other.negative {
            let mut mag = add_mag(&self.mag, &other.mag);
            bigint_trim(&mut mag);
            return BigInt {
                negative: self.negative && !mag.is_empty(),
                mag,
            };
        }
        // Opposite signs: the larger magnitude decides the sign and the difference is the
        // result.
        match cmp_mag(&self.mag, &other.mag) {
            std::cmp::Ordering::Equal => BigInt::zero(),
            std::cmp::Ordering::Greater => {
                let mut mag = sub_mag(&self.mag, &other.mag);
                bigint_trim(&mut mag);
                BigInt {
                    negative: self.negative,
                    mag,
                }
            }
            std::cmp::Ordering::Less => {
                let mut mag = sub_mag(&other.mag, &self.mag);
                bigint_trim(&mut mag);
                BigInt {
                    negative: other.negative,
                    mag,
                }
            }
        }
    }

    /// Signed subtraction.
    pub fn sub(&self, other: &BigInt) -> BigInt {
        self.add(&other.negate())
    }

    /// Signed multiplication.
    pub fn mul(&self, other: &BigInt) -> BigInt {
        if self.is_zero() || other.is_zero() {
            return BigInt::zero();
        }
        let mut mag = vec![0u32; self.mag.len() + other.mag.len()];
        for (i, a) in self.mag.iter().enumerate() {
            let mut carry = 0u64;
            for (j, b) in other.mag.iter().enumerate() {
                let slot = &mut mag[i + j];
                let total = (*a as u64) * (*b as u64) + (*slot as u64) + carry;
                *slot = total as u32;
                carry = total >> 32;
            }
            let mut k = i + other.mag.len();
            while carry != 0 {
                let total = mag[k] as u64 + carry;
                mag[k] = total as u32;
                carry = total >> 32;
                k += 1;
            }
        }
        bigint_trim(&mut mag);
        BigInt {
            negative: self.negative != other.negative,
            mag,
        }
    }

    /// Signed division, truncating toward zero, with the remainder.
    ///
    /// `None` for a zero divisor, which the caller turns into a `RangeError`.
    pub fn div_rem(&self, other: &BigInt) -> Option<(BigInt, BigInt)> {
        if other.is_zero() {
            return None;
        }
        let (quotient, remainder) = div_rem_mag(&self.mag, &other.mag);
        let mut quotient = quotient;
        let mut remainder = remainder;
        bigint_trim(&mut quotient);
        bigint_trim(&mut remainder);
        Some((
            BigInt {
                // Truncation toward zero, so the sign of the quotient is the product of the
                // signs — that is what makes `-7n / 2n` equal `-3n`.
                negative: (self.negative != other.negative) && !quotient.is_empty(),
                mag: quotient,
            },
            BigInt {
                negative: self.negative && !remainder.is_empty(),
                mag: remainder,
            },
        ))
    }

    /// `self` raised to `exponent`, by repeated squaring.
    pub fn pow(&self, exponent: &BigInt) -> Option<BigInt> {
        if exponent.negative {
            // A negative exponent on integers is `RangeError` — there is no fraction.
            return None;
        }
        let mut exponent = exponent.to_i64()? as u64;
        // Guard the allocation: the result needs `bits(self) * exponent` bits.
        let bits = self.mag.len() as u64 * 32;
        if bits.saturating_mul(exponent) > MAX_POW_BITS {
            return None;
        }
        let mut base = self.clone();
        let mut result = BigInt::one();
        while exponent > 0 {
            if exponent & 1 == 1 {
                result = result.mul(&base);
            }
            exponent >>= 1;
            if exponent > 0 {
                base = base.mul(&base);
            }
        }
        Some(result)
    }

    /// `self << bits`.
    pub fn shl(&self, bits: u64) -> BigInt {
        if self.is_zero() || bits == 0 {
            return self.clone();
        }
        let words = (bits / 32) as usize;
        let shift = (bits % 32) as u32;
        let mut mag = vec![0u32; words];
        if shift == 0 {
            mag.extend_from_slice(&self.mag);
        } else {
            // Each word contributes its low bits to its own slot and its high bits to
            // the next one, so the carry has to travel with the loop.
            let mut carry = 0u32;
            for word in &self.mag {
                mag.push((*word << shift) | carry);
                carry = *word >> (32 - shift);
            }
            if carry != 0 {
                mag.push(carry);
            }
        }
        bigint_trim(&mut mag);
        BigInt {
            negative: self.negative,
            mag,
        }
    }

    /// `self >> bits`, arithmetic — the same as `floor(self / 2^bits)`.
    pub fn shr(&self, bits: u64) -> BigInt {
        if bits == 0 {
            return self.clone();
        }
        let words = (bits / 32) as usize;
        let shift = (bits % 32) as u32;
        if words >= self.mag.len() {
            // Everything is shifted out; the floor of a negative value is `-1`.
            return if self.negative {
                BigInt::from_i64(-1)
            } else {
                BigInt::zero()
            };
        }
        let mut mag = self.mag[words..].to_vec();
        let mut lost_bit = false;
        if shift != 0 {
            for index in 0..mag.len() {
                let next = mag.get(index + 1).copied().unwrap_or(0);
                if index == 0 && mag[0] & ((1 << shift) - 1) != 0 {
                    lost_bit = true;
                }
                mag[index] = (mag[index] >> shift) | (next << (32 - shift));
            }
        }
        // The words that were dropped entirely also count as shifted-out bits.
        if self.mag[..words].iter().any(|word| *word != 0) {
            lost_bit = true;
        }
        bigint_trim(&mut mag);
        let mut result = BigInt {
            negative: self.negative && !mag.is_empty(),
            mag,
        };
        // Truncating a negative value rounds *toward zero*, so the floor needs one more
        // step down whenever a bit was discarded.
        if self.negative && lost_bit {
            result = result.sub(&BigInt::one());
        }
        result
    }

    /// `self & other`, on the infinite two's-complement form.
    pub fn bitand(&self, other: &BigInt) -> BigInt {
        self.bitwise(other, |a, b| a & b)
    }

    /// `self | other`.
    pub fn bitor(&self, other: &BigInt) -> BigInt {
        self.bitwise(other, |a, b| a | b)
    }

    /// `self ^ other`.
    pub fn bitxor(&self, other: &BigInt) -> BigInt {
        self.bitwise(other, |a, b| a ^ b)
    }

    /// `~self`.
    pub fn bitnot(&self) -> BigInt {
        // `~x` is `-x - 1` for every integer, which needs no width at all.
        self.negate().sub(&BigInt::one())
    }

    fn bitwise(&self, other: &BigInt, op: impl Fn(u32, u32) -> u32) -> BigInt {
        // One extra word so the sign extension is materialised rather than inferred.
        let width = self.mag.len().max(other.mag.len()) + 1;
        let left = self.twos_complement(width);
        let right = other.twos_complement(width);
        let words: Vec<u32> = left
            .iter()
            .zip(right.iter())
            .map(|(a, b)| op(*a, *b))
            .collect();
        BigInt::from_twos_complement(words)
    }

    /// The low `width` words of the two's-complement form.
    fn twos_complement(&self, width: usize) -> Vec<u32> {
        let mut words = vec![0u32; width];
        for (index, word) in self.mag.iter().enumerate().take(width) {
            words[index] = *word;
        }
        if self.negative {
            for word in words.iter_mut() {
                *word = !*word;
            }
            let mut carry = 1u64;
            for word in words.iter_mut() {
                let total = *word as u64 + carry;
                *word = total as u32;
                carry = total >> 32;
            }
        }
        words
    }

    /// Interpret `words` as two's complement, whose top bit decides the sign.
    fn from_twos_complement(mut words: Vec<u32>) -> BigInt {
        let negative = words.last().is_some_and(|word| word & 0x8000_0000 != 0);
        if negative {
            for word in words.iter_mut() {
                *word = !*word;
            }
            let mut carry = 1u64;
            for word in words.iter_mut() {
                let total = *word as u64 + carry;
                *word = total as u32;
                carry = total >> 32;
            }
        }
        bigint_trim(&mut words);
        BigInt {
            negative: negative && !words.is_empty(),
            mag: words,
        }
    }

    /// `BigInt.asUintN(bits, self)`.
    pub fn as_uint_n(&self, bits: u64) -> BigInt {
        if bits == 0 {
            return BigInt::zero();
        }
        let mask_words = bits.div_ceil(32) as usize;
        let mut words = self.twos_complement(mask_words);
        let spare = (mask_words as u64 * 32 - bits) as u32;
        if spare > 0
            && let Some(last) = words.last_mut()
        {
            *last &= u32::MAX >> spare;
        }
        bigint_trim(&mut words);
        BigInt {
            negative: false,
            mag: words,
        }
    }

    /// `BigInt.asIntN(bits, self)`.
    pub fn as_int_n(&self, bits: u64) -> BigInt {
        if bits == 0 {
            return BigInt::zero();
        }
        let mask_words = bits.div_ceil(32) as usize;
        let mut words = self.twos_complement(mask_words);
        let spare = (mask_words as u64 * 32 - bits) as u32;
        if spare > 0
            && let Some(last) = words.last_mut()
        {
            // Keep the bit below the spare ones as the sign, then re-extend.
            *last &= u32::MAX >> spare;
        }
        let negative = words
            .last()
            .is_some_and(|word| word & (1 << ((bits - 1) % 32)) != 0);
        if negative && spare > 0 {
            if let Some(last) = words.last_mut() {
                *last |= u32::MAX << (32 - spare);
            }
        }
        BigInt::from_twos_complement(words)
    }

    /// The decimal text, which is what `toString()` and the printer need.
    pub fn to_decimal_string(&self) -> String {
        self.to_radix_string(10)
    }

    /// The text in `radix`.
    pub fn to_radix_string(&self, radix: u32) -> String {
        if self.is_zero() {
            return "0".to_string();
        }
        let radix = radix.clamp(2, 36);
        let mut digits = Vec::new();
        let mut magnitude = self.mag.clone();
        // Peel off nine decimal digits at a time, which is the largest power of ten that
        // fits a `u32` and therefore the cheapest chunk the long division can take.
        let chunk = 1_000_000_000u32;
        if radix == 10 {
            while !magnitude.is_empty() {
                let remainder = div_rem_small(&mut magnitude, chunk);
                if magnitude.is_empty() {
                    digits.push(remainder.to_string());
                } else {
                    digits.push(format!("{remainder:09}"));
                }
            }
        } else {
            while !magnitude.is_empty() {
                let remainder = div_rem_small(&mut magnitude, radix);
                digits.push(
                    char::from_digit(remainder, radix)
                        .unwrap_or('0')
                        .to_string(),
                );
            }
        }
        digits.reverse();
        let body = digits.concat();
        if self.negative {
            format!("-{body}")
        } else {
            body
        }
    }
}

impl js_gc::Trace for BigInt {
    /// Nothing in a bignum is a GC value: it is a sign and machine words.
    fn trace(&self, _tracer: &mut js_gc::Tracer) {}
}

impl fmt::Display for BigInt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_decimal_string())
    }
}

impl PartialOrd for BigInt {
    fn partial_cmp(&self, other: &BigInt) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for BigInt {
    fn cmp(&self, other: &BigInt) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        match (self.negative, other.negative) {
            (false, true) => Ordering::Greater,
            (true, false) => Ordering::Less,
            (false, false) => cmp_mag(&self.mag, &other.mag),
            (true, true) => cmp_mag(&other.mag, &self.mag),
        }
    }
}

/// Drop trailing zero words, so every value has one representation.
fn bigint_trim(words: &mut Vec<u32>) {
    while words.last() == Some(&0) {
        words.pop();
    }
}

fn cmp_mag(a: &[u32], b: &[u32]) -> std::cmp::Ordering {
    if a.len() != b.len() {
        return a.len().cmp(&b.len());
    }
    for index in (0..a.len()).rev() {
        if a[index] != b[index] {
            return a[index].cmp(&b[index]);
        }
    }
    std::cmp::Ordering::Equal
}

fn add_mag(a: &[u32], b: &[u32]) -> Vec<u32> {
    let (long, short) = if a.len() >= b.len() { (a, b) } else { (b, a) };
    let mut out = Vec::with_capacity(long.len() + 1);
    let mut carry = 0u64;
    for (index, word) in long.iter().enumerate() {
        let total = *word as u64 + short.get(index).copied().unwrap_or(0) as u64 + carry;
        out.push(total as u32);
        carry = total >> 32;
    }
    if carry != 0 {
        out.push(carry as u32);
    }
    out
}

/// `a - b`, requiring `a >= b`.
fn sub_mag(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut out = Vec::with_capacity(a.len());
    let mut borrow = 0i64;
    for (index, word) in a.iter().enumerate() {
        let total = *word as i64 - b.get(index).copied().unwrap_or(0) as i64 - borrow;
        if total < 0 {
            out.push((total + (1i64 << 32)) as u32);
            borrow = 1;
        } else {
            out.push(total as u32);
            borrow = 0;
        }
    }
    out
}

/// Divide the magnitude by a small value, returning the remainder.
fn div_rem_small(words: &mut Vec<u32>, divisor: u32) -> u32 {
    let mut remainder = 0u64;
    for index in (0..words.len()).rev() {
        let total = (remainder << 32) | words[index] as u64;
        words[index] = (total / divisor as u64) as u32;
        remainder = total % divisor as u64;
    }
    bigint_trim(words);
    remainder as u32
}

/// Binary long division on the magnitudes.
///
/// Simple and obviously correct rather than fast: shift the divisor up to the dividend's
/// bit length, then subtract wherever it fits, which is the textbook restoring division.
fn div_rem_mag(a: &[u32], b: &[u32]) -> (Vec<u32>, Vec<u32>) {
    let mut remainder = Vec::new();
    let mut quotient = vec![0u32; a.len()];
    let bits = a.len() as u64 * 32;
    for bit in (0..bits).rev() {
        // `remainder = remainder << 1 | bit_of_a`
        let mut carry = (a[(bit / 32) as usize] >> (bit % 32)) & 1;
        for word in remainder.iter_mut() {
            let next = *word >> 31;
            *word = (*word << 1) | carry;
            carry = next;
        }
        if carry != 0 {
            remainder.push(carry);
        }
        bigint_trim(&mut remainder);
        if cmp_mag(&remainder, b) != std::cmp::Ordering::Less {
            remainder = sub_mag(&remainder, b);
            bigint_trim(&mut remainder);
            quotient[(bit / 32) as usize] |= 1 << (bit % 32);
        }
    }
    (quotient, remainder)
}

#[cfg(test)]
mod tests {
    use super::BigInt;

    fn big(value: i64) -> BigInt {
        BigInt::from_i64(value)
    }

    fn parsed(text: &str) -> BigInt {
        BigInt::from_string(text).expect("parses")
    }

    #[test]
    fn decimal_round_trips() {
        for text in [
            "0",
            "1",
            "-1",
            "123456789012345678901234567890",
            "-999999999999999999999999999999",
        ] {
            assert_eq!(parsed(text).to_decimal_string(), text);
        }
        // `-0n` is `0n`, so the sign never appears on a zero.
        assert_eq!(parsed("-0").to_decimal_string(), "0");
    }

    #[test]
    fn arithmetic_matches_i64_for_small_values() {
        let cases = [
            (7i64, 3i64),
            (-7, 3),
            (7, -3),
            (-7, -3),
            (0, 5),
            (123456, -789),
        ];
        for (a, b) in cases {
            assert_eq!(big(a).add(&big(b)).to_i64(), Some(a + b), "{a} + {b}");
            assert_eq!(big(a).sub(&big(b)).to_i64(), Some(a - b), "{a} - {b}");
            assert_eq!(big(a).mul(&big(b)).to_i64(), Some(a * b), "{a} * {b}");
            // Division truncates toward zero, like the language's `BigInt` division.
            assert_eq!(
                big(a).div_rem(&big(b)).unwrap().0.to_i64(),
                Some(a / b),
                "{a} / {b}"
            );
            assert_eq!(
                big(a).div_rem(&big(b)).unwrap().1.to_i64(),
                Some(a % b),
                "{a} % {b}"
            );
        }
    }

    #[test]
    fn division_is_exact_on_large_values() {
        // `2^1000 - (2^1000 - 1)` is 1: the arithmetic has to be exact at 1000 bits.
        let big_power = big(2).pow(&big(1000)).unwrap();
        let difference = big_power.sub(&big_power.sub(&big(1)));
        assert_eq!(difference.to_decimal_string(), "1");
        // …and dividing it back out recovers the operand.
        let (quotient, remainder) = big_power.div_rem(&big_power.sub(&big(1))).unwrap();
        assert_eq!(quotient.to_decimal_string(), "1");
        assert_eq!(remainder.to_decimal_string(), "1");
    }

    #[test]
    fn shifting_is_arithmetic() {
        assert_eq!(
            big(1).shl(128).to_decimal_string(),
            "340282366920938463463374607431768211456"
        );
        assert_eq!(big(1).shl(128).shr(128).to_decimal_string(), "1");
        // `-1n >> 1n` is `-1n`: the shift is a floor, not a truncation.
        assert_eq!(big(-1).shr(1).to_decimal_string(), "-1");
        assert_eq!(big(-2).shr(1).to_decimal_string(), "-1");
        assert_eq!(big(-3).shr(1).to_decimal_string(), "-2");
        assert_eq!(big(-1).shl(4).to_decimal_string(), "-16");
    }

    #[test]
    fn bitwise_uses_twos_complement() {
        // `-1n` is all ones, so `&` gives the other operand back and `|` gives `-1n`.
        assert_eq!(big(-1).bitand(&big(3)).to_i64(), Some(3));
        assert_eq!(big(-1).bitor(&big(3)).to_i64(), Some(-1));
        assert_eq!(big(-1).bitxor(&big(3)).to_i64(), Some(-4));
        // `~x` is `-x - 1`.
        assert_eq!(big(0).bitnot().to_i64(), Some(-1));
        assert_eq!(big(-1).bitnot().to_i64(), Some(0));
        assert_eq!(big(12).bitand(&big(10)).to_i64(), Some(8));
        assert_eq!(big(12).bitor(&big(10)).to_i64(), Some(14));
        assert_eq!(big(12).bitxor(&big(10)).to_i64(), Some(6));
        // A value wider than one word keeps working.
        let wide = big(1).shl(64);
        assert_eq!(
            wide.bitand(&wide).to_decimal_string(),
            "18446744073709551616"
        );
    }

    #[test]
    fn as_int_n_and_as_uint_n_wrap() {
        assert_eq!(big(255).as_uint_n(8).to_i64(), Some(255));
        assert_eq!(big(256).as_uint_n(8).to_i64(), Some(0));
        assert_eq!(big(-1).as_uint_n(8).to_i64(), Some(255));
        assert_eq!(big(128).as_int_n(8).to_i64(), Some(-128));
        assert_eq!(big(127).as_int_n(8).to_i64(), Some(127));
        assert_eq!(big(-129).as_int_n(8).to_i64(), Some(127));
        assert_eq!(big(5).as_int_n(0).to_i64(), Some(0));
    }

    #[test]
    fn ordering_is_signed() {
        assert!(big(-2) < big(1));
        assert!(big(-2) < big(-1));
        assert!(big(2) > big(1));
        assert_eq!(big(0), big(-0));
        let huge = parsed("100000000000000000000000");
        assert!(huge > big(i64::MAX));
        assert!(huge.negate() < big(i64::MIN + 1));
    }

    #[test]
    fn pow_guards_absurd_exponents() {
        assert!(
            big(2).pow(&big(-1)).is_none(),
            "a negative exponent has no integer result"
        );
        assert!(
            big(2).pow(&big(1_000_000_000)).is_none(),
            "the result would not fit memory"
        );
        assert_eq!(big(2).pow(&big(10)).unwrap().to_i64(), Some(1024));
        assert_eq!(big(0).pow(&big(0)).unwrap().to_i64(), Some(1));
    }
}
