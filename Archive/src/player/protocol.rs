use std::fmt;

use serde::{Serialize, ser::SerializeStruct as _};
use serde_json::{Map, Value};
use thiserror::Error;

pub const MPV_MAX_REQUEST_ID: u64 = i64::MAX as u64;

#[derive(Clone, PartialEq)]
pub struct MpvRequest {
    command: Vec<Value>,
    request_id: u64,
}

impl MpvRequest {
    /// Creates a replacing `loadfile` request.
    ///
    /// # Errors
    ///
    /// Returns [`MpvRequestError::RequestIdOutOfRange`] when the ID does not
    /// fit mpv's signed 64-bit integer representation.
    pub fn loadfile(request_id: u64, url: &str) -> Result<Self, MpvRequestError> {
        Self::new(
            request_id,
            vec![
                Value::String("loadfile".to_owned()),
                Value::String(url.to_owned()),
                Value::String("replace".to_owned()),
            ],
        )
    }

    /// Creates a structured property assignment request.
    ///
    /// # Errors
    ///
    /// Returns [`MpvRequestError::RequestIdOutOfRange`] when the ID does not
    /// fit mpv's signed 64-bit integer representation.
    pub fn set_property(
        request_id: u64,
        property: &str,
        value: Value,
    ) -> Result<Self, MpvRequestError> {
        Self::new(
            request_id,
            vec![
                Value::String("set_property".to_owned()),
                Value::String(property.to_owned()),
                value,
            ],
        )
    }

    /// Creates a relative seek request.
    ///
    /// # Errors
    ///
    /// Returns [`MpvRequestError::NonFiniteNumber`] when `seconds` is NaN or
    /// infinite, or [`MpvRequestError::RequestIdOutOfRange`] when the ID does
    /// not fit mpv's signed 64-bit integer representation.
    pub fn seek_relative(request_id: u64, seconds: f64) -> Result<Self, MpvRequestError> {
        let seconds = finite_number(seconds)?;
        Self::new(
            request_id,
            vec![
                Value::String("seek".to_owned()),
                Value::Number(seconds),
                Value::String("relative".to_owned()),
            ],
        )
    }

    /// Creates a volume property request.
    ///
    /// # Errors
    ///
    /// Returns [`MpvRequestError::NonFiniteNumber`] when `volume` is NaN or
    /// infinite, or [`MpvRequestError::RequestIdOutOfRange`] when the ID does
    /// not fit mpv's signed 64-bit integer representation.
    pub fn set_volume(request_id: u64, volume: f64) -> Result<Self, MpvRequestError> {
        let volume = finite_number(volume)?;
        Self::set_property(request_id, "volume", Value::Number(volume))
    }

    /// Creates a property observation request.
    ///
    /// # Errors
    ///
    /// Returns [`MpvRequestError::RequestIdOutOfRange`] when the ID does not
    /// fit mpv's signed 64-bit integer representation.
    pub fn observe_property(
        request_id: u64,
        observer_id: u64,
        property: &str,
    ) -> Result<Self, MpvRequestError> {
        Self::new(
            request_id,
            vec![
                Value::String("observe_property".to_owned()),
                Value::from(observer_id),
                Value::String(property.to_owned()),
            ],
        )
    }

    /// Creates a quit request.
    ///
    /// # Errors
    ///
    /// Returns [`MpvRequestError::RequestIdOutOfRange`] when the ID does not
    /// fit mpv's signed 64-bit integer representation.
    pub fn quit(request_id: u64) -> Result<Self, MpvRequestError> {
        Self::new(request_id, vec![Value::String("quit".to_owned())])
    }

    #[must_use]
    pub const fn request_id(&self) -> u64 {
        self.request_id
    }

    #[must_use]
    pub fn command(&self) -> &[Value] {
        &self.command
    }

    /// Serializes this request as one newline-delimited JSON frame.
    ///
    /// # Errors
    ///
    /// Returns an encoding error if serialization unexpectedly fails.
    pub fn to_json_line(&self) -> Result<String, MpvEncodeError> {
        let mut encoded = serde_json::to_string(self).map_err(MpvEncodeError::from)?;
        encoded.push('\n');
        Ok(encoded)
    }

    fn new(request_id: u64, command: Vec<Value>) -> Result<Self, MpvRequestError> {
        validate_request_id(request_id)?;
        Ok(Self {
            command,
            request_id,
        })
    }
}

