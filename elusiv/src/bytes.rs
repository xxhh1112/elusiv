use borsh::BorshSerialize;

pub use elusiv_types::bytes::*;

/// Rounds a integer division up
pub const fn div_ceiling(divident: u64, divisor: u64) -> u64 {
    if divisor == 0 { panic!() }
    (divident + divisor - 1) / divisor
}

macro_rules! safe_num_downcast {
    ($id: ident, $h: ty, $l: ty) => {
        pub const fn $id(u: $h) -> $l {
            if u > <$l>::MAX as $h { panic!() }
            u as $l
        }
    };
}

safe_num_downcast!(u64_as_u32_safe, u64, u32);
safe_num_downcast!(usize_as_u32_safe, usize, u32);
safe_num_downcast!(usize_as_u16_safe, usize, u16);
safe_num_downcast!(usize_as_u8_safe, usize, u8);

pub const fn u64_as_usize_safe(u: u64) -> usize {
    u64_as_u32_safe(u) as usize
}

// TODO: optimize find and contains with byte alignment
pub fn contains<N: BorshSerialize + BorshSerDeSized>(v: N, data: &[u8]) -> bool {
    let length = data.len() / N::SIZE;
    find(v, data, length).is_some()
}

pub fn find<N: BorshSerialize + BorshSerDeSized>(v: N, data: &[u8], length: usize) -> Option<usize> {
    let bytes = match N::try_to_vec(&v) {
        Ok(v) => v,
        Err(_) => return None
    };

    assert!(data.len() >= length);
    'A: for i in 0..length {
        let index = i * N::SIZE;
        if data[index] == bytes[0] {
            for j in 1..N::SIZE {
                if data[index + j] != bytes[j] { continue 'A; }
            }
            return Some(i);
        }
    }
    None
}

pub fn is_zero(s: &[u8]) -> bool {
    for i in (0..s.len()).step_by(16) {
        if s.len() - i >= 16 {
            let arr: [u8; 16] = s[i..i+16].try_into().unwrap();
            if u128::from_be_bytes(arr) != 0 { return false }
        } else {
            for &bit in s.iter().skip(i) {
                if bit != 0 { return false }
            }
        }
    }
    true
}

