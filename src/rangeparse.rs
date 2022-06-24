//! Library module for parsing byte offset numbers

use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u64)]
pub enum Suffix {
    // base unit of bytes
    B = 1,
    // base-10 SI suffixes
    KB = 1_000,
    MB = 1_000_000,
    GB = 1_000_000_000,
    TB = 1_000_000_000_000,
    // powers of 2 binary suffixes
    Ki = 1 << 10,
    Mi = 1 << 20,
    Gi = 1 << 30,
    Ti = 1 << 40,
}

impl Suffix {
    #[inline]
    pub const fn multiplier(&self) -> u64 {
        *self as u64
    }

    pub fn as_str(&self) -> &'static str {
        match *self {
            Self::B => "",
            Self::KB => "KB",
            Self::MB => "MB",
            Self::GB => "GB",
            Self::TB => "TB",
            Self::Ki => "KiB",
            Self::Mi => "MiB",
            Self::Gi => "GiB",
            Self::Ti => "TiB",
        }
    }
}

impl fmt::Display for Suffix {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.pad(self.as_str())
    }
}

impl From<Suffix> for u64 {
    #[inline]
    fn from(s: Suffix) -> u64 {
        s.multiplier()
    }
}

impl From<Suffix> for usize {
    #[cfg_attr(not(debug_assertions), inline)]
    fn from(s: Suffix) -> usize {
        let val = s.multiplier();
        if cfg!(debug_assertions) {
            match val.try_into() {
                Ok(usize_val) => usize_val,
                Err(_) => panic!("Suffix {s:?} does not fit into usize"),
            }
        } else {
            val as usize
        }
    }
}

#[derive(Debug, Clone)]
pub struct InvalidSuffixError(String);

impl fmt::Display for InvalidSuffixError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "invalid suffix '{}'", self.0)
    }
}

impl std::error::Error for InvalidSuffixError {}

impl FromStr for Suffix {
    type Err = InvalidSuffixError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s.to_ascii_lowercase().as_str() {
            "" | "b" => Self::B,
            "kb" => Self::KB,
            "mb" => Self::MB,
            "gb" => Self::GB,
            "tb" => Self::TB,
            "k" | "ki" | "kib" => Self::Ki,
            "m" | "mi" | "mib" => Self::Mi,
            "g" | "gi" | "gib" => Self::Gi,
            "t" | "ti" | "tib" => Self::Ti,
            _ => return Err(InvalidSuffixError(s.to_string())),
        })
    }
}

#[derive(Debug, Clone, Copy, Eq)]
pub struct Number {
    value: u64,
    suffix: Suffix,
}

impl PartialEq for Number {
    fn eq(&self, other: &Number) -> bool {
        if self.suffix == other.suffix {
            self.value == other.value
        } else {
            self.bytes_u128() == other.bytes_u128()
        }
    }
}

impl Default for Number {
    #[inline]
    fn default() -> Self {
        Self::new(0, Suffix::B)
    }
}

impl fmt::Display for Number {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}{}", self.value, self.suffix)
    }
}

impl Number {
    #[inline]
    pub const fn new(value: u64, suffix: Suffix) -> Self {
        Self { value, suffix }
    }

    #[cfg_attr(not(debug_assertions), inline)]
    pub fn bytes(&self) -> u64 {
        if cfg!(debug_assertions) {
            match self.try_bytes() {
                Some(val) => val,
                None => panic!("{self:?} can't fit in a u64"),
            }
        } else {
            self.value * (self.suffix.multiplier())
        }
    }

    #[inline]
    pub const fn try_bytes(&self) -> Option<u64> {
        self.value.checked_mul(self.suffix.multiplier())
    }

    #[inline]
    pub const fn bytes_u128(&self) -> u128 {
        let val = self.value as u128;
        let mul = self.suffix.multiplier() as u128;
        val * mul
    }

    #[rustfmt::skip]
    pub const fn normalize(&self) -> Self {
        if self.value == 0 {
            return Self::new(0, Suffix::B);
        }

        let val = self.bytes_u128();
        let suffix =
                 if val %         (1 << 40) == 0 { Suffix::Ti }
            else if val % 1_000_000_000_000 == 0 { Suffix::TB }
            else if val %         (1 << 30) == 0 { Suffix::Gi }
            else if val %     1_000_000_000 == 0 { Suffix::GB }
            else if val %         (1 << 20) == 0 { Suffix::Mi }
            else if val %         1_000_000 == 0 { Suffix::MB }
            else if val %         (1 << 10) == 0 { Suffix::Ki }
            else if val %             1_000 == 0 { Suffix::KB }
            else { Suffix::B };

        let val64 = (val / (suffix.multiplier() as u128)) as u64;
        Self::new(val64, suffix)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_suffix_parse() {
        macro_rules! check {
            ($str:expr, Err) => {
                assert!($str.parse::<Suffix>().is_err());
            };

            ($str:expr, $expected:ident) => {
                assert!(matches!($str.parse::<Suffix>(), Ok(Suffix::$expected)))
            };
        }
        check!("", B);
        check!("b", B);
        check!("KB", KB);
        check!("kb", KB);
        check!("mB", MB);
        check!("Mb", MB);
        check!("m", Mi);
        check!("GiB", Gi);
        check!("TI", Ti);
        check!("x", Err);
        check!("10", Err);
    }

    #[test]
    fn test_suffix_values() {
        macro_rules! check {
            ($variant:ident, $multiplier:expr) => {
                assert_eq!(Suffix::$variant.multiplier(), $multiplier);
            };
        }
        check!(B, 1);
        check!(KB, 1_000);
        check!(MB, 1_000_000);
        check!(GB, 1_000_000_000);
        check!(TB, 1_000_000_000_000);
        check!(Ki, 1_024);
        check!(Mi, 1_048_576);
        check!(Gi, 1_073_741_824);
        check!(Ti, 1_099_511_627_776);
    }

    #[test]
    fn test_number_eq() {
        macro_rules! check {
            ($vl:expr, $sl:ident, $vr:expr, $sr:ident) => {
                assert_eq!(Number::new($vl, Suffix::$sl), Number::new($vr, Suffix::$sr));
            };
        }
        check!(1024, B, 1, Ki);
        check!(1234, TB, 1234, TB);
        check!(1024, Ki, 1, Mi);
        check!(1_048_576_000, B, 1000, Mi);
    }

    #[test]
    fn test_number_normalize() {
        macro_rules! check {
            ($vl:expr, $sl:ident, $vr:expr, $sr:ident) => {
                let orig = Number::new($vl, Suffix::$sl);
                let norm = orig.normalize();
                eprintln!("original: {orig:?}, normalized: {norm:?}");
                assert_eq!(norm.value, $vr);
                assert_eq!(norm.suffix, Suffix::$sr);
            };
        }
        check!(1024, B, 1, Ki);
        check!(1234, TB, 1234, TB);
        check!(1024, Ki, 1, Mi);
        check!(1_048_576_000, B, 1000, Mi);
    }
}
