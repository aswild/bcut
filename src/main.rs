use std::convert::TryInto;
use std::fs::File;
use std::io::prelude::*;
use std::io::{self, SeekFrom};
use std::path::PathBuf;
use std::str::FromStr;

use anyhow::{anyhow, ensure, Context, Result};
use clap::Parser;
use regex::Regex;

/// Slice a byte range from a file
#[derive(Debug, Parser)]
#[clap(version)]
struct Args {
    /// Output file, omit or use "-" for stdout
    #[arg(short, long, name = "OUTFILE")]
    output: Option<PathBuf>,

    /// Hexdump the output
    #[arg(short = 'H', long)]
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
    #[arg(value_name = "RANGE", verbatim_doc_comment)]
    range: String,

    /// Input file, omit or use "-" for stdin
    #[arg(value_name = "FILE")]
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

/// This behaves the same as [`std::io::copy`] but much faster for large inputs. We lose the
/// Linux-specific sendfile/splice optimizations, but it seems like those don't get used by bcut
/// anyway and it falls back to stack_buffer_copy with an 8K IO buffer. Increasing that buffer size
/// to 1M gives nearly 3X speedup when copying large (multi-gigabyte) files on my machine.
fn io_copy<R: Read, W: Write>(reader: &mut R, writer: &mut W) -> io::Result<u64> {
    const BUF_SIZE: usize = 1024 * 1024;
    let mut buf = vec![0u8; BUF_SIZE];
    let mut total = 0;

    loop {
        let count = match reader.read(&mut buf[..]) {
            Ok(0) => break,
            Ok(count) => count,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        writer.write_all(&buf[..count])?;
        total += count as u64;
    }
    Ok(total)
}

fn prepare_input(path: &Option<PathBuf>, range: &Range) -> io::Result<Box<dyn Read>> {
    let is_stdin = match path {
        Some(ref path) => matches!(path.to_str(), Some("-")),
        None => true,
    };

    #[cfg(unix)]
    {
        // treat everything including stdin as a File so that we bypass std's buffering
        use std::os::unix::io::FromRawFd;
        let mut file = if is_stdin {
            // SAFETY: duplicating the file descriptor can't cause memory unsafety, and we get a new fd
            // which is owned by the file and will be closed on drop.
            unsafe {
                let fd = libc::dup(libc::STDIN_FILENO);
                if fd == -1 {
                    return Err(io::Error::last_os_error());
                }
                File::from_raw_fd(fd)
            }
        } else {
            File::open(path.as_ref().unwrap())?
        };

        // seek forward into the input if needed
        if range.start != 0 {
            match file.seek(SeekFrom::Current(range.start.try_into().unwrap())) {
                Ok(_) => (),
                Err(e) if e.raw_os_error() == Some(libc::ESPIPE) => {
                    // Failed to seek because this File is a pipe, so just read the first N bytes and
                    // throw them away.
                    let mut t = (&mut file).take(range.start);
                    io_copy(&mut t, &mut io::sink())?;
                }
                Err(e) => return Err(e),
            }
        }

        match range.count {
            Some(count) => Ok(Box::new(file.take(count))),
            None => Ok(Box::new(file)),
        }
    }

    #[cfg(not(unix))]
    {
        if is_stdin {
            let mut stdin = io::stdin();
            if range.start != 0 {
                let mut t = stdin.lock().take(range.start);
                io_copy(&mut t, &mut io::sink())?;
            }
            Ok(Box::new(stdin))
        } else {
            let mut file = File::open(path.as_ref().unwrap())?;
            if range.start != 0 {
                match file.seek(SeekFrom::Current(range.start.try_into().unwrap())) {
                    Ok(_) => (),
                    Err(e) => {
                        // failed to seek, probably a pipe? Not sure about Windows semantics...
                        let mut t = (&mut file).take(range.start);
                        io_copy(&mut t, &mut io::sink())?;
                    }
                }
            }
            Ok(Box::new(file))
        }
    }
}

/// Get a writer for stdout, making it unbuffered when possible on unix. std::io::Stdout is always
/// line-buffered, which wastes time on memchr looking for line endings when we're dumping lots of
/// binary data.
///
/// Note: this opens a new file descriptor for stdout which bypasses the standard library's
/// buffering and locking. Continuing to use println!() and io::stdout() won't cause safety issues,
/// but could result in unexpected jumbled results on stdout if writes between this object and
/// std's Stdout are interleaved without force-flushing.
fn open_stdout() -> io::Result<Box<dyn Write>> {
    #[cfg(unix)]
    {
        use std::os::unix::io::FromRawFd;

        // SAFETY: we create a new file descriptor with dup and transfer ownership of it to the
        // returned File object, which will close that fd when it's dropped.
        unsafe {
            let fd = libc::dup(libc::STDOUT_FILENO);
            if fd == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(Box::new(File::from_raw_fd(fd)))
        }
    }
    #[cfg(not(unix))]
    {
        Box::new(io::stdout())
    }
}

fn run() -> Result<()> {
    let args = Args::parse();
    // parse range manually so we can control the error message rather than letting clap do it
    let range: Range = args.range.parse()?;

    let mut input = prepare_input(&args.input, &range).context("failed to open input")?;

    let mut output: Box<dyn Write> = match &args.output {
        None => open_stdout().context("failed to open stdout")?,
        Some(p) if p.to_str() == Some("-") => open_stdout().context("failed to open stdout")?,
        Some(path) => Box::new(File::create(path).context("failed to open output file")?),
    };

    if args.hexdump {
        let mut printer = hexyl::PrinterBuilder::new(output).build();
        printer.print_all(&mut input)?;
    } else {
        io_copy(&mut input, &mut output)?;
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