impl Serialize for MpvRequest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        validate_request_id(self.request_id).map_err(serde::ser::Error::custom)?;
        let mut request = serializer.serialize_struct("MpvRequest", 2)?;
        request.serialize_field("command", &self.command)?;
        request.serialize_field("request_id", &self.request_id)?;
        request.end()
    }
}

impl fmt::Debug for MpvRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MpvRequest")
            .field("request_id", &self.request_id)
            .field("argument_count", &self.command.len())
            .finish()
    }
}

fn finite_number(value: f64) -> Result<serde_json::Number, MpvRequestError> {
    serde_json::Number::from_f64(value).ok_or(MpvRequestError::NonFiniteNumber)
}

fn validate_request_id(request_id: u64) -> Result<(), MpvRequestError> {
    if request_id <= MPV_MAX_REQUEST_ID {
        Ok(())
    } else {
        Err(MpvRequestError::RequestIdOutOfRange)
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum MpvRequestError {
    #[error("mpv command numbers must be finite")]
    NonFiniteNumber,
    #[error("mpv request IDs must fit its signed 64-bit integer range")]
    RequestIdOutOfRange,
}

#[derive(Debug, Error)]
#[error("could not encode an mpv request")]
pub struct MpvEncodeError {
    #[source]
    source: serde_json::Error,
}

impl From<serde_json::Error> for MpvEncodeError {
    fn from(source: serde_json::Error) -> Self {
        Self { source }
    }
}

#[derive(Clone, PartialEq)]
pub struct MpvReply {
    request_id: u64,
    error: String,
    data: Option<Value>,
}

impl MpvReply {
    #[must_use]
    pub const fn request_id(&self) -> u64 {
        self.request_id
    }

    #[must_use]
    pub fn error(&self) -> &str {
        &self.error
    }

    #[must_use]
    pub const fn data(&self) -> Option<&Value> {
        self.data.as_ref()
    }
}

impl fmt::Debug for MpvReply {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MpvReply")
            .field("request_id", &self.request_id)
            .field("error", &self.error)
            .field("has_data", &self.data.is_some())
            .finish()
    }
}

#[derive(Clone, PartialEq)]
pub enum MpvEvent {
    StartFile,
    PropertyChange {
        observer_id: u64,
        name: String,
        data: Value,
    },
    FileLoaded,
    EndFile {
        reason: Option<String>,
        error: Option<String>,
    },
    Shutdown,
    Unknown {
        name: String,
    },
}

impl fmt::Debug for MpvEvent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StartFile => formatter.write_str("StartFile"),
            Self::PropertyChange {
                observer_id, name, ..
            } => formatter
                .debug_struct("PropertyChange")
                .field("observer_id", observer_id)
                .field("name", name)
                .field("data", &"<redacted>")
                .finish(),
            Self::FileLoaded => formatter.write_str("FileLoaded"),
            Self::EndFile { reason, error } => formatter
                .debug_struct("EndFile")
                .field("reason", reason)
                .field("has_error", &error.is_some())
                .finish(),
            Self::Shutdown => formatter.write_str("Shutdown"),
            Self::Unknown { name } => formatter.debug_tuple("Unknown").field(name).finish(),
        }
    }
}

#[derive(Clone, PartialEq)]
pub enum MpvMessage {
    Reply(MpvReply),
    Event(MpvEvent),
}

