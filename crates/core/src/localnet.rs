//! Cheap local networking: the TCP tables straight from Windows (GetExtendedTcpTable), process
//! lookups, and plain HTTP/1.1 to loopback servers.
//! Listener tables are tiny; the connection table (hundreds of TIME_WAIT rows)
//! is read only when a caller needs connection counts.

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, ErrorKind, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::Path;
use std::time::Duration;

use serde_json::Value;
use windows::Win32::Foundation::{CloseHandle, ERROR_INSUFFICIENT_BUFFER, HANDLE, NO_ERROR};
use windows::Win32::NetworkManagement::IpHelper::{
    GetExtendedTcpTable, MIB_TCP_STATE_ESTAB, TCP_TABLE_CLASS, TCP_TABLE_OWNER_PID_CONNECTIONS,
    TCP_TABLE_OWNER_PID_LISTENER,
};
use windows::Win32::Networking::WinSock::{ADDRESS_FAMILY, AF_INET, AF_INET6};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE, QueryFullProcessImageNameW,
    TerminateProcess,
};
use windows::core::PWSTR;

/// One row of a TCP table. Ports are in host byte order.
struct Row {
    state: u32,
    local: u16,
    remote: u16,
    pid: u32,
}

/// Every row of one TCP table and address family.
fn table(family: ADDRESS_FAMILY, class: TCP_TABLE_CLASS) -> io::Result<Vec<Row>> {
    // u32 words keep the buffer aligned for the table, whose fields are all whole words.
    let mut buffer: Vec<u32> = Vec::new();
    let mut size = 0u32;
    // The table can grow between the size query and the read, hence the retries.
    for _ in 0..5 {
        let pointer = (!buffer.is_empty()).then(|| buffer.as_mut_ptr().cast());
        // SAFETY: `pointer` is None or points at `buffer`, which holds at least `size` bytes.
        let result = unsafe { GetExtendedTcpTable(pointer, &mut size, false, u32::from(family.0), class, 0) };
        if result == NO_ERROR.0 {
            return parse_table(&buffer, family);
        }
        if result != ERROR_INSUFFICIENT_BUFFER.0 {
            return Err(io::Error::from_raw_os_error(result as i32));
        }
        buffer.resize((size as usize).div_ceil(4), 0);
    }
    Err(io::Error::other("GetExtendedTcpTable kept growing"))
}

/// Read MIB_TCPTABLE_OWNER_PID (6 words a row) or MIB_TCP6TABLE_OWNER_PID (14 words a row).
fn parse_table(words: &[u32], family: ADDRESS_FAMILY) -> io::Result<Vec<Row>> {
    let Some((&count, rows)) = words.split_first() else {
        return Ok(Vec::new());
    };
    let width = if family == AF_INET { 6 } else { 14 };
    let rows = rows.get(..count as usize * width).ok_or_else(|| io::Error::other("Truncated TCP table"))?;
    // Ports sit in the low 16 bits of their word, in network byte order.
    let port = |word: u32| u16::from_be(word as u16);
    Ok(rows
        .chunks_exact(width)
        .map(|row| match family {
            AF_INET => Row { state: row[0], local: port(row[2]), remote: port(row[4]), pid: row[5] },
            _ => Row { state: row[12], local: port(row[5]), remote: port(row[11]), pid: row[13] },
        })
        .collect())
}

/// {listening port: owning pid}, from the IPv4 and IPv6 listener tables (TCP_TABLE_OWNER_PID_LISTENER).
pub fn listeners() -> io::Result<HashMap<u16, u32>> {
    let mut listening = HashMap::new();
    for family in [AF_INET, AF_INET6] {
        for row in table(family, TCP_TABLE_OWNER_PID_LISTENER)? {
            listening.entry(row.local).or_insert(row.pid);
        }
    }
    Ok(listening)
}

/// (listeners(), {port: established connection count}). Counts are only read when `connections`
/// is set (TCP_TABLE_OWNER_PID_CONNECTIONS) and include both ends of a loopback connection.
pub fn tcp_snapshot(connections: bool) -> io::Result<(HashMap<u16, u32>, HashMap<u16, u32>)> {
    let mut established = HashMap::new();
    if connections {
        for family in [AF_INET, AF_INET6] {
            for row in table(family, TCP_TABLE_OWNER_PID_CONNECTIONS)? {
                if row.state == MIB_TCP_STATE_ESTAB.0 as u32 {
                    *established.entry(row.local).or_insert(0) += 1;
                    *established.entry(row.remote).or_insert(0) += 1;
                }
            }
        }
    }
    Ok((listeners()?, established))
}

pub fn port_in_use(port: u16) -> bool {
    listeners().is_ok_and(|listening| listening.contains_key(&port))
}

/// Executable file name of a running process (QueryFullProcessImageNameW), or None.
pub fn process_name(pid: u32) -> Option<String> {
    // SAFETY: a plain Win32 call; the handle is closed below.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
    let mut path = vec![0u16; 32768];
    let mut size = path.len() as u32;
    // SAFETY: `path` holds `size` UTF-16 units.
    let queried =
        unsafe { QueryFullProcessImageNameW(handle, PROCESS_NAME_WIN32, PWSTR(path.as_mut_ptr()), &mut size) };
    close(handle);
    queried.ok()?;
    let path = String::from_utf16_lossy(&path[..size as usize]);
    Path::new(&path).file_name().map(|name| name.to_string_lossy().into_owned())
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

/// Force-end a process (TerminateProcess).
pub fn kill_process(pid: u32) -> io::Result<()> {
    // SAFETY: plain Win32 calls; the handle is closed below.
    let handle = unsafe { OpenProcess(PROCESS_TERMINATE, false, pid) }?;
    let ended = unsafe { TerminateProcess(handle, 1) };
    close(handle);
    Ok(ended?)
}

fn close(handle: HANDLE) {
    // SAFETY: `handle` came from OpenProcess and is not used afterwards.
    let _ = unsafe { CloseHandle(handle) };
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
