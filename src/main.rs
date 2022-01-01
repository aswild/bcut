#![deny(clippy::all)]
#![forbid(unsafe_code)]

use std::convert::TryInto;
use std::fs::File;
use std::io::prelude::*;
use std::io::{self, SeekFrom};
use std::path::PathBuf;
use std::str::FromStr;

use anyhow::{anyhow, ensure, Context, Result};
use clap::{AppSettings, Parser};
use hexyl::{BorderStyle, Input, Printer};
use regex::Regex;

/// Slice a byte range from a file
#[derive(Debug, Parser)]
#[clap(
    max_term_width = 80,
    setting = AppSettings::DeriveDisplayOrder,
    version,
)]
struct Args {
    /// Output file, omit or use "-" for stdout
    #[clap(short, long, name = "OUTFILE", parse(from_os_str))]
    output: Option<PathBuf>,

    /// Hexdump the output
    #[clap(short = 'H', long)]
    hexdump: bool,

    /// Byte range to select
    ///
    /// Byte numbers in the input start at zero.
    /// Numbers can be base-10 integers, or hex integers prefixed with "0x".
    /// RANGE can be one of these forms:
    ///   N-M   select bytes N through M (inclusive)
    ///   N+M   select M bytes starting at N
    ///   N-    select bytes N through EOF
    ///   N+    same as N-
    ///   -M    select first M bytes (same as 0-M)
    ///   +M    same as -M
    ///   -     select the whole input (same as 0-)
    ///   +     select the whole input (same as 0+)
    #[clap(name = "RANGE", verbatim_doc_comment)]
    range: String,

    /// Input file, omit or use "-" for stdin
    #[clap(name = "FILE", parse(from_os_str))]
    input: Option<PathBuf>,
}

#[derive(Debug)]
struct Range {
    /// starting byte offset
    start: u64,
    /// number of bytes to read, or None to read to EOF
    count: Option<u64>,
}

impl FromStr for Range {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        /// helper function to parse a number as hex or decimal depending on 0x prefix,
        /// and ignore underscores
        fn parse_number(s: &str) -> Result<u64, std::num::ParseIntError> {
            let s = s.replace('_', "");
            match s.strip_prefix("0x") {
                Some(x) => u64::from_str_radix(x, 16),
                None => s.parse::<u64>(),
            }
        }

        let re = Regex::new(
            r"^(?P<start>(?:0x)?[[:xdigit:]_]+)?(?P<sep>[+-])(?P<end>(?:0x)?[[:xdigit:]_]+)?$",
        )
        .unwrap();

        let caps = re.captures(s).ok_or_else(|| {
            anyhow!("invalid format. See `bcut --help` for range format details.")
        })?;

        let sep = caps.name("sep").unwrap().as_str();

        let start: u64 = caps
            .name("start")
            .map(|cap| {
                parse_number(cap.as_str())
                    .with_context(|| format!("invalid range start '{}'", cap.as_str()))
            })
            .transpose()?
            .unwrap_or(0);

        // we use start in SeekFrom::Current() which takes an i64. Thus make sure
        // this u64 can fit into an i64 and provide an error now rather than panic later.
        ensure!(start <= i64::MAX as u64, "range start {} exceeds i64::MAX", start);

        let end: Option<u64> = caps
            .name("end")
            .map(|cap| {
                parse_number(cap.as_str())
                    .with_context(|| format!("invalid range end '{}'", cap.as_str()))
            })
            .transpose()?;

        let count: Option<u64> = match (sep, end) {
            // read from start to EOF
            (_, None) => None,

            // read count bytes beginning at start
            ("+", Some(count)) => Some(count),

            // read bytes start to end (inclusive).
            // Because end is inclusive, count = end-start+1
            ("-", Some(end)) => Some(
                end.checked_sub(start)
                    .ok_or_else(|| anyhow!("range's end can't be less than start"))?
                    .checked_add(1)
                    .ok_or_else(|| anyhow!("byte count overflow"))?,
            ),

            // from our regex, sep will always be + or -
            (_, _) => unreachable!(),
        };

        Ok(Range { start, count })
    }
}

fn open_input<'a>(path: &Option<PathBuf>, stdin: &'a io::Stdin) -> Result<Input<'a>> {
    match path {
        None => Ok(Input::Stdin(stdin.lock())),
        Some(ref path) => {
            if let Some("-") = path.to_str() {
                Ok(Input::Stdin(stdin.lock()))
            } else {
                Ok(Input::File(
                    File::open(path)
                        .with_context(|| format!("unable to open '{}'", path.to_string_lossy()))?,
                ))
            }
        }
    }
}

fn run() -> Result<()> {
    let args = Args::parse();
    // parse range manually so we can control the error message rather than letting clap do it
    let range: Range = args.range.parse()?;

    let stdin = io::stdin();
    let mut input = open_input(&args.input, &stdin)?;
    if range.start != 0 {
        // hexyl::Input seek only supports from Current on pipes
        input.seek(SeekFrom::Current(range.start.try_into().unwrap()))?;
    }

    let mut input: Box<dyn Read> = match range.count {
        Some(count) => Box::new(input.take(count)),
        None => Box::new(input),
    };

    let mut output: Box<dyn Write> = match &args.output {
        None => Box::new(io::stdout()),
        Some(p) if p.to_str() == Some("-") => Box::new(io::stdout()),
        Some(path) => Box::new(File::create(path).context("failed to open output file")?),
    };

    if args.hexdump {
        let mut printer = Printer::new(&mut output, true, BorderStyle::Unicode, true);
        printer.print_all(&mut input)?;
    } else {
        io::copy(&mut input, &mut output)?;
    }

    Ok(())
}

fn main() {
    if let Err(err) = run() {
        eprintln!("Error: {:#}", err);
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::Range;

    macro_rules! assert_range_matches {
        ($s:expr, $pattern:pat) => {
            match $s.parse::<Range>() {
                $pattern => (),
                x => panic!("range '{}' = '{:?}' does not match '{}'", $s, x, stringify!($pattern)),
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

    #[test]
    fn range_test() {
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
