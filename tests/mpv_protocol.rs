use std::{
    error::Error,
    ffi::OsStr,
    io,
    path::Path,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};

use serde_json::json;
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, ReadBuf};
use ytermusic::{
    platform::{IpcEndpoint, MpvConnector, NativeMpvConnector},
    player::{
        protocol::{
            MpvEvent, MpvMessage, MpvRequest, ProtocolDiagnostic, RequestIdAllocator,
            RequestIdError, decode_line,
        },
        transport::{AsyncReadWrite, MpvTransport},
    },
};

type TestResult = Result<(), Box<dyn Error>>;

fn missing(message: &'static str) -> io::Error {
    io::Error::other(message)
}

#[test]
fn commands_are_structured_exact_json() -> TestResult {
    let cases = [
        (
            MpvRequest::loadfile(1, "https://media.invalid/private-token")?,
            json!({
                "command": ["loadfile", "https://media.invalid/private-token", "replace"],
                "request_id": 1
            }),
        ),
        (
            MpvRequest::set_property(2, "pause", json!(true))?,
            json!({"command": ["set_property", "pause", true], "request_id": 2}),
        ),
        (
            MpvRequest::seek_relative(3, -15.25)?,
            json!({"command": ["seek", -15.25, "relative"], "request_id": 3}),
        ),
        (
            MpvRequest::set_volume(4, 42.5)?,
            json!({"command": ["set_property", "volume", 42.5], "request_id": 4}),
        ),
        (
            MpvRequest::observe_property(5, 9, "time-pos")?,
            json!({"command": ["observe_property", 9, "time-pos"], "request_id": 5}),
        ),
        (
            MpvRequest::quit(6)?,
            json!({"command": ["quit"], "request_id": 6}),
        ),
    ];

    for (request, expected) in cases {
        assert_eq!(serde_json::to_value(&request)?, expected);
        let encoded = request.to_json_line()?;
        assert_eq!(encoded.as_bytes().last(), Some(&b'\n'));
        assert_eq!(encoded.bytes().filter(|byte| *byte == b'\n').count(), 1);
    }
    Ok(())
}

#[test]
fn non_finite_commands_are_rejected_before_construction() {
    assert!(MpvRequest::seek_relative(1, f64::NAN).is_err());
    assert!(MpvRequest::seek_relative(1, f64::INFINITY).is_err());
    assert!(MpvRequest::set_volume(1, f64::NEG_INFINITY).is_err());
}

#[test]
fn request_debug_never_discloses_command_arguments() -> TestResult {
    let secret = "https://media.invalid/secret-query?token=do-not-log";
    let rendered = format!("{:?}", MpvRequest::loadfile(41, secret)?);
    assert!(rendered.contains("41"));
    assert!(!rendered.contains(secret));
    assert!(!rendered.contains("do-not-log"));
    assert!(!rendered.contains("replace"));
    Ok(())
}

#[test]
fn embedded_newlines_remain_inside_one_json_frame() -> TestResult {
    let encoded = MpvRequest::loadfile(42, "https://media.invalid/a\nb")?.to_json_line()?;
    assert_eq!(encoded.bytes().filter(|byte| *byte == b'\n').count(), 1);
    assert!(encoded.ends_with('\n'));
    Ok(())
}

#[test]
fn request_ids_are_monotonic_and_exhaust_at_mpv_integer_maximum() -> TestResult {
    let mut normal = RequestIdAllocator::default();
    assert_eq!(normal.allocate(), Ok(1));
    assert_eq!(normal.allocate(), Ok(2));

    let maximum = i64::MAX as u64;
    let mut allocator = RequestIdAllocator::starting_at(maximum - 1)?;
    assert_eq!(allocator.allocate(), Ok(maximum - 1));
    assert_eq!(allocator.allocate(), Ok(maximum));
    assert_eq!(allocator.allocate(), Err(RequestIdError::Exhausted));
    assert_eq!(allocator.allocate(), Err(RequestIdError::Exhausted));
    assert_eq!(
        RequestIdAllocator::starting_at(maximum + 1),
        Err(RequestIdError::OutOfRange)
    );
    assert_eq!(
        serde_json::to_value(MpvRequest::quit(maximum)?)?,
        json!({"command": ["quit"], "request_id": maximum})
    );
    Ok(())
}