pub fn slice_to_array<N: Default + Copy, const SIZE: usize>(s: &[N]) -> [N; SIZE] {
    assert!(s.len() >= SIZE);
    let mut a = [N::default(); SIZE];
    a[..SIZE].copy_from_slice(&s[..SIZE]);
    a
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{macros::BorshSerDeSized, types::U256};
    use borsh::BorshDeserialize;
    use solana_program::pubkey::Pubkey;

    #[test]
    fn test_max() {
        assert_eq!(max(1, 3), 3);
        assert_eq!(max(3, 1), 3);
    }

    #[test]
    fn test_div_ceiling() {
        assert_eq!(div_ceiling(3, 2), 2);
        assert_eq!(div_ceiling(4, 3), 2);
        assert_eq!(div_ceiling(7, 3), 3);
    }

    #[test]
    #[should_panic]
    fn test_div_ceiling_zero() {
        div_ceiling(0, 0);
    }

    #[test]
    fn test_pubkey_ser_de() {
        assert_eq!(Pubkey::SIZE, Pubkey::new_unique().try_to_vec().unwrap().len());
    }

    macro_rules! test_safe_downcast {
        ($fn: ident, $test_a: ident, $test_b: ident, $h: ty, $l: ty) => {
            #[test]
            fn $test_a() {
                assert_eq!($fn(<$l>::MAX as $h), <$l>::MAX);
            }

            #[test]
            #[should_panic]
            fn $test_b() {
                let _ = $fn(<$l>::MAX as $h + 1);
            }
        };
    }

    test_safe_downcast!(u64_as_u32_safe, test_u64_as_u32_safe, test_u64_as_u32_safe_panic, u64, u32);
    test_safe_downcast!(usize_as_u32_safe, test_usize_as_u32_safe, test_usize_as_u32_safe_panic, usize, u32);
    test_safe_downcast!(usize_as_u16_safe, test_usize_as_u16_safe, test_usize_as_u16_safe_panic, usize, u16);
    test_safe_downcast!(usize_as_u8_safe, test_usize_as_u8_safe, test_usize_as_u8_safe_panic, usize, u8);

    #[test]
    fn test_u64_as_usize_safe() {
        assert_eq!(u64_as_usize_safe(u32::MAX as u64), u32::MAX as usize);
    }

    #[test]
    #[should_panic]
    fn test_u64_as_usize_safe_panic() {
        assert_eq!(u64_as_usize_safe(u32::MAX as u64 + 1), u32::MAX as usize + 1);
    }

    #[test]
    fn test_find_contains() {
        let length = 1000usize;
        let mut data = vec![0; length * 8];
        for i in 0..length {
            let bytes = u64::to_le_bytes(i as u64);
            for j in 0..8 {
                data[i * 8 + j] = bytes[j];
            }
        }

        for i in 0..length {
            assert!(contains(i as u64, &data[..]));
            assert_eq!(find(i as u64, &data[..], length).unwrap(), i as usize);
        }
        for i in length..length + 20 {
            assert!(!contains(i as u64, &data[..]));
            assert!(matches!(find(i as u64, &data[..], length), None));
        }
    }

    #[test]
    fn test_override_slice() {
        let mut slice = vec![0; 256];
        U256::override_slice(&[1; 32], &mut slice[32..64]).unwrap();

        for &v in slice.iter().take(64).skip(32) {
            assert_eq!(v, 1);
        }
    }

    #[derive(BorshDeserialize, BorshSerialize, BorshSerDeSized)]
    struct A {
        d: [u8; 11],
    }

    #[derive(BorshDeserialize, BorshSerialize, BorshSerDeSized)]
    struct B { a0: A, a1: A, a2: A }

    #[derive(BorshDeserialize, BorshSerialize, BorshSerDeSized)]
    enum C {
        A { a: A },
        B { b: B },
        AB { a: A, b: B },
    }

    #[test]
    fn test_borsh_ser_de_sized() {
        assert_eq!(A::SIZE, 11);
        assert_eq!(B::SIZE, 33);
        assert_eq!(C::SIZE, 11 + 33 + 1);
    }

    #[derive(BorshDeserialize, BorshSerialize, BorshSerDeSized, PartialEq, Debug)]
    enum TestEnum {
        A { v: [u64; 1] },
        B { v: [u64; 2] },
        C {
            v: [u64; 3],
            c: u8,
        },
    }

    #[test]
    fn test_enum_len() {
        assert_eq!(TestEnum::len(0), 8);
        assert_eq!(TestEnum::len(1), 16);
        assert_eq!(TestEnum::len(2), 25);
    }

    #[test]
    fn test_deserialize_enum() {
        let a = TestEnum::A { v: [333] };
        let mut data = a.try_to_vec().unwrap();
        data.extend(vec![255; TestEnum::SIZE - 8 - 1]);
        let buf = &mut &data[..];
        assert_eq!(TestEnum::deserialize_enum(buf).unwrap(), a);
        assert_eq!(TestEnum::deserialize_enum_full(buf).unwrap(), a);
    }

    #[test]
    #[should_panic]
    fn test_deserialize_enum_full() {
        let a = TestEnum::A { v: [333] };
        let data = a.try_to_vec().unwrap();
        let buf = &mut &data[..];
        _ = TestEnum::deserialize_enum_full(buf);
    }

    #[test]
    fn test_elusiv_option() {
        assert_eq!(ElusivOption::Some("abc").option(), Some("abc"));
        assert_eq!(ElusivOption::<u8>::None.option(), None);
    }
}