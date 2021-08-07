use std::convert::TryInto;
use std::fs::File;
use std::io::prelude::*;
use std::io::{self, SeekFrom};
use std::path::PathBuf;
use std::str::FromStr;

use anyhow::{anyhow, Context, Result};
use hexyl::{BorderStyle, Input, Printer};
use regex::Regex;
use structopt::clap::AppSettings;
use structopt::StructOpt;

#[derive(Debug, StructOpt)]
#[structopt(
    about = "Slice a byte range from a file",
    setting = AppSettings::ColoredHelp,
    setting = AppSettings::DeriveDisplayOrder,
    setting = AppSettings::UnifiedHelpMessage,
    max_term_width = 80,
)]
struct Args {
    /// Output file, omit or use "-" for stdout
    #[structopt(short, long, name = "OUTFILE", parse(from_os_str))]
    output: Option<PathBuf>,

    /// Hexdump the output
    #[structopt(short = "H", long)]
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
    ///   -M    same as 0-M
    ///   +M    same as 0+M
    ///   -     select the whole input (same as 0-)
    ///   +     select the whole input (same as 0+)
    #[structopt(name = "RANGE", verbatim_doc_comment)]
    range: String,

    /// Input file, omit or use "-" for stdin
    #[structopt(name = "FILE", parse(from_os_str))]
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
        let re = Regex::new(
            r"^(?P<start>(?:0x)?[0-9a-fA-F]+)?(?P<sep>[+-])(?P<end>(?:0x)?[0-9a-fA-F]+)?$",
        )
        .unwrap();

        let caps = re.captures(s).ok_or_else(|| {
            anyhow!("invalid format. See `bcut --help` for range format details.")
        })?;

        let sep = caps.name("sep").unwrap().as_str();

        let start: u64 = caps
            .name("start") // Option<Match>
            .map(|m| parse_number(m.as_str())) // Option<Result<u64>>
            .transpose() // Result<Option<u64>>
            .with_context(|| {
                format!(
                    // add error context (must use closure in case match is None)
                    "invalid range start '{}'",
                    caps.name("start").unwrap().as_str()
                )
            })? // return Err or unwrap to Option<u64>
            .unwrap_or(0); // if no start value provided, assume 0

        // we use start in SeekFrom::Current() which takes an i64. Thus make sure
        // this u64 can fit into an i64 and provide an error now rather than panic later.
        if start > (i64::MAX as u64) {
            return Err(anyhow!("range start {} exceeds i64::MAX", start));
        }

        let end: Option<u64> = caps
            .name("end")
            .map(|m| parse_number(m.as_str()))
            .transpose()
            .with_context(|| {
                format!("invalid range end '{}'", caps.name("end").unwrap().as_str())
            })?;

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

fn parse_number(s: &str) -> Result<u64, std::num::ParseIntError> {
    match s.strip_prefix("0x") {
        Some(x) => u64::from_str_radix(x, 16),
        None => s.parse::<u64>(),
    }
}

fn open_input<'a>(path: &Option<PathBuf>, stdin: &'a io::Stdin) -> Result<Input<'a>> {
    match path {
        None => Ok(Input::Stdin(stdin.lock())),
        Some(ref path) => {
            if let Some("-") = path.to_str() {
                Ok(Input::Stdin(stdin.lock()))
            } else {
                Ok(Input::File(File::open(path).with_context(|| {
                    format!("unable to open '{}'", path.to_string_lossy())
                })?))
            }
        }
    }
}

fn run() -> Result<()> {
    let args = Args::from_args();
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
        // have to re-map the error with anyhow because print_all in hexyl 0.8.0 returns an unsized
        // Box<dyn std::error::Error>
        printer
            .print_all(&mut input)
            .map_err(|e| anyhow!("{}", e))?;
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
        ($s:expr, $( $pattern:pat )|+ $( if $guard:expr )?) => {
            let result = Range::from_str($s);
            match result {
                $( $pattern )|+ $( if $guard )? => (),
                x => panic!("range '{}' = '{:?}' does not match '{}'",
                            $s, x, stringify!($( $pattern )|+ $( if $guard )?)),
            }
        }
    }

    macro_rules! range_test {
        ($s:expr, Err) => {
            assert_range_matches!($s, Err(_))
        };
        ($s:expr, $start:expr, None) => {
            assert_range_matches!(
                $s,
                Ok(Range {
                    start: $start,
                    count: None
                })
            );
        };
        ($s:expr, $start:expr, $count:expr) => {
            assert_range_matches!(
                $s,
                Ok(Range {
                    start: $start,
                    count: Some($count)
                })
            );
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

        range_test!("asdf", Err);
        range_test!("1-0xabcR", Err);
        range_test!("10", Err);
    }
}
