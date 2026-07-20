//! Length-prefixed, dependency-free control protocol shared by `vcabled` and
//! `vcablectl`.

use std::error::Error;
use std::fmt;
use std::io::{self, Read, Write};

pub const MAX_MESSAGE_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Request {
    Ping,
    Status,
    Shutdown,
    CreateDevice {
        id: String,
        name: String,
        input_channels: u32,
        output_channels: u32,
        sample_rate: u32,
    },
    DeleteDevice {
        id: String,
    },
    Connect {
        id: String,
        source: String,
        sink: String,
        gain_millidb: i32,
        matrix_millionths: Vec<i32>,
    },
    Disconnect {
        id: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Response {
    Ok(String),
    Error { code: String, message: String },
}

#[derive(Debug)]
pub enum ProtocolError {
    Io(io::Error),
    MessageTooLarge(usize),
    Truncated,
    InvalidUtf8,
    InvalidMessage(String),
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => error.fmt(f),
            Self::MessageTooLarge(size) => write!(f, "message is too large: {size} bytes"),
            Self::Truncated => write!(f, "message is truncated"),
            Self::InvalidUtf8 => write!(f, "message is not valid UTF-8"),
            Self::InvalidMessage(message) => write!(f, "invalid message: {message}"),
        }
    }
}

impl Error for ProtocolError {}

impl From<io::Error> for ProtocolError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

pub trait Message: Sized {
    fn encode(&self) -> String;
    fn decode(value: &str) -> Result<Self, ProtocolError>;
}

pub fn write_message<W: Write, M: Message>(
    writer: &mut W,
    message: &M,
) -> Result<(), ProtocolError> {
    let body = message.encode();
    if body.len() > MAX_MESSAGE_BYTES {
        return Err(ProtocolError::MessageTooLarge(body.len()));
    }
    let length = u32::try_from(body.len())
        .map_err(|_| ProtocolError::MessageTooLarge(body.len()))?
        .to_be_bytes();
    writer.write_all(&length)?;
    writer.write_all(body.as_bytes())?;
    Ok(())
}

pub fn read_message<R: Read, M: Message>(reader: &mut R) -> Result<M, ProtocolError> {
    let mut length = [0_u8; 4];
    reader.read_exact(&mut length).map_err(|error| {
        if error.kind() == io::ErrorKind::UnexpectedEof {
            ProtocolError::Truncated
        } else {
            ProtocolError::Io(error)
        }
    })?;
    let length = u32::from_be_bytes(length) as usize;
    if length > MAX_MESSAGE_BYTES {
        return Err(ProtocolError::MessageTooLarge(length));
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).map_err(|error| {
        if error.kind() == io::ErrorKind::UnexpectedEof {
            ProtocolError::Truncated
        } else {
            ProtocolError::Io(error)
        }
    })?;
    let body = String::from_utf8(body).map_err(|_| ProtocolError::InvalidUtf8)?;
    M::decode(&body)
}

fn escape(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"._:-/ ".contains(&byte) {
            result.push(char::from(byte));
        } else {
            use fmt::Write as _;
            let _ = write!(result, "%{byte:02X}");
        }
    }
    result
}

fn unescape(value: &str) -> Result<String, ProtocolError> {
    let bytes = value.as_bytes();
    let mut result = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'%' {
            result.push(bytes[index]);
            index += 1;
            continue;
        }
        let digits = bytes
            .get(index + 1..index + 3)
            .ok_or_else(|| ProtocolError::InvalidMessage("incomplete escape".to_owned()))?;
        let digits = std::str::from_utf8(digits)
            .map_err(|_| ProtocolError::InvalidMessage("invalid escape".to_owned()))?;
        result.push(
            u8::from_str_radix(digits, 16)
                .map_err(|_| ProtocolError::InvalidMessage("invalid escape".to_owned()))?,
        );
        index += 3;
    }
    String::from_utf8(result).map_err(|_| ProtocolError::InvalidUtf8)
}