#[test]
fn every_public_request_constructor_rejects_ids_outside_mpv_range() {
    let invalid = (i64::MAX as u64) + 1;
    assert!(MpvRequest::loadfile(invalid, "https://media.invalid/a").is_err());
    assert!(MpvRequest::set_property(invalid, "pause", json!(true)).is_err());
    assert!(MpvRequest::seek_relative(invalid, 1.0).is_err());
    assert!(MpvRequest::set_volume(invalid, 20.0).is_err());
    assert!(MpvRequest::observe_property(invalid, 1, "pause").is_err());
    assert!(MpvRequest::quit(invalid).is_err());
}

#[test]
fn fixture_parses_replies_known_events_and_unknown_events() -> TestResult {
    let fixture = include_str!("fixtures/mpv_events.jsonl");
    let messages = fixture
        .lines()
        .map(|line| decode_line(line.as_bytes()))
        .collect::<Result<Vec<_>, _>>()?;

    let MpvMessage::Reply(reply) = &messages[0] else {
        panic!("first frame should be a reply");
    };
    assert_eq!(reply.request_id(), 17);
    assert_eq!(reply.error(), "success");

    assert_eq!(
        messages[1],
        MpvMessage::Event(MpvEvent::PropertyChange {
            observer_id: 4,
            name: "time-pos".to_owned(),
            data: json!(12.5),
        })
    );
    assert_eq!(messages[2], MpvMessage::Event(MpvEvent::FileLoaded));
    assert_eq!(
        messages[3],
        MpvMessage::Event(MpvEvent::EndFile {
            reason: Some("error".to_owned()),
            error: Some("invented current failure".to_owned()),
        })
    );
    assert_eq!(messages[4], MpvMessage::Event(MpvEvent::Shutdown));
    assert_eq!(
        messages[5],
        MpvMessage::Event(MpvEvent::Unknown {
            name: "future-invented-event".to_owned(),
        })
    );

    assert_eq!(
        decode_line(br#"{"event":"end-file","reason":"error","error":"legacy fallback"}"#)?,
        MpvMessage::Event(MpvEvent::EndFile {
            reason: Some("error".to_owned()),
            error: Some("legacy fallback".to_owned()),
        })
    );
    assert_eq!(
        decode_line(br#"{"event":"end-file","file_error":"preferred current error","error":99}"#)?,
        MpvMessage::Event(MpvEvent::EndFile {
            reason: None,
            error: Some("preferred current error".to_owned()),
        })
    );
    Ok(())
}

#[test]
fn start_file_is_typed_without_retaining_event_payload() -> TestResult {
    let message = decode_line(
        br#"{"event":"start-file","playlist_entry_id":7,"path":"https://media.invalid/audio?signature=do-not-log"}"#,
    )?;

    assert_eq!(message, MpvMessage::Event(MpvEvent::StartFile));
    assert!(!format!("{message:?}").contains("do-not-log"));
    assert_eq!(
        decode_line(br#"{"event":"future-start-file"}"#)?,
        MpvMessage::Event(MpvEvent::Unknown {
            name: "future-start-file".to_owned(),
        })
    );
    Ok(())
}

#[test]
fn malformed_shapes_become_secret_safe_diagnostics() -> TestResult {
    let secret = b"{\"event\":\"property-change\",\"name\":\"secret-value\"}";
    let diagnostic = decode_line(secret)
        .err()
        .ok_or_else(|| missing("missing observer id unexpectedly parsed"))?;
    assert!(matches!(
        diagnostic,
        ProtocolDiagnostic::InvalidShape { .. }
    ));
    let rendered = format!("{diagnostic:?}");
    assert!(!rendered.contains("secret-value"));

    let invalid = decode_line(b"{not json secret")
        .err()
        .ok_or_else(|| missing("invalid JSON unexpectedly parsed"))?;
    assert_eq!(invalid, ProtocolDiagnostic::InvalidJson);
    assert!(!format!("{invalid:?}").contains("secret"));
    Ok(())
}

#[tokio::test]
async fn invalid_and_oversized_frames_recover_at_the_next_newline() -> TestResult {
    let (mut peer, stream) = tokio::io::duplex(512);
    let mut transport = MpvTransport::new(Box::new(stream), 48)?;
    peer.write_all(b"{not-json}\n").await?;
    peer.write_all(b"{\"event\":\"property-change\"}\n").await?;
    peer.write_all(&[b'x'; 100]).await?;
    peer.write_all(b"\n{\"event\":\"file-loaded\"}\n").await?;
    peer.shutdown().await?;

    assert_eq!(
        transport.receive_next_frame().await?,
        Some(Err(ProtocolDiagnostic::InvalidJson))
    );
    assert_eq!(
        transport.receive_next_frame().await?,
        Some(Err(ProtocolDiagnostic::InvalidShape {
            context: "property-change"
        }))
    );
    assert_eq!(
        transport.receive_next_frame().await?,
        Some(Err(ProtocolDiagnostic::Oversized { max_bytes: 48 }))
    );
    assert_eq!(
        transport.receive_next_frame().await?,
        Some(Ok(MpvMessage::Event(MpvEvent::FileLoaded)))
    );
    assert_eq!(transport.receive_next_frame().await?, None);
    Ok(())
}

#[tokio::test]
async fn partial_eof_is_typed_and_reported_once() -> TestResult {
    let (mut peer, stream) = tokio::io::duplex(128);
    let mut transport = MpvTransport::new(Box::new(stream), 64)?;
    peer.write_all(b"{\"event\":\"shutdown\"").await?;
    peer.shutdown().await?;

    assert_eq!(
        transport.receive_next_frame().await?,
        Some(Err(ProtocolDiagnostic::UnexpectedEof { bytes_read: 19 }))
    );
    assert_eq!(transport.receive_next_frame().await?, None);
    Ok(())
}

#[tokio::test]
async fn zero_line_limit_is_rejected() {
    let (stream, _peer) = tokio::io::duplex(16);
    assert_eq!(
        MpvTransport::new(Box::new(stream), 0)
            .err()
            .map(|error| error.kind()),
        Some(io::ErrorKind::InvalidInput)
    );
}

#[tokio::test]
async fn transport_writes_exactly_one_newline_and_hides_stream_details() -> TestResult {
    let (mut peer, stream) = tokio::io::duplex(512);
    let mut transport = MpvTransport::new(Box::new(stream), 256)?;
    let request = MpvRequest::loadfile(7, "https://media.invalid/hidden")?;

    transport.send(&request).await?;
    let mut line = Vec::new();
    loop {
        let byte = peer.read_u8().await?;
        line.push(byte);
        if byte == b'\n' {
            break;
        }
    }
    assert_eq!(
        String::from_utf8(line)?,
        "{\"command\":[\"loadfile\",\"https://media.invalid/hidden\",\"replace\"],\"request_id\":7}\n"
    );
    let rendered = format!("{transport:?}");
    assert!(!rendered.contains("hidden"));
    Ok(())
}

struct PartialThenPending {
    captured: Arc<Mutex<Vec<u8>>>,
    wrote_once: bool,
}

impl AsyncRead for PartialThenPending {
    fn poll_read(
        self: Pin<&mut Self>,
        _context: &mut Context<'_>,
        _buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Poll::Pending
    }
}

impl AsyncWrite for PartialThenPending {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _context: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.wrote_once {
            return Poll::Pending;
        }
        let count = bytes.len().min(7);
        match self.captured.lock() {
            Ok(mut captured) => captured.extend_from_slice(&bytes[..count]),
            Err(_) => {
                return Poll::Ready(Err(io::Error::other("test capture mutex was poisoned")));
            }
        }
        self.wrote_once = true;
        Poll::Ready(Ok(count))
    }

    fn poll_flush(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Pending
    }

    fn poll_shutdown(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[tokio::test]
async fn cancelling_a_partial_send_poisons_transport_instead_of_corrupting_next_frame() -> TestResult
{
    let captured = Arc::new(Mutex::new(Vec::new()));
    let stream = PartialThenPending {
        captured: Arc::clone(&captured),
        wrote_once: false,
    };
    let mut transport = MpvTransport::new(Box::new(stream), 256)?;
    let first = MpvRequest::loadfile(1, "https://media.invalid/first")?;

    let mut send = Box::pin(transport.send(&first));
    assert!(futures::poll!(send.as_mut()).is_pending());
    drop(send);

    let before_retry = captured
        .lock()
        .map_err(|_| missing("test capture mutex was poisoned"))?
        .clone();
    let error = transport
        .send(&MpvRequest::quit(2)?)
        .await
        .err()
        .ok_or_else(|| missing("poisoned transport accepted another request"))?;
    assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    assert_eq!(
        *captured
            .lock()
            .map_err(|_| missing("test capture mutex was poisoned"))?,
        before_retry
    );
    assert_eq!(
        transport
            .receive_next_frame()
            .await
            .err()
            .map(|error| error.kind()),
        Some(io::ErrorKind::BrokenPipe)
    );
    Ok(())
}

struct CountingStream {
    bytes: Vec<u8>,
    cursor: usize,
    reads: Arc<AtomicUsize>,
}

impl AsyncRead for CountingStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        let count = (self.bytes.len() - self.cursor).min(buffer.remaining());
        let end = self.cursor + count;
        buffer.put_slice(&self.bytes[self.cursor..end]);
        self.cursor = end;
        Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for CountingStream {
    fn poll_write(
        self: Pin<&mut Self>,
        _context: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        Poll::Ready(Ok(bytes.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[tokio::test]
async fn one_chunk_read_can_feed_multiple_frames_without_another_syscall() -> TestResult {
    let reads = Arc::new(AtomicUsize::new(0));
    let stream = CountingStream {
        bytes: b"{\"event\":\"file-loaded\"}\n{\"event\":\"shutdown\"}\n".to_vec(),
        cursor: 0,
        reads: Arc::clone(&reads),
    };
    let mut transport = MpvTransport::new(Box::new(stream), 128)?;

    assert_eq!(
        transport.receive_next_frame().await?,
        Some(Ok(MpvMessage::Event(MpvEvent::FileLoaded)))
    );
    assert_eq!(
        transport.receive_next_frame().await?,
        Some(Ok(MpvMessage::Event(MpvEvent::Shutdown)))
    );
    assert_eq!(reads.load(Ordering::SeqCst), 1);
    Ok(())
}

struct GatedReadStream {
    step: u8,
    release: Arc<AtomicBool>,
}

impl AsyncRead for GatedReadStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.step {
            0 => {
                buffer.put_slice(b"{\"event\":\"file-");
                self.step = 1;
                Poll::Ready(Ok(()))
            }
            1 if !self.release.load(Ordering::SeqCst) => Poll::Pending,
            1 => {
                buffer.put_slice(b"loaded\"}\n");
                self.step = 2;
                Poll::Ready(Ok(()))
            }
            _ => Poll::Ready(Ok(())),
        }
    }
}

impl AsyncWrite for GatedReadStream {
    fn poll_write(
        self: Pin<&mut Self>,
        _context: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        Poll::Ready(Ok(bytes.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[tokio::test]
async fn cancelling_a_chunked_receive_preserves_the_partial_frame() -> TestResult {
    let release = Arc::new(AtomicBool::new(false));
    let stream = GatedReadStream {
        step: 0,
        release: Arc::clone(&release),
    };
    let mut transport = MpvTransport::new(Box::new(stream), 128)?;

    let mut receive = Box::pin(transport.receive_next_frame());
    assert!(futures::poll!(receive.as_mut()).is_pending());
    drop(receive);

    release.store(true, Ordering::SeqCst);
    assert_eq!(
        transport.receive_next_frame().await?,
        Some(Ok(MpvMessage::Event(MpvEvent::FileLoaded)))
    );
    Ok(())
}

fn connector_is_object_safe(_: &dyn MpvConnector) {}
fn stream_is_object_safe(_: Box<dyn AsyncReadWrite>) {}

#[test]
fn connector_and_stream_traits_are_object_safe() {
    let connector = NativeMpvConnector;
    connector_is_object_safe(&connector);
    let (stream, _peer) = tokio::io::duplex(64);
    stream_is_object_safe(Box::new(stream));
}

#[cfg(unix)]
#[test]
fn native_endpoint_is_unique_private_and_cleans_up() -> TestResult {
    use std::os::unix::fs::PermissionsExt as _;

    let first = IpcEndpoint::native()?;
    let second = IpcEndpoint::native()?;
    assert_ne!(first.as_os_str(), second.as_os_str());
    assert!(
        first
            .as_path()
            .ok_or_else(|| missing("missing Unix path"))?
            .is_absolute()
    );
    assert!(first.as_os_str().as_encoded_bytes().len() < 104);

    let directory = first
        .cleanup_directory()
        .ok_or_else(|| missing("missing Unix cleanup directory"))?
        .to_path_buf();
    let mode = std::fs::metadata(&directory)?.permissions().mode() & 0o777;
    assert_eq!(mode, 0o700);
    drop(first);
    assert!(!directory.exists());
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn production_connector_exchanges_a_real_protocol_frame_and_cleans_up()
-> Result<(), Box<dyn std::error::Error>> {
    use tokio::net::UnixListener;

    let endpoint = IpcEndpoint::native()
        .map_err(|error| io::Error::new(error.kind(), format!("create endpoint: {error}")))?;
    let socket_path = endpoint
        .as_path()
        .ok_or_else(|| missing("missing Unix socket path"))?
        .to_path_buf();
    let directory = endpoint
        .cleanup_directory()
        .ok_or_else(|| missing("missing cleanup directory"))?
        .to_path_buf();
    let listener = UnixListener::bind(&socket_path).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "bind fake peer at {} ({} bytes): {error}",
                socket_path.display(),
                socket_path.as_os_str().as_encoded_bytes().len()
            ),
        )
    })?;

    let server = tokio::spawn(async move {
        let (mut peer, _) = listener.accept().await?;
        let mut request = Vec::new();
        loop {
            let byte = peer.read_u8().await?;
            request.push(byte);
            if byte == b'\n' {
                break;
            }
        }
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&request)?,
            json!({"command":["quit"],"request_id":23})
        );
        peer.write_all(b"{\"request_id\":23,\"error\":\"success\"}\n")
            .await?;
        peer.shutdown().await
    });

    let stream = NativeMpvConnector
        .connect(&endpoint)
        .await
        .map_err(|error| io::Error::new(error.kind(), format!("connect fake peer: {error}")))?;
    let mut transport = MpvTransport::new(stream, 256)?;
    transport.send(&MpvRequest::quit(23)?).await?;
    let received = tokio::time::timeout(Duration::from_secs(2), transport.receive_next_frame())
        .await
        .map_err(|_| missing("timed out waiting for fake peer reply"))??;
    let frame = received.ok_or_else(|| missing("missing reply frame"))??;
    let MpvMessage::Reply(reply) = frame else {
        panic!("expected reply");
    };
    assert_eq!(reply.request_id(), 23);
    tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .map_err(|_| missing("timed out waiting for fake peer"))??
        .map_err(|error| io::Error::new(error.kind(), format!("fake peer exchange: {error}")))?;

    drop(transport);
    drop(endpoint);
    assert!(!socket_path.exists());
    assert!(!directory.exists());
    Ok(())
}

#[cfg(windows)]
#[tokio::test]
async fn production_connector_exchanges_a_real_protocol_frame()
-> Result<(), Box<dyn std::error::Error>> {
    use tokio::net::windows::named_pipe::ServerOptions;

    let endpoint = IpcEndpoint::native()?;
    let pipe_name = endpoint.as_os_str().to_os_string();
    let server = ServerOptions::new()
        .first_pipe_instance(true)
        .create(&pipe_name)?;
    let server_task = tokio::spawn(async move {
        let mut server = server;
        server.connect().await?;
        let mut request = Vec::new();
        loop {
            let byte = server.read_u8().await?;
            request.push(byte);
            if byte == b'\n' {
                break;
            }
        }
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&request)?,
            json!({"command":["quit"],"request_id":23})
        );
        server
            .write_all(b"{\"request_id\":23,\"error\":\"success\"}\n")
            .await
    });

    let stream = NativeMpvConnector.connect(&endpoint).await?;
    let mut transport = MpvTransport::new(stream, 256)?;
    transport.send(&MpvRequest::quit(23)?).await?;
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(2), transport.receive_next_frame())
            .await
            .map_err(|_| missing("timed out waiting for fake peer reply"))??,
        Some(Ok(MpvMessage::Reply(_)))
    ));
    tokio::time::timeout(Duration::from_secs(2), server_task)
        .await
        .map_err(|_| missing("timed out waiting for fake peer"))???;
    Ok(())
}

#[allow(dead_code)]
fn _portable_endpoint_surface(endpoint: &IpcEndpoint) {
    let _: &OsStr = endpoint.as_os_str();
    let _: Option<&Path> = endpoint.as_path();
    let _: io::Result<()> = Ok(());
}
