//! Thin wrappers over libproc and sysctl. Everything here works for processes
//! owned by the current user without special entitlements, and none of it
//! spawns a subprocess, which is what keeps startup instant.

use std::collections::HashMap;
use std::ffi::CStr;
use std::mem::{size_of, zeroed};
use std::os::raw::{c_char, c_int, c_void};
use std::sync::OnceLock;

#[derive(Clone, Debug)]
pub struct Proc {
    pub pid: i32,
    pub ppid: i32,
    pub comm: String,
    /// Start time in microseconds since the epoch; with the pid, a process's identity.
    pub start: u64,
}

#[derive(Clone, Debug, Default)]
pub struct ProcArgs {
    pub exe: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
}

#[derive(Clone, Copy, Debug)]
pub struct Usage {
    pub footprint: u64,
    pub cpu_ns: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TcpState {
    Listen,
    Established,
}

#[derive(Clone, Debug)]
pub struct Socket {
    pub pid: i32,
    pub port: u16,
    pub address: String,
    pub state: TcpState,
}

pub fn all_processes() -> HashMap<i32, Proc> {
    let estimate = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if estimate <= 0 {
        return HashMap::new();
    }
    let mut pids = vec![0i32; estimate as usize + 64];
    let bytes = (pids.len() * size_of::<i32>()) as c_int;
    let count = unsafe { libc::proc_listallpids(pids.as_mut_ptr() as *mut c_void, bytes) };
    if count <= 0 {
        return HashMap::new();
    }
    pids.truncate(count as usize);
    pids.into_iter()
        .filter(|&pid| pid > 0)
        .filter_map(|pid| snapshot(pid).map(|p| (pid, p)))
        .collect()
}

pub fn snapshot(pid: i32) -> Option<Proc> {
    let mut info: libc::proc_bsdinfo = unsafe { zeroed() };
    let size = size_of::<libc::proc_bsdinfo>() as c_int;
    let read = unsafe {
        libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, &mut info as *mut _ as *mut c_void, size)
    };
    if read != size {
        return None;
    }
    Some(Proc {
        pid,
        ppid: info.pbi_ppid as i32,
        // pbi_name holds 32 characters to pbi_comm's 16.
        comm: Some(c_string(info.pbi_name.as_ptr())).filter(|n| !n.is_empty()).unwrap_or_else(|| c_string(info.pbi_comm.as_ptr())),
        start: info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec,
    })
}

pub fn usage(pid: i32) -> Option<Usage> {
    let mut info: libc::rusage_info_v4 = unsafe { zeroed() };
    let result = unsafe {
        libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V4, &mut info as *mut _ as *mut libc::rusage_info_t)
    };
    if result != 0 {
        return None;
    }
    let (numer, denom) = timebase();
    let ticks = info.ri_user_time + info.ri_system_time;
    Some(Usage { footprint: info.ri_phys_footprint, cpu_ns: ticks * numer / denom })
}

#[allow(deprecated)]
fn timebase() -> (u64, u64) {
    static TIMEBASE: OnceLock<(u64, u64)> = OnceLock::new();
    *TIMEBASE.get_or_init(|| {
        let mut info = libc::mach_timebase_info { numer: 0, denom: 0 };
        unsafe { libc::mach_timebase_info(&mut info) };
        (info.numer.max(1) as u64, info.denom.max(1) as u64)
    })
}

pub fn current_dir(pid: i32) -> Option<String> {
    let mut info: libc::proc_vnodepathinfo = unsafe { zeroed() };
    let size = size_of::<libc::proc_vnodepathinfo>() as c_int;
    let read = unsafe {
        libc::proc_pidinfo(pid, libc::PROC_PIDVNODEPATHINFO, 0, &mut info as *mut _ as *mut c_void, size)
    };
    if read != size {
        return None;
    }
    let path = c_string(info.pvi_cdir.vip_path.as_ptr() as *const c_char);
    (!path.is_empty()).then_some(path)
}

