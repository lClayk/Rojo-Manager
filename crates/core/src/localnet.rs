//! Cheap local networking: who listens on which local TCP port, process lookups (per platform, in
//! localnet_windows.rs and localnet_macos.rs), and plain HTTP/1.1 to loopback servers.
//! Connection counts are read only when a caller needs them.

use std::io::{self, BufRead, BufReader, ErrorKind, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::time::Duration;

use serde_json::Value;

#[cfg(target_os = "macos")]
#[path = "localnet_macos.rs"]
mod platform;
#[cfg(windows)]
#[path = "localnet_windows.rs"]
mod platform;

pub use platform::{kill_process, listeners, process_name, tcp_snapshot};

pub fn port_in_use(port: u16) -> bool {
    listeners().is_ok_and(|listening| listening.contains_key(&port))
}

/// (pid, executable name) of the process listening on a local TCP port. Names pids 0 and 4
/// "System Idle Process" and "System", and others Windows will not name "an unknown process".
pub fn port_owner(port: u16) -> Option<(u32, String)> {
    let pid = *listeners().ok()?.get(&port)?;
    let name = process_name(pid).unwrap_or_else(|| {
        match pid {
            0 => "System Idle Process",
            4 => "System",
            _ => "an unknown process",
        }
        .to_string()
    });
    Some((pid, name))
}

#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub content_type: String,
    pub body: Vec<u8>,
}

/// Longest status or header line accepted before a reply is not taken for HTTP.
const MAX_LINE: u64 = 16384;

/// One HTTP/1.1 request to 127.0.0.1 with `Connection: close`. A JSON body is sent with
/// Content-Type application/json. The body is cut at `max_bytes`. Fails fast with
/// ConnectionRefused when nothing listens on the port (Windows retries a refused loopback connect
/// for about 2 s), and with an error when the server times out or does not speak HTTP.
pub fn request(
    port: u16,
    method: &str,
    path: &str,
    body: Option<&Value>,
    headers: &[(&str, &str)],
    timeout: Duration,
    max_bytes: Option<usize>,
) -> io::Result<HttpResponse> {
    if !port_in_use(port) {
        return Err(io::Error::new(ErrorKind::ConnectionRefused, format!("Nothing is listening on 127.0.0.1:{port}")));
    }
    let stream = TcpStream::connect_timeout(&SocketAddr::from((Ipv4Addr::LOCALHOST, port)), timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    stream.set_nodelay(true)?;

    let data = body.map(serde_json::to_vec).transpose()?;
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n");
    if data.is_some() {
        head.push_str("Content-Type: application/json\r\n");
    }
    if data.is_some() || !matches!(method, "GET" | "HEAD") {
        head.push_str(&format!("Content-Length: {}\r\n", data.as_ref().map_or(0, Vec::len)));
    }
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    let mut message = head.into_bytes();
    message.extend(data.unwrap_or_default());
    (&stream).write_all(&message)?;

    let bad =
        |what: String| io::Error::new(ErrorKind::InvalidData, format!("Bad response from 127.0.0.1:{port}: {what}"));
    let mut reader = BufReader::new(&stream);
    let status_line = read_line(&mut reader)?;
    let status = status_line
        .strip_prefix("HTTP/1.")
        .and_then(|rest| rest.split(' ').nth(1))
        .filter(|code| code.len() == 3)
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| bad(format!("{:?}", status_line.trim_end())))?;
    let (mut content_type, mut length, mut chunked) = (String::new(), None, false);
    loop {
        let line = read_line(&mut reader)?;
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        let (name, value) = line.split_once(':').ok_or_else(|| bad(format!("{line:?}")))?;
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-type") {
            content_type = value.to_string();
        } else if name.eq_ignore_ascii_case("content-length") {
            length = Some(value.parse::<usize>().map_err(|_| bad(format!("Content-Length {value:?}")))?);
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            chunked = value.to_ascii_lowercase().contains("chunked");
        }
    }

    let limit = max_bytes.unwrap_or(usize::MAX);
    let mut body = Vec::new();
    if method == "HEAD" || matches!(status, 100..=199 | 204 | 304) {
        // No body.
    } else if chunked {
        while body.len() < limit {
            let line = read_line(&mut reader)?;
            let size = line.split(';').next().unwrap_or_default().trim();
            let size = usize::from_str_radix(size, 16).map_err(|_| bad(format!("chunk size {size:?}")))?;
            if size == 0 {
                break;
            }
            let wanted = size.min(limit - body.len());
            read_more(&mut reader, wanted, &mut body)?;
            if wanted < size {
                break;
            }
            read_line(&mut reader)?; // the CRLF that ends the chunk
        }
    } else if let Some(length) = length {
        read_more(&mut reader, length.min(limit), &mut body)?;
    } else {
        reader.take(limit as u64).read_to_end(&mut body)?;
    }
    Ok(HttpResponse { status, content_type, body })
}

