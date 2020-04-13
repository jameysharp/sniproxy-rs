// This implementation is inspired by https://github.com/dlundquist/sniproxy, but I wrote it from
// scratch based on a careful reading of the TLS 1.3 specification.

use std::net::SocketAddr;
use std::time::Duration;
use tokio::io::{self, AsyncReadExt, AsyncWriteExt, Error, ErrorKind};
use tokio::net;
use tokio::signal::unix::{signal, SignalKind};
use tokio::task;
use tokio::time::{timeout, Elapsed};

// Unless otherwise specified, all quotes are from RFC 8446 (TLS 1.3).

// legacy_record_version: "MUST be set to 0x0303 for all records generated by a TLS
// 1.3 implementation"
const TLS_LEGACY_RECORD_VERSION: [u8; 2] = [0x03, 0x03];

const TLS_HANDSHAKE_CONTENT_TYPE: u8 = 0x16;
const TLS_HANDSHAKE_TYPE_CLIENT_HELLO: u8 = 0x01;

const TLS_EXTENSION_SNI: usize = 0x0000;
const TLS_SNI_HOST_NAME_TYPE: u8 = 0;

const TLS_ALERT_CONTENT_TYPE: u8 = 21;
const TLS_ALERT_LENGTH: [u8; 2] = [0x00, 0x02];
const TLS_ALERT_LEVEL_FATAL: u8 = 2;

enum TlsError {
    UnexpectedMessage = 10,
    RecordOverflow = 22,
    DecodeError = 50,
    InternalError = 80,
    UserCanceled = 90,
    UnrecognizedName = 112,
}

impl From<Error> for TlsError {
    fn from(_error: Error) -> Self {
        TlsError::InternalError
    }
}

impl From<Elapsed> for TlsError {
    fn from(_error: Elapsed) -> Self {
        TlsError::UserCanceled
    }
}

type TlsResult<O> = Result<O, TlsError>;

struct TlsHandshakeReader<R> {
    source: R,
    buffer: Vec<u8>,
    offset: usize,
    limit: usize,
}

fn check_length(length: usize, limit: &mut usize) -> TlsResult<()> {
    *limit = limit.checked_sub(length).ok_or(TlsError::DecodeError)?;
    Ok(())
}

impl<R: AsyncReadExt> TlsHandshakeReader<R> {
    fn new(source: R) -> Self {
        TlsHandshakeReader {
            source: source,
            buffer: Vec::with_capacity(4096),
            offset: 0,
            limit: 0,
        }
    }

    fn seek(&mut self, offset: usize, limit: &mut usize) -> TlsResult<()> {
        self.offset += offset;
        check_length(offset, limit)
    }

    async fn fill_to(&mut self, target: usize) -> TlsResult<()> {
        while self.buffer.len() < target {
            if self.source.read_buf(&mut self.buffer).await? == 0 {
                return Err(TlsError::DecodeError);
            }
        }
        Ok(())
    }

    async fn read(&mut self) -> TlsResult<u8> {
        while self.offset >= self.limit {
            self.fill_to(self.limit + 5).await?;

            // section 5.1: "Handshake messages MUST NOT be interleaved with other record types.
            // That is, if a handshake message is split over two or more records, there MUST NOT be
            // any other records between them."
            if self.buffer[self.limit] != TLS_HANDSHAKE_CONTENT_TYPE {
                return Err(TlsError::UnexpectedMessage);
            }

            let length = (self.buffer[self.limit + 3] as usize) << 8
                | (self.buffer[self.limit + 4] as usize);

            // section 5.1: "Implementations MUST NOT send zero-length fragments of Handshake
            // types, even if those fragments contain padding."
            if length == 0 {
                return Err(TlsError::DecodeError);
            }

            // section 5.1: "The record layer fragments information blocks into TLSPlaintext
            // records carrying data in chunks of 2^14 bytes or less."
            if length > (1 << 14) {
                return Err(TlsError::RecordOverflow);
            }

            self.offset += 5;
            self.limit += 5 + length;
        }

        self.fill_to(self.offset + 1).await?;
        let v = self.buffer[self.offset];
        self.offset += 1;
        Ok(v)
    }

    async fn read_length(&mut self, length: u8) -> TlsResult<usize> {
        debug_assert!(length > 0 && length <= 4);
        let mut result = 0;
        for _ in 0..length {
            result <<= 8;
            result |= self.read().await? as usize;
        }
        Ok(result)
    }

    async fn into_source<W: AsyncWriteExt + Unpin>(self, dest: &mut W) -> io::Result<R> {
        dest.write_all(&self.buffer[..]).await?;
        Ok(self.source)
    }
}

