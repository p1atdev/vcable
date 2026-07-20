use std::env;
use std::error::Error;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use vcable_protocol::{Request, Response, read_message, write_message};

fn main() {
    if let Err(error) = run() {
        eprintln!("vcablectl: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let mut args = env::args().skip(1);
    let Some(flag) = args.next() else {
        return Err(usage().into());
    };
    if flag == "--help" || flag == "-h" {
        println!("{}", usage());
        return Ok(());
    }
    if flag != "--socket" {
        return Err("the first argument must be --socket PATH".into());
    }
    let socket = PathBuf::from(args.next().ok_or("--socket requires a path")?);
    let command = args.next().ok_or_else(usage)?;
    let request = match command.as_str() {
        "ping" => Request::Ping,
        "status" => Request::Status,
        "shutdown" => Request::Shutdown,
        "create" => Request::CreateDevice {
            id: required(&mut args, "device id")?,
            name: required(&mut args, "device name")?,
            input_channels: parse(&mut args, "input channels")?,
            output_channels: parse(&mut args, "output channels")?,
            sample_rate: parse(&mut args, "sample rate")?,
        },
        "delete" => Request::DeleteDevice {
            id: required(&mut args, "device id")?,
        },
        "connect" => {
            let id = required(&mut args, "route id")?;
            let source = required(&mut args, "source endpoint")?;
            let sink = required(&mut args, "sink endpoint")?;
            let gain_millidb = parse(&mut args, "gain in 0.001 dB")?;
            let matrix_millionths = args
                .next()
                .map(|matrix| {
                    matrix
                        .split(',')
                        .map(|value| {
                            value
                                .parse::<i32>()
                                .map_err(|_| format!("invalid matrix coefficient: {value}"))
                        })
                        .collect::<Result<Vec<_>, _>>()
                })
                .transpose()?
                .unwrap_or_default();
            Request::Connect {
                id,
                source,
                sink,
                gain_millidb,
                matrix_millionths,
            }
        }
        "disconnect" => Request::Disconnect {
            id: required(&mut args, "route id")?,
        },
        _ => return Err(format!("unknown command: {command}\n{}", usage()).into()),
    };
    if let Some(extra) = args.next() {
        return Err(format!("unexpected argument: {extra}").into());
    }

    let mut stream = UnixStream::connect(socket)?;
    write_message(&mut stream, &request)?;
    match read_message::<_, Response>(&mut stream)? {
        Response::Ok(message) => {
            println!("{message}");
            Ok(())
        }
        Response::Error { code, message } => Err(format!("{code}: {message}").into()),
    }
}

fn required(args: &mut impl Iterator<Item = String>, name: &str) -> Result<String, Box<dyn Error>> {
    args.next().ok_or_else(|| format!("missing {name}").into())
}

fn parse<T: std::str::FromStr>(
    args: &mut impl Iterator<Item = String>,
    name: &str,
) -> Result<T, Box<dyn Error>> {
    let value = required(args, name)?;
    value
        .parse()
        .map_err(|_| format!("invalid {name}: {value}").into())
}

fn usage() -> String {
    [
        "usage:",
        "  vcablectl --socket PATH ping",
        "  vcablectl --socket PATH status",
        "  vcablectl --socket PATH shutdown",
        "  vcablectl --socket PATH create ID NAME INPUTS OUTPUTS RATE",
        "  vcablectl --socket PATH delete ID",
        "  vcablectl --socket PATH connect ID SOURCE SINK GAIN_MILLIDB [MATRIX_MILLIONTHS]",
        "  vcablectl --socket PATH disconnect ID",
    ]
    .join("\n")
}