/// One line up to its LF; an error when the connection closes first or the line is too long.
fn read_line(reader: &mut impl BufRead) -> io::Result<String> {
    let mut line = Vec::new();
    reader.take(MAX_LINE).read_until(b'\n', &mut line)?;
    if line.last() != Some(&b'\n') {
        return Err(io::Error::new(ErrorKind::InvalidData, "Incomplete HTTP response"));
    }
    Ok(String::from_utf8_lossy(&line).into_owned())
}

/// Append exactly `size` more bytes to `body`.
fn read_more(reader: &mut impl Read, size: usize, body: &mut Vec<u8>) -> io::Result<()> {
    let start = body.len();
    body.resize(start + size, 0);
    reader.read_exact(&mut body[start..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread;
    use std::time::Instant;

    #[test]
    fn listeners_and_connections_are_read_from_the_tcp_table() {
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = server.local_addr().unwrap().port();
        let (listening, established) = tcp_snapshot(true).unwrap();
        assert_eq!(listening.get(&port), Some(&std::process::id()));
        assert!(!established.contains_key(&port));
        {
            let _client = TcpStream::connect(("127.0.0.1", port)).unwrap();
            let _accepted = server.accept().unwrap();
            assert_eq!(tcp_snapshot(true).unwrap().1.get(&port), Some(&2)); // client and server ends
        }
        let exe = std::env::current_exe().unwrap();
        let name = exe.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(port_owner(port), Some((std::process::id(), name)));
        drop(server);
        assert!(!port_in_use(port));
        assert_eq!(port_owner(port), None);

        let started = Instant::now();
        let error = request(port, "GET", "/", None, &[], Duration::from_secs(5), None).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::ConnectionRefused);
        assert!(started.elapsed() < Duration::from_millis(500), "a refused loopback connect waits about 2 s");
    }

    /// Answer one connection with `reply` once its request has arrived; joining returns the request.
    fn serve_once(reply: &'static [u8]) -> (u16, thread::JoinHandle<String>) {
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = server.local_addr().unwrap().port();
        let handle = thread::spawn(move || {
            let (mut stream, _) = server.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let (mut request, mut length) = (String::new(), 0);
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if let Some(value) = line.strip_prefix("Content-Length: ") {
                    length = value.trim().parse().unwrap();
                }
                request.push_str(&line);
                if line == "\r\n" {
                    break;
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            request.push_str(&String::from_utf8(body).unwrap());
            stream.write_all(reply).unwrap();
            request
        });
        (port, handle)
    }

    #[test]
    fn requests_send_json_and_read_status_type_and_body() {
        let (port, server) =
            serve_once(b"HTTP/1.1 201 Created\r\nContent-Type: application/json\r\nContent-Length: 7\r\n\r\n{\"a\":1}");
        let body = serde_json::json!({"a": 1});
        let response =
            request(port, "POST", "/x", Some(&body), &[("X-Token", "t")], Duration::from_secs(5), None).unwrap();
        assert_eq!((response.status, response.content_type.as_str()), (201, "application/json"));
        assert_eq!(response.body, b"{\"a\":1}");
        let sent = server.join().unwrap();
        assert!(sent.starts_with("POST /x HTTP/1.1\r\n"));
        assert!(sent.contains("Content-Type: application/json\r\n") && sent.contains("X-Token: t\r\n"));
        assert!(sent.ends_with("\r\n\r\n{\"a\":1}"));
    }

    #[test]
    fn chunked_and_unsized_bodies_are_read_and_cut_at_max_bytes() {
        let (port, server) =
            serve_once(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n4;x=1\r\ndefg\r\n0\r\n\r\n");
        let response = request(port, "GET", "/", None, &[], Duration::from_secs(5), None).unwrap();
        assert_eq!(response.body, b"abcdefg");
        server.join().unwrap();

        let (port, server) = serve_once(b"HTTP/1.0 200 OK\r\n\r\nuntil the end");
        let response = request(port, "GET", "/", None, &[], Duration::from_secs(5), Some(5)).unwrap();
        assert_eq!(response.body, b"until");
        server.join().unwrap();
    }

    #[test]
    fn non_http_listeners_are_errors() {
        let (port, server) = serve_once(b"not http\r\n\r\n");
        assert!(request(port, "GET", "/", None, &[], Duration::from_secs(5), None).is_err());
        server.join().unwrap();
    }
}
