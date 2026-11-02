// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::mem::size_of;

pub trait BitsSource: Copy {
    fn into_u128(self) -> u128;
}

macro_rules! impl_bits_source {
    ($($t:ty),*) => {
        $(
            impl BitsSource for $t {
                #[inline(always)]
                fn into_u128(self) -> u128 {
                    self as u128
                }
            }
        )*
    };
}

impl_bits_source!(u8, u16, u32, u64, u128, usize);

pub trait BitsTarget: Copy {
    fn from_u128(val: u128) -> Self;
}

macro_rules! impl_bits_target {
    ($($t:ty),*) => {
        $(
            impl BitsTarget for $t {
                #[inline(always)]
                fn from_u128(val: u128) -> Self {
                    val as $t
                }
            }
        )*
    };
}

impl_bits_target!(u8, u16, u32, u64, u128, usize);

impl BitsTarget for bool {
    #[inline(always)]
    fn from_u128(val: u128) -> Self {
        val != 0
    }
}

/// Extracts the bit range `[HIGH_BIT:LOW_BIT]` (inclusive) from a numerical `input`.
#[inline(always)]
pub fn extract_bits<const HIGH_BIT: usize, const LOW_BIT: usize, R: BitsTarget, S: BitsSource>(
    input: S,
) -> R {
    const {
        assert!(HIGH_BIT >= LOW_BIT, "High bit must be greater or equal to low bit.");
        assert!(HIGH_BIT < size_of::<S>() * 8, "Source value ends before high bit");
        assert!(
            (HIGH_BIT + 1 - LOW_BIT) <= size_of::<R>() * 8,
            "Return type is not large enough to hold requested bits."
        );
    }
    let bit_count = HIGH_BIT + 1 - LOW_BIT;
    let mask = if bit_count == 128 { u128::MAX } else { (1u128 << bit_count) - 1 };
    R::from_u128((input.into_u128() >> LOW_BIT) & mask)
}

#[inline(always)]
pub fn extract_bit<const BIT: usize, R: BitsTarget, S: BitsSource>(input: S) -> R {
    extract_bits::<BIT, BIT, R, S>(input)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_bits() {
        let val: u32 = 0xabcd_1234;
        assert_eq!(extract_bits::<3, 0, u8, _>(val), 0x4);
        assert_eq!(extract_bits::<7, 4, u8, _>(val), 0x3);
        assert_eq!(extract_bits::<11, 8, u8, _>(val), 0x2);
        assert_eq!(extract_bits::<31, 24, u8, _>(val), 0xab);
        assert!(extract_bit::<2, bool, _>(val));
        assert!(!extract_bit::<0, bool, _>(val));
    }
}
