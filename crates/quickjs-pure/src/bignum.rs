//! Unsigned big integers — only the arithmetic exact radix conversion needs.
//!
//! `Number.prototype.toString(radix)` has to print the *shortest* digit string that
//! reads back as the same double. The reference decides that in arbitrary precision:
//! `dtoa.c` keeps the value as a bignum mantissa times a power of two, rounds a
//! candidate mantissa in the output radix, converts it back and compares bit patterns.
//! Doing the same in floating point is what made `(1.3).toString(7)` come out one digit
//! short and misrounded. Add, subtract, multiply, shift, compare and division by a small
//! integer are enough for that, so this is a limb vector rather than a bignum library.

use std::cmp::Ordering;

/// A non-negative arbitrary-precision integer.
///
/// The vector holds little-endian 32-bit limbs; the most significant limb is never
/// zero and an empty vector is zero, so equal values always compare equal limb-wise.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Big {
    limbs: Vec<u32>,
}

impl Big {
    /// Zero.
    pub(crate) fn zero() -> Big {
        Big { limbs: Vec::new() }
    }

    /// One.
    pub(crate) fn one() -> Big {
        Big::from_u64(1)
    }

    /// The integer `value`.
    pub(crate) fn from_u64(value: u64) -> Big {
        let mut big = Big {
            limbs: vec![value as u32, (value >> 32) as u32],
        };
        big.trim();
        big
    }

    /// Whether this is zero.
    pub(crate) fn is_zero(&self) -> bool {
        self.limbs.is_empty()
    }

    /// Whether the low bit is set.
    pub(crate) fn is_odd(&self) -> bool {
        self.limbs.first().is_some_and(|low| low & 1 == 1)
    }

    /// Drop the high limbs that turned out to be zero.
    fn trim(&mut self) {
        while self.limbs.last() == Some(&0) {
            self.limbs.pop();
        }
    }

    /// Compare two non-negative integers.
    pub(crate) fn cmp(&self, other: &Big) -> Ordering {
        match self.limbs.len().cmp(&other.limbs.len()) {
            Ordering::Equal => {
                for (left, right) in self.limbs.iter().zip(&other.limbs).rev() {
                    match left.cmp(right) {
                        Ordering::Equal => {}
                        other => return other,
                    }
                }
                Ordering::Equal
            }
            other => other,
        }
    }

    /// Multiply by a value that fits in 32 bits.
    pub(crate) fn mul_small(&mut self, factor: u32) {
        if factor == 0 {
            self.limbs.clear();
            return;
        }
        if self.is_zero() {
            return;
        }
        let mut carry = 0u64;
        for limb in &mut self.limbs {
            let product = *limb as u64 * factor as u64 + carry;
            *limb = product as u32;
            carry = product >> 32;
        }
        if carry != 0 {
            self.limbs.push(carry as u32);
        }
    }

    /// Multiply by `base` raised to `exponent`.
    pub(crate) fn mul_pow(&mut self, base: u32, exponent: u32) {
        for _ in 0..exponent {
            self.mul_small(base);
        }
    }

    /// Multiply by two raised to `bits`.
    pub(crate) fn shl(&mut self, bits: u32) {
        if bits == 0 || self.is_zero() {
            return;
        }
        let words = (bits / 32) as usize;
        let shift = bits % 32;
        if shift == 0 {
            self.limbs.splice(0..0, std::iter::repeat_n(0, words));
            return;
        }
        let mut carry = 0u32;
        for limb in &mut self.limbs {
            let shifted = (*limb << shift) | carry;
            carry = *limb >> (32 - shift);
            *limb = shifted;
        }
        if carry != 0 {
            self.limbs.push(carry);
        }
        if words > 0 {
            self.limbs.splice(0..0, std::iter::repeat_n(0, words));
        }
    }

    /// Add a value that fits in 32 bits.
    pub(crate) fn add_small(&mut self, addend: u32) {
        let mut carry = addend as u64;
        for limb in &mut self.limbs {
            if carry == 0 {
                return;
            }
            let sum = *limb as u64 + carry;
            *limb = sum as u32;
            carry = sum >> 32;
        }
        if carry != 0 {
            self.limbs.push(carry as u32);
        }
    }

    /// Add one.
    pub(crate) fn add_one(&mut self) {
        self.add_small(1);
    }

