use std::convert::TryInto;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;

mod range;
use range::Range;

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

trait ReadExt: Read {
    /// Read and discard the first `count` bytes from this reader.
    ///
    /// This is equivalent to a forward seek, but works on pipes and other un-seekable readers.
    fn skip(&mut self, count: u64) -> io::Result<()>;

    /// Maybe limit how many bytes we'll read from this reader, always return a Box
    fn take_dyn(self, count: Option<u64>) -> Box<dyn Read>;
}

impl<R: Read + 'static> ReadExt for R {
    fn skip(&mut self, count: u64) -> io::Result<()> {
        if count == 0 {
            return Ok(());
        }
        let mut t = self.take(count);
        io_copy(&mut t, &mut io::sink())?;
        Ok(())
    }

    fn take_dyn(self, count: Option<u64>) -> Box<dyn Read> {
        match count {
            Some(count) => Box::new(self.take(count)),
            None => Box::new(self),
        }
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
        Some(path) => matches!(path.to_str(), Some("-")),
        None => true,
    };

    // On Linux, treat everything including stdin as a File so that we bypass std's buffering
    #[cfg(target_os = "linux")]
    {
        use rustix::{
            fs::{SeekFrom, seek},
            io::{Errno, dup},
            stdio::stdin,
        };

        let mut file =
            if is_stdin { File::from(dup(stdin())?) } else { File::open(path.as_ref().unwrap())? };

        // seek forward into the input if needed
        if range.start != 0 {
            match seek(&file, SeekFrom::Current(range.start.try_into().unwrap())) {
                Ok(_) => (),
                // failed because this is a pipe, do a skip read instead
                Err(Errno::SPIPE) => file.skip(range.start)?,
                // some other error, bail
                Err(e) => return Err(e.into()),
            }
        }

        Ok(file.take_dyn(range.count))
    }

    // Otherwise, continue treating stdin as a special snowflake
    #[cfg(not(target_os = "linux"))]
    {
        use std::io::{Seek, SeekFrom};

        if is_stdin {
            let mut stdin = io::stdin().lock();
            stdin.skip(range.start)?;
            Ok(stdin.take_dyn(range.count))
        } else {
            let mut file = File::open(path.as_ref().unwrap())?;
            // TODO: can this be combined with the linux implemenation? Maybe in ReadExt::skip?
            if range.start != 0 {
                match file.seek(SeekFrom::Current(range.start.try_into().unwrap())) {
                    Ok(_) => (),
                    Err(_) => file.skip(range.start)?,
                }
            }
            Ok(file.take_dyn(range.count))
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
        Ok(Box::new(File::from(rustix::io::dup(rustix::stdio::stdout())?)))
    }
    #[cfg(not(unix))]
    {
        Ok(Box::new(io::stdout()))
    }
}

fn run() -> Result<()> {
    let args = Args::parse();
    // parse range manually so we can control the error message rather than letting clap do it
    let range: Range = args.range.parse().context("range parse error")?;

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
