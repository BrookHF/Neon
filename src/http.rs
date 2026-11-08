//! A hand-written HTTP/1.1 client over TLS. Small on purpose: one POST, one streamed body.

use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

use crate::error::{AgentError, Result};

pub struct Response {
    pub status: u16,
    pub body: Box<dyn Read>,
}

/// Either a TLS connection or a plain one, so a local proxy can be used later.
enum Stream {
    Tls(TlsStream),
    Plain(TcpStream),
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Stream::Tls(t) => t.read(buf),
            Stream::Plain(t) => t.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Stream::Tls(t) => t.write(buf),
            Stream::Plain(t) => t.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Stream::Tls(t) => t.flush(),
            Stream::Plain(t) => t.flush(),
        }
    }
}

struct TlsStream {
    io: TcpStream,
    conn: rustls::ClientConnection,
}

impl Read for TlsStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        // With a blocking socket, Ok means rustls made every bit of progress it can.
        match self.conn.complete_io(&mut self.io) {
            Ok(_) => match self.conn.reader().read(buf) {
                Ok(n) => Ok(n),
                // Nothing decrypted after full progress: the connection ended without
                // a TLS close_notify. That is the end of the body, not an error.
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(0),
                Err(e) => Err(e),
            },
            // WouldBlock as an error is the socket read timeout firing.
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                Err(io::Error::new(io::ErrorKind::TimedOut, "read timed out"))
            }
            Err(e) => Err(e),
        }
    }
}

impl Write for TlsStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.conn.writer().write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.conn.complete_io(&mut self.io).map(|_| ())
    }
}

pub fn split_url(url: &str) -> Result<(String, u16, String)> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| AgentError::Http(format!("not a url: {url}")))?;
    if scheme != "https" && scheme != "http" {
        return Err(AgentError::Http(format!("unsupported scheme: {scheme}")));
    }
    let host_part = rest.split('/').next().unwrap_or(rest);
    let path = match rest.find('/') {
        Some(i) => rest[i..].to_string(),
        None => "/".to_string(),
    };
    let port = if scheme == "https" { 443 } else { 80 };
    let host = host_part
        .split(':')
        .next()
        .unwrap_or(host_part)
        .to_string();
    Ok((host, port, path))
}

fn connect(host: &str, port: u16, tls: bool) -> Result<Stream> {
    let addr = (host, port)
        .to_socket_addrs()
        .map_err(|e| AgentError::Http(format!("resolve {host}: {e}")))?
        .next()
        .ok_or_else(|| AgentError::Http(format!("no address for {host}")))?;
    let io = TcpStream::connect_timeout(&addr, Duration::from_secs(15))
        .map_err(|e| AgentError::Http(format!("connect {host}: {e}")))?;
    io.set_read_timeout(Some(Duration::from_secs(120)))
        .map_err(|e| AgentError::Http(format!("timeout: {e}")))?;

    if !tls {
        return Ok(Stream::Plain(io));
    }
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let cfg = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let name = rustls::pki_types::ServerName::try_from(host.to_string())
        .map_err(|e| AgentError::Http(format!("bad server name: {e}")))?;
    let conn = rustls::ClientConnection::new(Arc::new(cfg), name)
        .map_err(|e| AgentError::Http(format!("tls: {e}")))?;
    let mut stream = Stream::Tls(TlsStream { io, conn });
    stream.flush().map_err(|e| AgentError::Http(format!("handshake: {e}")))?;
    Ok(stream)
}

pub fn post(url: &str, headers: &[(&str, &str)], body: &[u8]) -> Result<Response> {
    request("POST", url, headers, body)
}

/// A GET request. Used by the `net.fetch` plugin service.
pub fn get(url: &str) -> Result<Response> {
    request("GET", url, &[], b"")
}