    /// Subtract one. The caller must know the value is positive.
    pub(crate) fn sub_one(&mut self) {
        for limb in &mut self.limbs {
            if *limb > 0 {
                *limb -= 1;
                self.trim();
                return;
            }
            *limb = u32::MAX;
        }
        unreachable!("sub_one on zero");
    }

    /// `self - other`. The caller must know `self >= other`.
    fn sub(&self, other: &Big) -> Big {
        let mut limbs = Vec::with_capacity(self.limbs.len());
        let mut borrow = 0i64;
        for (index, limb) in self.limbs.iter().enumerate() {
            let right = other.limbs.get(index).copied().unwrap_or(0) as i64;
            let mut difference = *limb as i64 - right - borrow;
            borrow = 0;
            if difference < 0 {
                difference += 1 << 32;
                borrow = 1;
            }
            limbs.push(difference as u32);
        }
        let mut result = Big { limbs };
        result.trim();
        result
    }

    /// `self * other`.
    pub(crate) fn mul(&self, other: &Big) -> Big {
        if self.is_zero() || other.is_zero() {
            return Big::zero();
        }
        let mut limbs = vec![0u32; self.limbs.len() + other.limbs.len()];
        for (left_index, left) in self.limbs.iter().enumerate() {
            let mut carry = 0u64;
            for (right_index, right) in other.limbs.iter().enumerate() {
                let slot = left_index + right_index;
                let product = *left as u64 * *right as u64 + limbs[slot] as u64 + carry;
                limbs[slot] = product as u32;
                carry = product >> 32;
            }
            let mut slot = left_index + other.limbs.len();
            while carry != 0 {
                let sum = limbs[slot] as u64 + carry;
                limbs[slot] = sum as u32;
                carry = sum >> 32;
                slot += 1;
            }
        }
        let mut result = Big { limbs };
        result.trim();
        result
    }

    /// `self / divisor` (in place) returning the remainder.
    ///
    /// `divisor` is a value that fits in 32 bits, which is all the radix conversion
    /// needs: pulling digits off the end of a mantissa.
    pub(crate) fn div_rem_small(&mut self, divisor: u32) -> u32 {
        let mut remainder = 0u64;
        for limb in self.limbs.iter_mut().rev() {
            let dividend = (remainder << 32) | *limb as u64;
            *limb = (dividend / divisor as u64) as u32;
            remainder = dividend % divisor as u64;
        }
        self.trim();
        remainder as u32
    }

    /// `self / divisor` and `self % divisor`, by long division in `radix`.
    ///
    /// Working a digit at a time in the caller's radix keeps the code small: each
    /// quotient digit is found by trying `0..radix` against the running remainder,
    /// which is cheap because a conversion only needs a few dozen digits.
    pub(crate) fn div_rem_radix(&self, divisor: &Big, radix: u32) -> (Big, Big) {
        if self.cmp(divisor) == Ordering::Less {
            return (Big::zero(), self.clone());
        }
        // `power` walks down from the largest `divisor * radix^k` that still fits.
        let mut power = divisor.clone();
        let mut power_digits = 0u32;
        loop {
            let mut next = power.clone();
            next.mul_small(radix);
            if next.cmp(self) == Ordering::Greater {
                break;
            }
            power = next;
            power_digits += 1;
        }
        let mut remainder = self.clone();
        let mut quotient = Big::zero();
        loop {
            let mut digit = 0u32;
            while digit + 1 < radix {
                let mut candidate = power.clone();
                candidate.mul_small(digit + 1);
                if candidate.cmp(&remainder) == Ordering::Greater {
                    break;
                }
                digit += 1;
            }
            if digit > 0 {
                let mut multiple = power.clone();
                multiple.mul_small(digit);
                remainder = remainder.sub(&multiple);
            }
            quotient.mul_small(radix);
            if digit > 0 {
                quotient.add_small(digit);
            }
            if power_digits == 0 {
                break;
            }
            power_digits -= 1;
            power.div_rem_small(radix);
        }
        (quotient, remainder)
    }

    /// The value written in `radix` (2..=36), most significant digit first.
    pub(crate) fn to_radix_string(&self, radix: u32) -> String {
        if self.is_zero() {
            return "0".to_string();
        }
        let mut digits = Vec::new();
        let mut value = self.clone();
        while !value.is_zero() {
            let digit = value.div_rem_small(radix);
            digits.push(std::char::from_digit(digit, radix).unwrap_or('0'));
        }
        digits.iter().rev().collect()
    }
}
