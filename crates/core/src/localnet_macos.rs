//! macOS: TCP sockets and their owners from libproc, the way lsof finds them, without starting a
//! process every second. Only the signed-in user's processes are visible, which covers Rojo.
#![allow(dead_code)] // the C structs below

use std::collections::HashMap;
use std::ffi::{c_int, c_void};
use std::io;
use std::mem::{size_of, zeroed};
use std::path::Path;

// The structs below mirror xnu's bsd/sys/proc_info.h; libc does not declare the socket ones. Most
// fields are never read, but each must be there for the ones after it to line up.

#[repr(C)]
#[derive(Clone, Copy)]
struct ProcFileInfo {
    openflags: u32,
    status: u32,
    offset: i64,
    kind: i32,
    guardflags: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct VinfoStat {
    dev: u32,
    mode: u16,
    nlink: u16,
    ino: u64,
    uid: u32,
    gid: u32,
    /// atime, mtime, ctime, and birthtime, each seconds then nanoseconds.
    times: [i64; 8],
    size: i64,
    blocks: i64,
    blksize: i32,
    flags: u32,
    generation: u32,
    rdev: u32,
    qspare: [i64; 2],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct SockbufInfo {
    cc: u32,
    hiwat: u32,
    mbcnt: u32,
    mbmax: u32,
    lowat: u32,
    flags: i16,
    timeo: i16,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct InSockInfo {
    /// Ports are in network byte order.
    fport: c_int,
    lport: c_int,
    gencnt: u64,
    flags: u32,
    flow: u32,
    vflag: u8,
    ip_ttl: u8,
    rfu_1: u32,
    faddr: [u32; 4],
    laddr: [u32; 4],
    v4_tos: u8,
    v6: InV6,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct InV6 {
    hlim: u8,
    cksum: c_int,
    ifindex: u16,
    hops: i16,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct TcpSockInfo {
    ini: InSockInfo,
    state: c_int,
    timer: [c_int; 4],
    mss: c_int,
    flags: u32,
    rfu_1: u32,
    tp: u64,
}

/// soi_proto; its largest member (un_sockinfo) is 528 bytes, and a larger buffer is fine.
#[repr(C)]
#[derive(Clone, Copy)]
union Proto {
    tcp: TcpSockInfo,
    padding: [u64; 80],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct SocketInfo {
    stat: VinfoStat,
    so: u64,
    pcb: u64,
    kind: c_int,
    protocol: c_int,
    family: c_int,
    options: i16,
    linger: i16,
    state: i16,
    qlen: i16,
    incqlen: i16,
    qlimit: i16,
    timeo: i16,
    error: u16,
    oobmark: u32,
    rcv: SockbufInfo,
    snd: SockbufInfo,
    /// SOCKINFO_TCP for TCP sockets, whose `proto` is then `tcp`.
    proto_kind: c_int,
    rfu_1: u32,
    proto: Proto,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct SocketFdInfo {
    pfi: ProcFileInfo,
    psi: SocketInfo,
}

const PROC_PIDLISTFDS: c_int = 1;
const PROX_FDTYPE_SOCKET: u32 = 2;
const PROC_PIDFDSOCKETINFO: c_int = 3;
const SOCKINFO_TCP: c_int = 2;
const TSI_S_LISTEN: c_int = 1;
const TSI_S_ESTABLISHED: c_int = 4;

/// One TCP socket. Ports are in host byte order.
struct Socket {
    state: c_int,
    local: u16,
    remote: u16,
    pid: u32,
}

fn pids() -> io::Result<Vec<c_int>> {
    // SAFETY: a null buffer asks for the count.
    let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if count <= 0 {
        return Err(io::Error::last_os_error());
    }
    // Processes can start between the two calls.
    let mut pids = vec![0 as c_int; count as usize + 64];
    let bytes = (pids.len() * size_of::<c_int>()) as c_int;
    // SAFETY: `pids` holds `bytes` bytes.
    let count = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
    if count < 0 {
        return Err(io::Error::last_os_error());
    }
    pids.truncate(count as usize);
    Ok(pids)
}

/// The TCP sockets of one process; empty when it is not ours to inspect or has exited.
fn sockets_of(pid: c_int, found: &mut Vec<Socket>) {
    // SAFETY: a null buffer asks for the size of the descriptor list.
    let size = unsafe { libc::proc_pidinfo(pid, PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
    if size <= 0 {
        return;
    }
    let mut fds =
        vec![libc::proc_fdinfo { proc_fd: 0, proc_fdtype: 0 }; size as usize / size_of::<libc::proc_fdinfo>() + 16];
    let bytes = (fds.len() * size_of::<libc::proc_fdinfo>()) as c_int;
    // SAFETY: `fds` holds `bytes` bytes.
    let size = unsafe { libc::proc_pidinfo(pid, PROC_PIDLISTFDS, 0, fds.as_mut_ptr().cast(), bytes) };
    if size <= 0 {
        return;
    }
    fds.truncate(size as usize / size_of::<libc::proc_fdinfo>());
    for fd in fds.iter().filter(|fd| fd.proc_fdtype == PROX_FDTYPE_SOCKET) {
        // SAFETY: SocketFdInfo is plain data, so all zeroes is a valid value.
        let mut info: SocketFdInfo = unsafe { zeroed() };
        let size = size_of::<SocketFdInfo>() as c_int;
        // SAFETY: `info` holds `size` bytes.
        let read = unsafe {
            libc::proc_pidfdinfo(pid, fd.proc_fd, PROC_PIDFDSOCKETINFO, (&raw mut info).cast::<c_void>(), size)
        };
        if read <= 0 || info.psi.proto_kind != SOCKINFO_TCP {
            continue;
        }
        // SAFETY: proto_kind says the union holds TCP socket info.
        let tcp = unsafe { info.psi.proto.tcp };
        let port = |port: c_int| u16::from_be(port as u16);
        found.push(Socket {
            state: tcp.state,
            local: port(tcp.ini.lport),
            remote: port(tcp.ini.fport),
            pid: pid as u32,
        });
    }
}

fn sockets() -> io::Result<Vec<Socket>> {
    let mut found = Vec::new();
    for pid in pids()? {
        sockets_of(pid, &mut found);
    }
    Ok(found)
}

fn listening(sockets: &[Socket]) -> HashMap<u16, u32> {
    let mut listening = HashMap::new();
    for socket in sockets.iter().filter(|socket| socket.state == TSI_S_LISTEN) {
        listening.entry(socket.local).or_insert(socket.pid);
    }
    listening
}

/// {listening port: owning pid}.
pub fn listeners() -> io::Result<HashMap<u16, u32>> {
    Ok(listening(&sockets()?))
}

/// (listeners(), {port: established connection count}). Counts are only filled in when
/// `connections` is set and include both ends of a loopback connection.
pub fn tcp_snapshot(connections: bool) -> io::Result<(HashMap<u16, u32>, HashMap<u16, u32>)> {
    let sockets = sockets()?;
    let mut established = HashMap::new();
    if connections {
        // As on Windows, each socket counts for both its ports.
        for socket in sockets.iter().filter(|socket| socket.state == TSI_S_ESTABLISHED) {
            *established.entry(socket.local).or_insert(0) += 1;
            *established.entry(socket.remote).or_insert(0) += 1;
        }
    }
    Ok((listening(&sockets), established))
}

/// Executable file name of a running process (proc_pidpath), or None.
pub fn process_name(pid: u32) -> Option<String> {
    let mut path = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: `path` holds the byte count passed.
    let length = unsafe { libc::proc_pidpath(pid as c_int, path.as_mut_ptr().cast(), path.len() as u32) };
    if length <= 0 {
        return None;
    }
    let path = String::from_utf8_lossy(&path[..length as usize]).into_owned();
    Path::new(&path).file_name().map(|name| name.to_string_lossy().into_owned())
}

/// Force-end a process (SIGKILL).
pub fn kill_process(pid: u32) -> io::Result<()> {
    // SAFETY: a plain signal send.
    if unsafe { libc::kill(pid as c_int, libc::SIGKILL) } == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}