fn request(method: &str, url: &str, headers: &[(&str, &str)], body: &[u8]) -> Result<Response> {
    let (host, port, path) = split_url(url)?;
    let tls = url.starts_with("https://");

    // A drop before the response headers arrive is transient here (PRoot networking,
    // or a server that closes without answering). Nothing was processed yet, so retrying
    // the same request is safe. Failures after headers are not retried.
    let mut last_err = String::new();
    for attempt in 1..=3 {
        let mut stream = match connect(&host, port, tls) {
            Ok(s) => s,
            Err(e) => {
                last_err = e.to_string();
                continue;
            }
        };
        let mut req = Vec::new();
        req.extend_from_slice(format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\nContent-Length: {}\r\nConnection: close\r\n", body.len()).as_bytes());
        for (k, v) in headers {
            req.extend_from_slice(format!("{k}: {v}\r\n").as_bytes());
        }
        req.extend_from_slice(b"\r\n");
        req.extend_from_slice(body);

        if let Err(e) = stream.write_all(&req).and_then(|()| stream.flush()) {
            last_err = format!("write: {e}");
            continue;
        }
        match read_head(&mut stream) {
            Ok((status, hs, leftover)) => {
                let chunked = hs
                    .iter()
                    .any(|(k, v)| k == "transfer-encoding" && v.contains("chunked"));
                return Ok(Response {
                    status,
                    body: Box::new(Body {
                        inner: stream,
                        leftover,
                        chunked,
                        chunk_left: 0,
                        done: false,
                        pending: std::collections::VecDeque::new(),
                        raw: Vec::new(),
                    }),
                });
            }
            Err(e) => {
                last_err = format!("head: {e}");
                continue;
            }
        }
    }
    Err(AgentError::Http(format!("{last_err} (after 3 attempts)")))
}

/// Read status line and headers, and return bytes already read past the header block.
fn read_head<R: Read>(r: &mut R) -> io::Result<(u16, Vec<(String, String)>, Vec<u8>)> {
    let mut buf: Vec<u8> = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        let n = r.read(&mut tmp)?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
        if find(&buf, b"\r\n\r\n").is_some() {
            break;
        }
        if buf.len() > 64 * 1024 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "header block too large"));
        }
    }
    let sep = find(&buf, b"\r\n\r\n")
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no header terminator"))?;
    let head = String::from_utf8_lossy(&buf[..sep]);
    let leftover = buf[sep + 4..].to_vec();

    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or("");
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_lowercase(), v.trim().to_string()))
        .collect();
    Ok((status, headers, leftover))
}

struct Body<R: Read> {
    inner: R,
    leftover: Vec<u8>,
    chunked: bool,
    chunk_left: usize,
    done: bool,
    pending: std::collections::VecDeque<u8>,
    raw: Vec<u8>,
}

impl<R: Read> Body<R> {
    /// Fill `pending` with decoded chunk bytes. Returns false at the end of the body.
    fn fill(&mut self) -> io::Result<bool> {
        if !self.chunked {
            return Ok(true);
        }
        loop {
            if !self.pending.is_empty() {
                return Ok(true);
            }
            if self.done {
                return Ok(false);
            }
            if self.chunk_left > 0 {
                if self.raw.len() >= self.chunk_left + 2 {
                    let take = self.chunk_left;
                    self.pending.extend(self.raw.drain(..take));
                    self.raw.drain(..2.min(self.raw.len()));
                    self.chunk_left = 0;
                }
                continue;
            }
            if let Some(pos) = find(&self.raw, b"\r\n") {
                let line = self.raw[..pos].to_vec();
                self.raw.drain(..pos + 2);
                let size = parse_chunk_size(&line)?;
                if size == 0 {
                    self.done = true;
                    continue;
                }
                self.chunk_left = size;
                continue;
            }
            let mut tmp = [0u8; 8192];
            let n = self.inner.read(&mut tmp)?;
            if n == 0 {
                self.done = true;
                continue;
            }
            self.raw.extend_from_slice(&tmp[..n]);
        }
    }
}

impl<R: Read> Read for Body<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.chunked {
            if !self.leftover.is_empty() {
                self.raw.append(&mut self.leftover);
            }
        } else if !self.leftover.is_empty() {
            let n = self.leftover.len().min(out.len());
            out[..n].copy_from_slice(&self.leftover[..n]);
            self.leftover.drain(..n);
            return Ok(n);
        }
        if !self.chunked {
            return self.inner.read(out);
        }
        if self.pending.is_empty() && !self.fill()? {
            return Ok(0);
        }
        let n = self.pending.len().min(out.len());
        for byte in out[..n].iter_mut() {
            *byte = self.pending.pop_front().expect("pending has bytes");
        }
        Ok(n)
    }
}