impl Message for Request {
    fn encode(&self) -> String {
        match self {
            Self::Ping => "ping".to_owned(),
            Self::Status => "status".to_owned(),
            Self::Shutdown => "shutdown".to_owned(),
            Self::CreateDevice {
                id,
                name,
                input_channels,
                output_channels,
                sample_rate,
            } => format!(
                "create\t{}\t{}\t{input_channels}\t{output_channels}\t{sample_rate}",
                escape(id),
                escape(name)
            ),
            Self::DeleteDevice { id } => format!("delete\t{}", escape(id)),
            Self::Connect {
                id,
                source,
                sink,
                gain_millidb,
                matrix_millionths,
            } => {
                let matrix = matrix_millionths
                    .iter()
                    .map(i32::to_string)
                    .collect::<Vec<_>>()
                    .join(",");
                format!(
                    "connect\t{}\t{}\t{}\t{gain_millidb}\t{matrix}",
                    escape(id),
                    escape(source),
                    escape(sink)
                )
            }
            Self::Disconnect { id } => format!("disconnect\t{}", escape(id)),
        }
    }

    fn decode(value: &str) -> Result<Self, ProtocolError> {
        let fields = value.split('\t').collect::<Vec<_>>();
        match fields.as_slice() {
            ["ping"] => Ok(Self::Ping),
            ["status"] => Ok(Self::Status),
            ["shutdown"] => Ok(Self::Shutdown),
            ["create", id, name, input, output, rate] => Ok(Self::CreateDevice {
                id: unescape(id)?,
                name: unescape(name)?,
                input_channels: parse(input, "input channel count")?,
                output_channels: parse(output, "output channel count")?,
                sample_rate: parse(rate, "sample rate")?,
            }),
            ["delete", id] => Ok(Self::DeleteDevice { id: unescape(id)? }),
            ["connect", id, source, sink, gain, matrix] => Ok(Self::Connect {
                id: unescape(id)?,
                source: unescape(source)?,
                sink: unescape(sink)?,
                gain_millidb: parse(gain, "gain")?,
                matrix_millionths: if matrix.is_empty() {
                    Vec::new()
                } else {
                    matrix
                        .split(',')
                        .map(|value| parse(value, "matrix coefficient"))
                        .collect::<Result<Vec<_>, _>>()?
                },
            }),
            ["disconnect", id] => Ok(Self::Disconnect { id: unescape(id)? }),
            _ => Err(ProtocolError::InvalidMessage(value.to_owned())),
        }
    }
}

impl Message for Response {
    fn encode(&self) -> String {
        match self {
            Self::Ok(message) => format!("ok\t{}", escape(message)),
            Self::Error { code, message } => {
                format!("error\t{}\t{}", escape(code), escape(message))
            }
        }
    }

    fn decode(value: &str) -> Result<Self, ProtocolError> {
        let fields = value.split('\t').collect::<Vec<_>>();
        match fields.as_slice() {
            ["ok", message] => Ok(Self::Ok(unescape(message)?)),
            ["error", code, message] => Ok(Self::Error {
                code: unescape(code)?,
                message: unescape(message)?,
            }),
            _ => Err(ProtocolError::InvalidMessage(value.to_owned())),
        }
    }
}

fn parse<T: std::str::FromStr>(value: &str, field: &str) -> Result<T, ProtocolError> {
    value
        .parse()
        .map_err(|_| ProtocolError::InvalidMessage(format!("invalid {field}: {value}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trips_through_framing() {
        let request = Request::CreateDevice {
            id: "chat-device".to_owned(),
            name: "Chat % Device\n".to_owned(),
            input_channels: 2,
            output_channels: 2,
            sample_rate: 48_000,
        };
        let mut bytes = Vec::new();
        write_message(&mut bytes, &request).unwrap();
        assert_eq!(
            read_message::<_, Request>(&mut bytes.as_slice()).unwrap(),
            request
        );
    }
}