impl fmt::Debug for MpvMessage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Reply(reply) => formatter.debug_tuple("Reply").field(reply).finish(),
            Self::Event(event) => formatter.debug_tuple("Event").field(event).finish(),
        }
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum ProtocolDiagnostic {
    #[error("mpv sent invalid JSON")]
    InvalidJson,
    #[error("mpv sent an invalid {context} message")]
    InvalidShape { context: &'static str },
    #[error("mpv frame exceeded the configured {max_bytes}-byte limit")]
    Oversized { max_bytes: usize },
    #[error("mpv closed a partial frame after {bytes_read} bytes")]
    UnexpectedEof { bytes_read: usize },
}

/// Decodes one complete mpv JSON line.
///
/// Unknown event names are retained only by name and remain valid events.
///
/// # Errors
///
/// Returns a secret-safe typed diagnostic for malformed JSON or invalid known
/// message shapes.
pub fn decode_line(line: &[u8]) -> Result<MpvMessage, ProtocolDiagnostic> {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let value: Value = serde_json::from_slice(line).map_err(|_| ProtocolDiagnostic::InvalidJson)?;
    let object = value
        .as_object()
        .ok_or(ProtocolDiagnostic::InvalidShape { context: "root" })?;

    if object.contains_key("event") {
        decode_event(object).map(MpvMessage::Event)
    } else {
        decode_reply(object).map(MpvMessage::Reply)
    }
}

fn decode_reply(object: &Map<String, Value>) -> Result<MpvReply, ProtocolDiagnostic> {
    let request_id = object
        .get("request_id")
        .and_then(Value::as_u64)
        .ok_or(ProtocolDiagnostic::InvalidShape { context: "reply" })?;
    let error = object
        .get("error")
        .and_then(Value::as_str)
        .ok_or(ProtocolDiagnostic::InvalidShape { context: "reply" })?;

    Ok(MpvReply {
        request_id,
        error: error.to_owned(),
        data: object.get("data").cloned(),
    })
}

fn decode_event(object: &Map<String, Value>) -> Result<MpvEvent, ProtocolDiagnostic> {
    let name = object
        .get("event")
        .and_then(Value::as_str)
        .ok_or(ProtocolDiagnostic::InvalidShape { context: "event" })?;

    match name {
        "start-file" => Ok(MpvEvent::StartFile),
        "property-change" => {
            let observer_id = object.get("id").and_then(Value::as_u64).ok_or(
                ProtocolDiagnostic::InvalidShape {
                    context: "property-change",
                },
            )?;
            let property_name = object.get("name").and_then(Value::as_str).ok_or(
                ProtocolDiagnostic::InvalidShape {
                    context: "property-change",
                },
            )?;
            let data = object
                .get("data")
                .cloned()
                .ok_or(ProtocolDiagnostic::InvalidShape {
                    context: "property-change",
                })?;
            Ok(MpvEvent::PropertyChange {
                observer_id,
                name: property_name.to_owned(),
                data,
            })
        }
        "file-loaded" => Ok(MpvEvent::FileLoaded),
        "end-file" => {
            let error = match optional_string(object, "file_error", "end-file")? {
                Some(error) => Some(error),
                None => optional_string(object, "error", "end-file")?,
            };
            Ok(MpvEvent::EndFile {
                reason: optional_string(object, "reason", "end-file")?,
                error,
            })
        }
        "shutdown" => Ok(MpvEvent::Shutdown),
        unknown => Ok(MpvEvent::Unknown {
            name: unknown.to_owned(),
        }),
    }
}

fn optional_string(
    object: &Map<String, Value>,
    field: &str,
    context: &'static str,
) -> Result<Option<String>, ProtocolDiagnostic> {
    object
        .get(field)
        .map(|value| {
            value
                .as_str()
                .map(ToOwned::to_owned)
                .ok_or(ProtocolDiagnostic::InvalidShape { context })
        })
        .transpose()
}

#[derive(Debug, Eq, PartialEq)]
pub struct RequestIdAllocator {
    next: Option<u64>,
}

impl Default for RequestIdAllocator {
    fn default() -> Self {
        Self { next: Some(1) }
    }
}

impl RequestIdAllocator {
    /// Creates an allocator whose first result is `next`.
    ///
    /// # Errors
    ///
    /// Returns [`RequestIdError::OutOfRange`] when `next` does not fit mpv's
    /// signed 64-bit integer representation.
    pub const fn starting_at(next: u64) -> Result<Self, RequestIdError> {
        if next <= MPV_MAX_REQUEST_ID {
            Ok(Self { next: Some(next) })
        } else {
            Err(RequestIdError::OutOfRange)
        }
    }

    /// Allocates the next request ID without wrapping.
    ///
    /// # Errors
    ///
    /// Returns [`RequestIdError::Exhausted`] after [`MPV_MAX_REQUEST_ID`] has
    /// been issued.
    pub fn allocate(&mut self) -> Result<u64, RequestIdError> {
        let current = self.next.ok_or(RequestIdError::Exhausted)?;
        self.next = if current == MPV_MAX_REQUEST_ID {
            None
        } else {
            Some(current + 1)
        };
        Ok(current)
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum RequestIdError {
    #[error("mpv request ID space is exhausted")]
    Exhausted,
    #[error("mpv request IDs must fit its signed 64-bit integer range")]
    OutOfRange,
}