fn parse_chunk_size(line: &[u8]) -> io::Result<usize> {
    let text = String::from_utf8_lossy(line);
    let hex = text.split(';').next().unwrap_or("").trim();
    usize::from_str_radix(hex, 16)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, format!("bad chunk size: {hex}")))
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Call `cb` for each line of a streamed body. A connection that ends without a TLS
/// close_notify still delivers everything it sent; the read error is returned as a message.
pub fn each_line<R: Read>(r: &mut R, mut cb: impl FnMut(&str)) -> Option<String> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 8192];
    let mut io_error: Option<String> = None;
    loop {
        match r.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
            Err(e) => {
                io_error = Some(e.to_string());
                break;
            }
        }
        loop {
            match find(&buf, b"\n") {
                Some(pos) => {
                    let line = buf[..pos].to_vec();
                    buf.drain(..pos + 1);
                    cb(std::str::from_utf8(&line).unwrap_or(""));
                }
                None => break,
            }
        }
    }
    if !buf.is_empty() {
        cb(std::str::from_utf8(&buf).unwrap_or(""));
    }
    io_error
}

/// Read to the end, keeping whatever arrived if the connection closes badly.
pub fn read_all<R: Read>(r: &mut R) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut tmp = [0u8; 8192];
    loop {
        match r.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => bytes.extend_from_slice(&tmp[..n]),
            Err(_) => break,
        }
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_split() {
        let (host, port, path) = split_url("https://example.com/v1/a?b=1").unwrap();
        assert_eq!((host.as_str(), port, path.as_str()), ("example.com", 443, "/v1/a?b=1"));
        let (host, port, path) = split_url("http://127.0.0.1:8080/v1").unwrap();
        assert_eq!((host.as_str(), port, path.as_str()), ("127.0.0.1", 80, "/v1"));
    }

    #[test]
    fn chunk_size_with_extension() {
        assert_eq!(parse_chunk_size(b"1a;ext=x\r").unwrap(), 26);
    }

    #[test]
    fn decodes_chunked_body() {
        let raw = b"5\r\nhello\r\nf\r\nworld!!!!!!!!!!\r\n0\r\n\r\n";
        let mut body = Body {
            inner: std::io::Cursor::new(raw.to_vec()),
            leftover: Vec::new(),
            chunked: true,
            chunk_left: 0,
            done: false,
            pending: std::collections::VecDeque::new(),
            raw: Vec::new(),
        };
        let mut text = String::new();
        body.read_to_string(&mut text).unwrap();
        assert_eq!(text, "helloworld!!!!!!!!!!");
    }

    #[test]
    fn leftover_bytes_join_the_chunk_stream() {
        let raw = b"2\r\nab\r\n2\r\ncd\r\n0\r\n\r\n";
        let mut body = Body {
            inner: std::io::Cursor::new(raw[2..].to_vec()),
            leftover: raw[..2].to_vec(),
            chunked: true,
            chunk_left: 0,
            done: false,
            pending: std::collections::VecDeque::new(),
            raw: Vec::new(),
        };
        let mut text = String::new();
        body.read_to_string(&mut text).unwrap();
        assert_eq!(text, "abcd");
    }

    #[test]
    fn plain_body_reads_to_end() {
        let mut body = Body {
            inner: std::io::Cursor::new(b"abc".to_vec()),
            leftover: Vec::new(),
            chunked: false,
            chunk_left: 0,
            done: false,
            pending: std::collections::VecDeque::new(),
            raw: Vec::new(),
        };
        let mut text = String::new();
        body.read_to_string(&mut text).unwrap();
        assert_eq!(text, "abc");
    }

    #[test]
    fn lines_stream_in_order() {
        let mut cursor = std::io::Cursor::new(b"data: one\n\ndata: two\nlast".to_vec());
        let mut lines = Vec::new();
        each_line(&mut cursor, |l| lines.push(l.to_string()));
        assert_eq!(lines, ["data: one", "", "data: two", "last"]);
    }
}
