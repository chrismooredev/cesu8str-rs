use std::borrow::Cow;
use std::ffi::{OsString, OsStr};
use std::fs::File;
use std::io::{ErrorKind, Read, Write};

use clap::Parser;

use cesu8str::{Cesu8Str, Variant, Mutf8Str};

const HELP_TEXT: &str = "Converts files or standard IO streams between standard UTF8 and CESU8, or the JVM's modified UTF-8.
Note that the default Windows' console does not support non-UTF8 sequences - attempting to type/print them will result in an error.

This tool will immediately exit upon finding an invalid character sequence.

EXIT CODES:
0 - if completed normally
1 - if an IO error has occured
2 - if an encoding error has occured (invalid/incomplete character sequences/etc)
";

#[derive(Debug, clap::Parser)]
#[clap(version = env!("CARGO_PKG_VERSION"), author = env!("CARGO_PKG_AUTHORS"), about = HELP_TEXT)]
struct Opts {
    /// Toggles the use of the JVM's modified UTF8. In effect, it (en|de)codes nul-bytes as 0xC0,0x80 while nul-bytes are left alone in normal mode.
    #[clap(short, long)]
    java: bool,
    /// Decodes CESU8 text into standard UTF8. By default, this tool encodes UTF8 to CESU8.
    #[clap(short, long)]
    decode: bool,
    /// The input file. Defaults to stdin if '-' or not set
    #[clap(short, long)]
    input: Option<OsString>,
    /// The output file. Defaults to stdout if '-' or not set
    #[clap(short, long)]
    output: Option<OsString>,
    // #[clap(short, long, default_value = "4096")]
    // chunk: usize,
}

// const BUFSIZE: usize = 1*1024; // 4KiB
const EXITCODE_SUCCESS: i32 = 0;
const EXITCODE_ERROR_IO: i32 = 1;
const EXITCODE_ERROR_ENCODING: i32 = 2;

fn main() {
    let exit_code = real_main();
    std::process::exit(exit_code)
}

fn real_main() -> i32 {
    let opts = Opts::parse();

    // take input in a loop, output it
    // if either input or output is closed unexpectedly, exit gracefully (instead of panickreturning a pipe error or something)

    let h_stdin = std::io::stdin();
    let h_stdout = std::io::stdout();

    let mut input: Box<dyn Read> = match opts.input {
        None => Box::new(h_stdin.lock()),
        Some(f) if f == "-" => Box::new(h_stdin.lock()),
        Some(file) => {
            // we are a custom file, also not "-"
            let file = match File::open(file) {
                Ok(f) => f,
                Err(e) => {
                    eprintln!("error while opening input file for reading:");
                    eprintln!("{:?}", e);
                    return EXITCODE_ERROR_IO;
                }
            };
            Box::new(file)
        }
    };

    let mut output: Box<dyn Write> = match opts.output.as_ref() {
        None => Box::new(h_stdout.lock()),
        Some(f) if f == "-" => Box::new(h_stdout.lock()),
        Some(file) => {
            // we are a custom file, also not "-"
            let file = match File::create(file) {
                Ok(f) => f,
                Err(e) => {
                    eprintln!("error while creating output file for writing:");
                    eprintln!("{:?}", e);
                    return EXITCODE_ERROR_IO;
                }
            };
            Box::new(file)
        }
    };

    let variant = match opts.java {
        false => Variant::Standard,
        true => Variant::Java,
    };

    read_write_loop(
        4096, /* opts.chunk */
        !opts.decode,
        variant,
        input,
        output,
    )
}