async fn get_server_name<R: AsyncReadExt>(source: &mut TlsHandshakeReader<R>) -> TlsResult<String> {
    // section 4.1.2: "When a client first connects to a server, it is REQUIRED to send the
    // ClientHello as its first TLS message."
    if source.read().await? != TLS_HANDSHAKE_TYPE_CLIENT_HELLO {
        return Err(TlsError::UnexpectedMessage);
    }

    let mut hello_length = source.read_length(3).await?;

    // skip legacy_version (2) and random (32)
    source.seek(34, &mut hello_length)?;

    // skip legacy_session_id
    check_length(1, &mut hello_length)?;
    let length = source.read_length(1).await?;
    source.seek(length, &mut hello_length)?;

    // skip cipher_suites
    check_length(2, &mut hello_length)?;
    let length = source.read_length(2).await?;
    source.seek(length, &mut hello_length)?;

    // skip legacy_compression_methods
    check_length(1, &mut hello_length)?;
    let length = source.read_length(1).await?;
    source.seek(length, &mut hello_length)?;

    // section 4.1.2: "TLS 1.3 servers might receive ClientHello messages without an extensions
    // field from prior versions of TLS. The presence of extensions can be detected by determining
    // whether there are bytes following the compression_methods field at the end of the
    // ClientHello. Note that this method of detecting optional data differs from the normal TLS
    // method of having a variable-length field, but it is used for compatibility with TLS before
    // extensions were defined. ... If negotiating a version of TLS prior to 1.3, a server MUST
    // check that the message either contains no data after legacy_compression_methods or that it
    // contains a valid extensions block with no data following. If not, then it MUST abort the
    // handshake with a "decode_error" alert."
    //
    // If there is no extensions block, treat it like a server name extension was present but with
    // an unrecognized name. I don't think the spec allows this, but it doesn't NOT allow it?
    if hello_length == 0 {
        return Err(TlsError::UnrecognizedName);
    }

    // ClientHello ends immediately after the extensions
    check_length(2, &mut hello_length)?;
    if hello_length != source.read_length(2).await? {
        return Err(TlsError::DecodeError);
    }

    while hello_length > 0 {
        check_length(4, &mut hello_length)?;
        let extension = source.read_length(2).await?;
        let mut length = source.read_length(2).await?;

        if extension != TLS_EXTENSION_SNI {
            source.seek(length, &mut hello_length)?;
            continue;
        }

        check_length(length, &mut hello_length)?;

        // This extension ends immediately after server_name_list
        check_length(2, &mut length)?;
        if length != source.read_length(2).await? {
            return Err(TlsError::DecodeError);
        }

        while length > 0 {
            check_length(3, &mut length)?;
            let name_type = source.read().await?;
            let name_length = source.read_length(2).await?;

            if name_type != TLS_SNI_HOST_NAME_TYPE {
                source.seek(name_length, &mut length)?;
                continue;
            }

            check_length(name_length, &mut length)?;

            // RFC 6066 section 3: "The ServerNameList MUST NOT contain more than one name of the
            // same name_type." So we can just extract the first one we find.

            // Hostnames are limited to 255 octets with a trailing dot, but RFC 6066 prohibits the
            // trailing dot, so the limit here is 254 octets. Enforcing this limit ensures an
            // attacker can't make us heap-allocate 64kB for a hostname we'll never match.
            if name_length > 254 {
                return Err(TlsError::UnrecognizedName);
            }

            // The following validation rules ensure that we won't return a hostname which could
            // lead to pathname traversal (e.g. "..", "", or "a/b") and that semantically
            // equivalent hostnames are only returned in a canonical form. This does not validate
            // anything else about the hostname, such as length limits on individual labels.

            let mut name = Vec::with_capacity(name_length);
            let mut start_of_label = true;
            for _ in 0..name_length {
                let b = source.read().await?.to_ascii_lowercase();

                if start_of_label && (b == b'-' || b == b'.') {
                    // a hostname label can't start with dot or dash
                    return Err(TlsError::UnrecognizedName);
                }

                // the next byte is the start of a label iff this one was a dot
                start_of_label = b'.' == b;

                match b {
                    b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' => name.push(b),
                    _ => return Err(TlsError::UnrecognizedName),
                }
            }

            // If we're expecting a new label after reading the whole hostname, then either the
            // name was empty or it ended with a dot; neither is allowed.
            if start_of_label {
                return Err(TlsError::UnrecognizedName);
            }

            // safety: every byte was already checked for being a valid subset of UTF-8
            let name = unsafe { String::from_utf8_unchecked(name) };
            return Ok(name);
        }

        // None of the names were of the right type, and section 4.2 says "There MUST NOT be more
        // than one extension of the same type in a given extension block", so there definitely
        // isn't a server name in this ClientHello.
        break;
    }

    // Like when the extensions block is absent, pretend as if a server name was present but not
    // recognized.
    Err(TlsError::UnrecognizedName)
}

