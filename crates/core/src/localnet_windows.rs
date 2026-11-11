//! Windows: the TCP tables straight from Windows (GetExtendedTcpTable) and process lookups.

use std::collections::HashMap;
use std::io;
use std::path::Path;

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
