use std::str::FromStr;

use nom::{
    branch::alt,
    bytes::complete::tag,
    character::complete::{char, digit1, hex_digit1, one_of},
    combinator::{eof, map_res, opt, recognize},
    error::Error as NomError,
    multi::{many0, many1},
    sequence::{preceded, terminated},
    Finish, IResult,
};

#[derive(Debug)]
pub struct Range {
    /// starting byte offset
    pub start: u64,
    /// number of bytes to read, or None to read to EOF
    pub count: Option<u64>,
}

#[derive(Debug, thiserror::Error)]
pub enum ParseRangeError {
    #[error(transparent)]
    Nom(#[from] NomError<String>),
    #[error("range start exceeds i64::MAX")]
    StartOutOfBounds,
    #[error("range end is less than start")]
    EndBeforeStart,
    #[error("byte count overflow")]
    Overflow,
}

/// ParseRangeError needs an owned error type, extra conversion for the borrowed error we get from
/// various nom parsers.
impl From<NomError<&str>> for ParseRangeError {
    fn from(err: NomError<&str>) -> Self {
        Self::Nom(NomError::new(err.input.to_owned(), err.code))
    }
}

/// Parse a hex integer preceeded with 0x
fn hex(input: &str) -> IResult<&str, u64> {
    // parse and run a function on the output
    map_res(
        // parse and discard a prefix, then parse and return the 2nd argument
        preceded(
            // match either of these prefixes
            alt((tag("0x"), tag("0X"))),
            // run a parser, if it matches then return all of recognize's input
            recognize(
                // one or more groups of
                // [(one or more hex digits) followed by (zero or more underscores)]
                many1(terminated(hex_digit1, many0(char('_')))),
            ),
        ),
        // what to do with the output
        |out: &str| u64::from_str_radix(&out.replace('_', ""), 16),
    )(input)
}

/// Parse an base-10 integer
fn dec(input: &str) -> IResult<&str, u64> {
    // like hex but we don't need to pull off the 0x prefix
    map_res(recognize(many1(terminated(digit1, many0(char('_'))))), |out: &str| {
        out.replace('_', "").parse()
    })(input)
}

/// Parse an integer, either decimal or hex
fn number(input: &str) -> IResult<&str, u64> {
    alt((hex, dec))(input)
}

/// The top-level raw components we parse using nom
#[derive(Debug)]
struct RangePieces {
    start: Option<u64>,
    mode: char,
    end: Option<u64>,
}

/// Parse a string into RangePieces
fn parse_range_pieces(input: &str) -> IResult<&str, RangePieces> {
    let (input, start) = opt(number)(input)?;
    let (input, mode) = one_of("-+")(input)?;
    let (input, end) = opt(number)(input)?;
    let (input, _) = eof(input)?;

    Ok((input, RangePieces { start, mode, end }))
}

/// Parse a string into a Range
impl FromStr for Range {
    type Err = ParseRangeError;

    fn from_str(input: &str) -> Result<Range, ParseRangeError> {
        let (rest, pieces) = parse_range_pieces(input).finish()?;
        assert!(rest.is_empty(), "unexpected trailing data {rest:?}");

        let start = pieces.start.unwrap_or(0);
        if start > (i64::MAX as u64) {
            return Err(ParseRangeError::StartOutOfBounds);
        }

        let count = match (pieces.mode, pieces.end) {
            // read from start to EOF
            (_, None) => None,

            // read count bytes beginning at start
            ('+', Some(count)) => Some(count),

            // read bytes start to end (inclusive).
            // Because end is inclusive, count = end-start+1
            ('-', Some(end)) => Some(
                end.checked_sub(start)
                    .ok_or(ParseRangeError::EndBeforeStart)?
                    .checked_add(1)
                    .ok_or(ParseRangeError::Overflow)?,
            ),

            // unreachable unless the one_of call in parse_range_pieces is wrong
            (sep, _) => unreachable!("unexpected separator {sep}"),
        };

        Ok(Range { start, count })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hex() {
        assert_eq!(hex("0x0"), Ok(("", 0)));
        assert_eq!(hex("0x1234 hello"), Ok((" hello", 0x1234)));
        assert_eq!(hex("0X12"), Ok(("", 18)));
        assert!(hex("").is_err());
        assert!(hex(" 0x12").is_err());
    }

    #[test]
    fn parse_dec() {
        assert_eq!(dec("0"), Ok(("", 0)));
        assert_eq!(dec("123"), Ok(("", 123)));
        assert_eq!(dec("123-456"), Ok(("-456", 123)));
    }

    #[test]
    fn parse_number() {
        assert_eq!(number("0x0"), Ok(("", 0)));
        assert_eq!(number("0x1234 hello"), Ok((" hello", 0x1234)));
        assert_eq!(number("0X12"), Ok(("", 18)));
        assert!(number("").is_err());
        assert!(number(" 0x12").is_err());

        assert_eq!(number("0"), Ok(("", 0)));
        assert_eq!(number("123"), Ok(("", 123)));
        assert_eq!(number("123-456"), Ok(("-456", 123)));
    }

    #[test]
    fn parse_range() {
        macro_rules! assert_range_matches {
            ($s:expr, $pattern:pat) => {
                match $s.parse::<Range>() {
                    $pattern => (),
                    x => panic!(
                        "range '{}' = '{:?}' does not match '{}'",
                        $s,
                        x,
                        stringify!($pattern)
                    ),
                }
            };
        }

        macro_rules! range_test {
            ($s:expr, Err) => {
                assert_range_matches!($s, Err(_))
            };
            ($s:expr, $start:expr, None) => {
                assert_range_matches!($s, Ok(Range { start: $start, count: None }));
            };
            ($s:expr, $start:expr, $count:expr) => {
                assert_range_matches!($s, Ok(Range { start: $start, count: Some($count) }));
            };
        }

        range_test!("-", 0, None);
        range_test!("+", 0, None);
        range_test!("10-", 10, None);
        range_test!("-10", 0, 11);
        range_test!("+20", 0, 20);
        range_test!("10-12", 10, 3);
        range_test!("123+456", 123, 456);

        range_test!("-0xff", 0, 256);
        range_test!("+0xff", 0, 255);
        range_test!("0x100-0x200", 256, 257);
        range_test!("0x200-", 512, None);
        range_test!("16-0xff", 16, 240);

        range_test!("1_000-0xffff_ffff", 1000, 4294966296);
        range_test!("1______0+", 10, None);
        range_test!("1_2_3_4+99", 1234, 99);

        range_test!("asdf", Err);
        range_test!("1-0xabcR", Err);
        range_test!("10", Err);
        range_test!("0x80000000_00000000+10", Err); // start exceeds isize
        range_test!("0-0xffffffff_ffffffff", Err); // overflow
    }
}