/// Executable, argv and environment from `KERN_PROCARGS2`.
pub fn arguments(pid: i32) -> Option<ProcArgs> {
    static ARG_MAX: OnceLock<usize> = OnceLock::new();
    let arg_max = *ARG_MAX.get_or_init(|| {
        let mut mib = [libc::CTL_KERN, libc::KERN_ARGMAX];
        let mut value: c_int = 0;
        let mut size = size_of::<c_int>();
        let ok = unsafe {
            libc::sysctl(mib.as_mut_ptr(), 2, &mut value as *mut _ as *mut c_void, &mut size, std::ptr::null_mut(), 0)
        };
        if ok == 0 && value > 0 { value as usize } else { 1 << 20 }
    });

    let mut buffer = vec![0u8; arg_max];
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    let mut size = arg_max;
    let ok = unsafe {
        libc::sysctl(mib.as_mut_ptr(), 3, buffer.as_mut_ptr() as *mut c_void, &mut size, std::ptr::null_mut(), 0)
    };
    if ok != 0 || size <= size_of::<c_int>() {
        return None;
    }
    buffer.truncate(size);

    let argc = i32::from_ne_bytes(buffer[..4].try_into().ok()?).max(0);
    let mut index = 4;
    let read = |index: &mut usize| -> Option<String> {
        if *index >= buffer.len() {
            return None;
        }
        let start = *index;
        while *index < buffer.len() && buffer[*index] != 0 {
            *index += 1;
        }
        let value = String::from_utf8_lossy(&buffer[start..*index]).into_owned();
        *index += 1;
        Some(value)
    };

    let exe = read(&mut index)?;
    while index < size && buffer[index] == 0 {
        index += 1;
    }
    let mut args = Vec::with_capacity(argc as usize);
    for _ in 0..argc {
        match read(&mut index) {
            Some(arg) => args.push(arg),
            None => break,
        }
    }
    let mut env = HashMap::new();
    while let Some(entry) = read(&mut index) {
        if entry.is_empty() {
            break;
        }
        if let Some((key, value)) = entry.split_once('=') {
            env.insert(key.to_string(), value.to_string());
        }
    }
    Some(ProcArgs { exe, args, env })
}

// `struct socket_fdinfo` from <sys/proc_info.h>. Its nested unions are read as
// raw bytes at offsets checked against the macOS SDK headers.
const PROC_PIDFDSOCKETINFO: c_int = 3;
const SOCKET_FDINFO_SIZE: usize = 792;
const SOI_KIND: usize = 256; // psi (24) + soi_kind (232)
const SOCKINFO_TCP: i32 = 2;
const INSI_LPORT: usize = 268; // psi + soi_proto (240) + insi_lport (4)
const INSI_VFLAG: usize = 288; // psi + soi_proto + insi_vflag (24)
const INSI_LADDR: usize = 312; // psi + soi_proto + insi_laddr (48)
const TCPSI_STATE: usize = 344; // psi + soi_proto + tcpsi_state (80)
const INI_IPV4: u8 = 1;
const TSI_S_LISTEN: i32 = 1;
const TSI_S_ESTABLISHED: i32 = 4;