fn read_write_loop(
    buf_size: usize,
    encode: bool,
    variant: Variant,
    input: Box<dyn Read>,
    output: Box<dyn Write>,
) -> i32 {
    let debug_output = std::env::var("CESU_DEBUG").is_ok();

    macro_rules! debugln {
        ($fmt: literal $(, $rest:expr)*) => {
            if debug_output {
                eprintln!(concat!("[", file!(), ":", stringify!(line!()), "] ", $fmt) $(, $rest)*);
            }
        }
    }

    // our raw buffer
    let mut buf_vec = vec![0u8; buf_size];
    let buf = Box::new(buf_vec.as_mut_slice());
    let mut absolute_start = 0;
    let mut start = 0;
    let mut end = 0;

    // let mut input_done = false;
    let mut io_error = false;
    let mut bad_encoding = false;
    let mut oinput = Option::Some(input);
    let mut ooutput = Option::Some(output); // wrap in option to drop on close

    loop {
        // move existing buffer to start of slice
        if start != 0 {
            debugln!(
                "moving buffer from {:?} to {:?}",
                start..end,
                0..(end - start)
            );

            buf.copy_within(start..end, 0);
            end -= start;
            start = 0;
        }
        if oinput.is_none() && end == 0 {
            // we are done with input, and we have no more data to process
            break;
        }

        // if we have over 3/4s of a buffer to fill, attempt to fill the buffer
        if let Some(input) = oinput.as_mut() {
            if end <= buf.len() / 4 {
                // this can block - that is ok
                // any output we possibly could have given, we have already done so
                // if we're not waiting to output - we are only waiting for input
                match input.read(&mut buf[end..]) {
                    Ok(0) => {
                        oinput.take().expect("attempt to take already taken stdin");
                    }
                    Ok(n) => {
                        debugln!("read input; end += n, ({} += {}) = {}", end, n, end + n);
                        debug_assert!(end + n <= buf.len(), "read more than the buffer can hold");
                        end += n;
                    }
                    Err(e) if e.kind() == ErrorKind::BrokenPipe => {
                        // BrokenPipe is "expected", just treat as EOF
                        oinput.take().expect("attempt to take already taken stdin");
                    }
                    Err(e) => {
                        debugln!("input error: {}", e);
                        io_error = true;
                        oinput.take().expect("attempt to take already taken stdin");
                    }
                }
            }
        }

        // if we have data
        if end > 0 {
            let (data, err) = if encode {
                let (s, err) = match std::str::from_utf8(&buf[..end]) {
                    Ok(s) => (s, None),
                    Err(e) => {
                        let s = unsafe { std::str::from_utf8_unchecked(&buf[..e.valid_up_to()]) };
                        (s, Some((e.valid_up_to(), e.error_len())))
                    }
                };
                let bytes = match variant {
                    Variant::Standard => Cesu8Str::from_utf8(s).as_bytes().to_owned(),
                    Variant::Java => Mutf8Str::from_utf8(s).as_bytes().to_owned()
                };
                (bytes, err)
            } else {
                let res = match variant {
                    Variant::Standard => Cesu8Str::try_from_bytes(&buf[..end]).map(|b| b.to_str()),
                    Variant::Java => Mutf8Str::try_from_bytes(&buf[..end]).map(|b| b.to_str()),
                };
                // let res = Cesu8Str::from_cesu8(&buf[..end], variant);
                // debugln!("from_cesu8 = {:?}", res);
                let (s, err) = match res {
                    Ok(s) => (s, None),
                    Err(e) => {
                        // for various reasons, there is no from_cesu8_unchecked
                        let s = match variant {
                            Variant::Standard => unsafe { Cesu8Str::from_bytes_unchecked(&buf[..e.valid_up_to()]).to_str() },
                            Variant::Java => unsafe { Mutf8Str::from_bytes_unchecked(&buf[..e.valid_up_to()]).to_str() },
                        };
                        // debugln!("valid_cesu8 = (len = {} = 0x{:X}) {:?}", s.as_bytes().len(), s.as_bytes().len(), s);
                        (s, Some((e.valid_up_to(), e.error_len().map(|n| n.get() as usize))))
                    }
                };
                (s.into_owned().into_bytes(), err)
            };

            // if let Some(e) = err {
            //     debugln!("error on absolute index {} (or 0x{:X}), error_len = {:?}", absolute_start + e.0, absolute_start + e.0, e.1);
            // }

            match err {
                None => {
                    // we've consumed all of it
                    debugln!(
                        "no error on chunk 0x{:x}..0x{:x}",
                        absolute_start,
                        absolute_start + end
                    );
                    start = end;
                    absolute_start += end;
                }
                Some((n, None)) => {
                    // there was an error that could be solved with more data
                    // "consume" n bytes

                    debugln!(
                        "need more bytes to continue from 0x{:x} (0x{:x} left)",
                        absolute_start + n,
                        end - n
                    );

                    start = n;
                    absolute_start += n;

                    if oinput.is_none() {
                        eprintln!("encoding error: input truncated");
                        bad_encoding = true;
                        break;
                    }
                }
                Some((n, Some(el))) => {
                    // there was an error in the byte encoding

                    let src = if !encode { "utf-8" } else { "cesu-8" };
                    eprintln!(
                        "input error: invalid source {} sequence of {} bytes from index {} (or 0x{:X}) (&data[{}..{}+6] = {:?})",
                        src,
                        el,
                        absolute_start + n,
                        absolute_start + n,
                        absolute_start + n,
                        absolute_start + n,
                        &data[n..data.len().min(n+6)]
                    );
                    debugln!(
                        "-> abs_start+n  ::  {}+{}={} :: 0x{:X}+0x{:X}=0x{:X}",
                        absolute_start,
                        n,
                        absolute_start + n,
                        absolute_start,
                        n,
                        absolute_start + n
                    );
                    debugln!("-> (start..end) = {:?}", start..end);
                    let portion = &buf[n.saturating_sub(4)..(n + el + 4).min(end)];
                    debugln!("-> erroring portion (4 byte context): {:X?}", portion);

                    start = n;
                    absolute_start += n;

                    end = n; // skip past erroring data

                    bad_encoding = true;
                    oinput.take(); // it is ok if the input closed beforehand
                }
            }

            debugln!("writing out {} bytes = {:?}", data.len(), data);
            if let Some(output) = ooutput.as_mut() {
                match output.write_all(&data) {
                    Ok(()) => {
                        // dbg!(output.flush()); // best effort - ignore error
                    }
                    Err(e) if e.kind() == ErrorKind::BrokenPipe => {
                        // stop procesing - we can't do any more
                        ooutput.take().expect("attempt to take empty ooutput");
                        break;
                    }
                    Err(e) => {
                        eprintln!("output error: {}", e);
                        io_error = true;
                        break;
                    }
                }
            }
        }
    }

    if io_error {
        EXITCODE_ERROR_IO
    } else if bad_encoding {
        EXITCODE_ERROR_ENCODING
    } else {
        EXITCODE_SUCCESS
    }
}
