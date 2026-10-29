// SPDX-License-Identifier: GPL-3.0-or-later
//! Python's `decimal.Decimal` as the `junit` callback writes its times: the exact value of a
//! float, sums under the default context of 28 digits, and `str()`.

/// A Python `Decimal`: the integer `digits` times ten to `exp`.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Dec {
    digits: Vec<u8>,
    exp: i64,
}

/// `Decimal(f)`: the exact value of a float, `n * 5**k` over `10**k` for `n / 2**k`.
pub(super) fn exact(f: f64) -> Dec {
    if f == 0.0 || !f.is_finite() {
        return Dec::zero();
    }
    let bits = f.abs().to_bits();
    let exponent = i64::try_from((bits >> 52) & 0x7ff).unwrap_or(0);
    let fraction = bits & ((1 << 52) - 1);
    let (mut mantissa, mut power) = if exponent == 0 {
        (fraction, -1074)
    } else {
        (fraction | (1 << 52), exponent - 1075)
    };
    while mantissa % 2 == 0 {
        mantissa /= 2;
        power += 1;
    }
    let mut digits: Vec<u8> = mantissa.to_string().bytes().map(|b| b - b'0').collect();
    if power >= 0 {
        for _ in 0..power {
            digits = times(&digits, 2);
        }
        return Dec { digits, exp: 0 };
    }
    for _ in 0..-power {
        digits = times(&digits, 5);
    }
    Dec { digits, exp: power }
}

/// `digits` times a small factor.
fn times(digits: &[u8], factor: u8) -> Vec<u8> {
    let mut out = Vec::with_capacity(digits.len() + 1);
    let mut carry = 0u8;
    for d in digits.iter().rev() {
        let v = d * factor + carry;
        out.push(v % 10);
        carry = v / 10;
    }
    if carry > 0 {
        out.push(carry);
    }
    out.reverse();
    out
}

impl Dec {
    pub(super) fn zero() -> Self {
        Self {
            digits: vec![0],
            exp: 0,
        }
    }

    pub(super) fn is_zero(&self) -> bool {
        self.digits.iter().all(|d| *d == 0)
    }

    /// `self + other` under the default context: exact, then rounded half-even to 28 digits.
    pub(super) fn add(&self, other: &Dec) -> Dec {
        let exp = self.exp.min(other.exp);
        let align = |d: &Dec| {
            let mut v = d.digits.clone();
            v.extend(std::iter::repeat_n(
                0,
                usize::try_from(d.exp - exp).unwrap_or(0),
            ));
            v
        };
        let (a, b) = (align(self), align(other));
        let len = a.len().max(b.len());
        let mut sum = Vec::with_capacity(len + 1);
        let mut carry = 0u8;
        for i in 0..len {
            let x = if i < a.len() { a[a.len() - 1 - i] } else { 0 };
            let y = if i < b.len() { b[b.len() - 1 - i] } else { 0 };
            let v = x + y + carry;
            sum.push(v % 10);
            carry = v / 10;
        }
        if carry > 0 {
            sum.push(carry);
        }
        sum.reverse();
        while sum.len() > 1 && sum[0] == 0 {
            sum.remove(0);
        }
        Dec { digits: sum, exp }.rounded(28)
    }

    fn rounded(mut self, precision: usize) -> Dec {
        if self.digits.len() <= precision {
            return self;
        }
        let dropped = self.digits.split_off(precision);
        self.exp += i64::try_from(dropped.len()).unwrap_or(0);
        let rest_nonzero = dropped[1..].iter().any(|d| *d != 0);
        let up = dropped[0] > 5
            || (dropped[0] == 5
                && (rest_nonzero || self.digits.last().is_some_and(|d| d % 2 == 1)));
        if up {
            let mut i = self.digits.len();
            loop {
                if i == 0 {
                    self.digits.insert(0, 1);
                    self.digits.pop();
                    self.exp += 1;
                    break;
                }
                i -= 1;
                if self.digits[i] == 9 {
                    self.digits[i] = 0;
                } else {
                    self.digits[i] += 1;
                    break;
                }
            }
        }
        self
    }

    /// `str(Decimal)`.
    pub(super) fn text(&self) -> String {
        let coefficient: String = self.digits.iter().map(|d| char::from(b'0' + d)).collect();
        let left = self.exp + i64::try_from(coefficient.len()).unwrap_or(0);
        if self.exp <= 0 && left > -6 {
            if self.exp == 0 {
                return coefficient;
            }
            if left <= 0 {
                let zeros = "0".repeat(usize::try_from(-left).unwrap_or(0));
                return format!("0.{zeros}{coefficient}");
            }
            let at = usize::try_from(left).unwrap_or(0);
            return format!("{}.{}", &coefficient[..at], &coefficient[at..]);
        }
        let (head, tail) = coefficient.split_at(1);
        let dot = if tail.is_empty() {
            String::new()
        } else {
            format!(".{tail}")
        };
        format!("{head}{dot}E{:+}", left - 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimals_read_as_python_writes_them() {
        assert_eq!(exact(0.25).text(), "0.25");
        assert_eq!(exact(1.0).text(), "1");
        assert_eq!(
            exact(0.1).text(),
            "0.1000000000000000055511151231257827021181583404541015625"
        );
        // `sum` rounds each step to 28 digits.
        assert_eq!(
            Dec::zero().add(&exact(0.1)).text(),
            "0.1000000000000000055511151231"
        );
        assert_eq!(
            exact(1e-7).text(),
            "9.99999999999999954748111825886258685613938723690807819366455078125E-8"
        );
    }
}