/// Listening and established TCP sockets held by one process.
pub fn tcp_sockets(pid: i32, out: &mut Vec<Socket>) {
    let needed = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
    if needed <= 0 {
        return;
    }
    let capacity = needed as usize / size_of::<libc::proc_fdinfo>() + 16;
    let mut fds: Vec<libc::proc_fdinfo> = vec![unsafe { zeroed() }; capacity];
    let bytes = (capacity * size_of::<libc::proc_fdinfo>()) as c_int;
    let read = unsafe {
        libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, fds.as_mut_ptr() as *mut c_void, bytes)
    };
    if read <= 0 {
        return;
    }
    fds.truncate(read as usize / size_of::<libc::proc_fdinfo>());

    let mut raw = [0u8; SOCKET_FDINFO_SIZE];
    for fd in fds.iter().filter(|fd| fd.proc_fdtype == libc::PROX_FDTYPE_SOCKET as u32) {
        let got = unsafe {
            libc::proc_pidfdinfo(pid, fd.proc_fd, PROC_PIDFDSOCKETINFO, raw.as_mut_ptr() as *mut c_void, SOCKET_FDINFO_SIZE as c_int)
        };
        if got as usize != SOCKET_FDINFO_SIZE || read_i32(&raw, SOI_KIND) != SOCKINFO_TCP {
            continue;
        }
        let state = match read_i32(&raw, TCPSI_STATE) {
            TSI_S_LISTEN => TcpState::Listen,
            TSI_S_ESTABLISHED => TcpState::Established,
            _ => continue,
        };
        // The port is stored in network byte order in the low 16 bits.
        let port = u16::from_be(read_i32(&raw, INSI_LPORT) as u16);
        let address = if raw[INSI_VFLAG] & INI_IPV4 != 0 {
            let a = &raw[INSI_LADDR + 12..INSI_LADDR + 16];
            if a == [0, 0, 0, 0] { "*".to_string() } else { format!("{}.{}.{}.{}", a[0], a[1], a[2], a[3]) }
        } else {
            let bytes: [u8; 16] = raw[INSI_LADDR..INSI_LADDR + 16].try_into().unwrap_or_default();
            let ip = std::net::Ipv6Addr::from(bytes);
            if ip.is_unspecified() { "*".to_string() } else { format!("[{ip}]") }
        };
        out.push(Socket { pid, port, address, state });
    }
}

fn read_i32(raw: &[u8], offset: usize) -> i32 {
    i32::from_ne_bytes(raw[offset..offset + 4].try_into().unwrap_or_default())
}

/// Physical memory and "Memory Used" as Activity Monitor counts it:
/// app memory (anonymous, non-purgeable) + wired + compressed.
pub fn system_memory() -> (u64, u64) {
    let total = sysctl_u64(c"hw.memsize").unwrap_or(0);
    let mut stats: libc::vm_statistics64 = unsafe { zeroed() };
    let mut count = libc::HOST_VM_INFO64_COUNT;
    #[allow(deprecated)]
    let result = unsafe {
        libc::host_statistics64(libc::mach_host_self(), libc::HOST_VM_INFO64, &mut stats as *mut _ as *mut i32, &mut count)
    };
    if result != 0 {
        return (total, 0);
    }
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(4096) as u64;
    let anonymous = stats.internal_page_count as u64;
    let app = anonymous.saturating_sub(stats.purgeable_count as u64);
    let used = (app + stats.wire_count as u64 + stats.compressor_page_count as u64) * page;
    (total, used.min(total))
}

pub fn cpu_count() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)
}

fn sysctl_u64(name: &CStr) -> Option<u64> {
    let mut value: u64 = 0;
    let mut size = size_of::<u64>();
    let ok = unsafe {
        libc::sysctlbyname(name.as_ptr(), &mut value as *mut _ as *mut c_void, &mut size, std::ptr::null_mut(), 0)
    };
    (ok == 0).then_some(value)
}

/// wtp and every process above it: the shell, terminal or agent that ran
/// it. None of these is ever climbed into or signalled.
pub fn own_lineage() -> std::collections::HashSet<i32> {
    let mut lineage = std::collections::HashSet::new();
    let mut pid = std::process::id() as i32;
    while pid > 1 && lineage.insert(pid) {
        match snapshot(pid) {
            Some(p) => pid = p.ppid,
            None => break,
        }
    }
    lineage
}

/// True when `pid` is still the process that started at `start`, so a reused
/// pid is never signalled.
pub fn is_same_process(pid: i32, start: u64) -> bool {
    if pid <= 1 || pid == std::process::id() as i32 {
        return false;
    }
    snapshot(pid).is_some_and(|p| p.start == start)
}

fn c_string(ptr: *const c_char) -> String {
    unsafe { CStr::from_ptr(ptr) }.to_string_lossy().into_owned()
}