async fn connect_backend<R: AsyncReadExt>(
    source: R,
    local: SocketAddr,
    remote: SocketAddr,
) -> TlsResult<(R, net::UnixStream)> {
    let mut source = TlsHandshakeReader::new(source);

    // timeout can return a "Elapsed" error, or else return the result from get_server_name, which
    // might be a TlsError. So there are two "?" here to unwrap both.
    let name = timeout(Duration::from_secs(10), get_server_name(&mut source)).await??;

    let path: &std::path::Path = name.as_ref();

    // The client sent a name and it's been validated to be safe to use as a path. Consider it a
    // valid server name if connecting to the path doesn't return any of these errors:
    // - is a directory (NotFound after joining a relative path)
    // - which contains an entry named "tls-socket" (NotFound)
    // - which is accessible to this proxy (PermissionDenied)
    // - and is a listening socket (ConnectionRefused)
    // If it isn't a valid server name, then that's the error to report. Anything else is not the
    // client's fault.
    let mut backend = net::UnixStream::connect(path.join("tls-socket"))
        .await
        .map_err(|e| match e.kind() {
            ErrorKind::NotFound | ErrorKind::PermissionDenied | ErrorKind::ConnectionRefused => {
                TlsError::UnrecognizedName
            }
            _ => TlsError::InternalError,
        })?;

    // After this point, all I/O errors are internal errors.

    // If this file exists, turn on the PROXY protocol.
    // NOTE: This is a blocking syscall, but stat should be fast enough that it's not worth
    // spawning off a thread.
    if std::fs::metadata(path.join("send-proxy-v1")).is_ok() {
        let header = format!(
            "PROXY {} {} {} {} {}\r\n",
            match remote {
                SocketAddr::V4(_) => "TCP4",
                SocketAddr::V6(_) => "TCP6",
            },
            remote.ip(),
            local.ip(),
            remote.port(),
            local.port(),
        );

        backend.write_all(header.as_bytes()).await?;
    }

    let source = source.into_source(&mut backend).await?;
    Ok((source, backend))
}

async fn handle_connection(mut client: net::TcpStream, local: SocketAddr, remote: SocketAddr) {
    let (client_in, mut client_out) = client.split();

    let (client_in, mut backend) = match connect_backend(client_in, local, remote).await {
        Ok(r) => r,
        Err(e) => {
            // Try to send an alert before closing the connection, but if that fails, don't worry
            // about it... they'll figure it out eventually.
            let _ = client_out
                .write_all(&[
                    TLS_ALERT_CONTENT_TYPE,
                    TLS_LEGACY_RECORD_VERSION[0],
                    TLS_LEGACY_RECORD_VERSION[1],
                    TLS_ALERT_LENGTH[0],
                    TLS_ALERT_LENGTH[1],
                    TLS_ALERT_LEVEL_FATAL,
                    // AlertDescription comes from the returned error; see TlsError above
                    e as u8,
                ])
                .await;
            return;
        }
    };

    let (backend_in, backend_out) = backend.split();

    // Ignore errors in either direction; just half-close the destination when the source stops
    // being readable. And if that fails, ignore that too.
    async fn copy_all<R, W>(mut from: R, mut to: W)
    where
        R: AsyncReadExt + Unpin,
        W: AsyncWriteExt + Unpin,
    {
        let _ = io::copy(&mut from, &mut to).await;
        let _ = to.shutdown().await;
    }

    tokio::join!(
        copy_all(client_in, backend_out),
        copy_all(backend_in, client_out),
    );
}

async fn main_loop() -> io::Result<()> {
    // safety: the rest of the program must not use stdin
    let listener = unsafe { std::os::unix::io::FromRawFd::from_raw_fd(0) };

    // Assume stdin is an already bound and listening TCP socket.
    let mut listener = net::TcpListener::from_std(listener)?;

    // Asking for the listening socket's local address has the side effect of checking that it is
    // actually a TCP socket.
    let local = listener.local_addr()?;

    println!("listening on {}", local);

    let mut graceful_shutdown = signal(SignalKind::hangup())?;

    loop {
        tokio::select!(
            result = listener.accept() => result.map(|(socket, remote)| {
                let local = socket.local_addr().unwrap_or(local);
                task::spawn_local(handle_connection(socket, local, remote));
            })?,
            Some(_) = graceful_shutdown.recv() => break,
        );
    }

    println!("got SIGHUP, shutting down");
    Ok(())
}

#[tokio::main]
async fn main() -> io::Result<()> {
    let local = task::LocalSet::new();
    local.run_until(main_loop()).await?;
    timeout(Duration::from_secs(10), local)
        .await
        .map_err(|_| ErrorKind::TimedOut.into())
}
