use clap::Parser;
use rkyv::{Archive, Deserialize, Serialize, rancor::Error, util::AlignedVec};
#[cfg(target_os = "linux")]
use rustix::fd::AsFd;
use rustix::fd::{AsRawFd, OwnedFd, RawFd};
use rustix::fs::{Mode, OFlags};
use rustix::io::Errno;
use rustix::net::{
    AddressFamily, SendFlags, SocketAddrUnix, SocketFlags, SocketType, connect, send, socket_with,
};
use rustix::process::{Pid, Signal, WaitOptions, getpid, getppid, kill_process, waitpid};
#[cfg(target_os = "linux")]
use rustix::process::{
    PidfdFlags, WaitId, WaitIdOptions, pidfd_open, pidfd_send_signal,
    set_parent_process_death_signal, waitid,
};
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const MAX_IPC_FRAME_LEN: usize = 1024 * 1024;

#[derive(Parser, Debug)]
#[command(
    name = "bwmon-publisher",
    about = "Reads interface statistics across network namespaces and writes counters atomically",
    version
)]
pub struct Cli {
    /// Path to network devices configuration file
    #[arg(short, long, default_value = "/etc/bwmon-devices", value_name = "FILE")]
    pub config: PathBuf,

    /// Destination path for bandwidth monitoring state
    #[arg(short, long, default_value = "/run/bwmon.state", value_name = "FILE")]
    pub outfile: PathBuf,

    /// Sampling interval in seconds
    #[arg(short, long, default_value_t = 1.0, value_name = "SECONDS")]
    pub interval: f64,

    /// Run once and exit instead of looping continuously
    #[arg(long)]
    pub once: bool,

    /// Enable verbose logging to stderr
    #[arg(short, long)]
    pub verbose: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DeviceEntry {
    /// "-" indicates root/init namespace; otherwise contains the target namespace name
    pub netns: String,
    pub iface: String,
}

/// Serialized payload transmitted from child worker to parent
#[derive(Archive, Serialize, Deserialize, Debug)]
#[rkyv(derive(Debug))]
pub struct DeviceStat {
    pub iface: String,
    pub rx: u64,
    pub tx: u64,
}

/// Validates interface and network namespace identifiers against path traversal
fn is_valid_identifier(name: &str) -> bool {
    !name.is_empty()
        && name != "-"
        && name != "."
        && name.len() <= 64
        && !name.contains('/')
        && !name.contains('\0')
        && !name.contains("..")
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
}

/// Parses the devices configuration file.
/// Supports both "- <iface>" and "<iface>" for root namespace, as well as "<netns> <iface>".
/// Filters duplicate entries and sorts root ("-") first, then applies natural/human ordering to netns and iface.
fn parse_config(path: &Path, verbose: bool) -> std::io::Result<Vec<DeviceEntry>> {
    let content = std::fs::read_to_string(path)?;
    let mut entries = Vec::new();
    let mut seen = HashSet::new();

    for (line_num, raw_line) in content.lines().enumerate() {
        // Skip empty lines or comments
        let line = raw_line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }

        let parts: Vec<&str> = line.split_whitespace().collect();
        // Valid forms:
        //   "<iface>"          -> root namespace (shorthand)
        //   "- <iface>"        -> root namespace
        //   "<netns> <iface>"  -> named namespace
        let (netns, iface) = match parts.as_slice() {
            [iface] if *iface != "-" => ("-", *iface),
            [netns, iface] => {
                if *netns == "-" {
                    ("-", *iface)
                } else {
                    (*netns, *iface)
                }
            }
            _ => {
                // Wrong field count: lone "-", or 3+ tokens (extra tokens
                // used to be silently discarded).
                if verbose {
                    eprintln!(
                        "Warning: {}:{}: expected 1 or 2 fields \
                         (<iface> | \"- <iface>\" | <netns> <iface>), got {}; skipping line {:?}",
                        path.display(),
                        line_num + 1,
                        parts.len(),
                        line
                    );
                }
                continue;
            }
        };

        if netns != "-" && !is_valid_identifier(netns) {
            if verbose {
                eprintln!(
                    "Warning: invalid network namespace '{}' at {}:{}, skipping",
                    netns,
                    path.display(),
                    line_num + 1
                );
            }
            continue;
        }

        if !is_valid_identifier(iface) {
            if verbose {
                eprintln!(
                    "Warning: invalid interface name '{}' at {}:{}, skipping",
                    iface,
                    path.display(),
                    line_num + 1
                );
            }
            continue;
        }

        let entry = DeviceEntry { netns: netns.to_string(), iface: iface.to_string() };

        // Discard duplicate namespace/device combinations
        if seen.insert(entry.clone()) {
            entries.push(entry);
        } else if verbose {
            eprintln!(
                "Notice: duplicate entry '{} {}' at {}:{}, skipping",
                netns,
                iface,
                path.display(),
                line_num + 1
            );
        }
    }

    // Sort entries: root namespace ("-") first, then natural/human ordering for netns and iface
    entries.sort_by(|a, b| {
        let ns_cmp = match (a.netns.as_str(), b.netns.as_str()) {
            ("-", "-") => std::cmp::Ordering::Equal,
            ("-", _) => std::cmp::Ordering::Less,
            (_, "-") => std::cmp::Ordering::Greater,
            (ns_a, ns_b) => natord::compare(ns_a, ns_b),
        };

        ns_cmp.then_with(|| natord::compare(&a.iface, &b.iface))
    });

    Ok(entries)
}

/// Reads statistics for a given interface from sysfs.
/// Used directly in-process for root namespace, and by worker children inside each netns.
fn read_device_stats(iface: &str) -> (u64, u64) {
    let rx_path = format!("/sys/class/net/{}/statistics/rx_bytes", iface);
    let tx_path = format!("/sys/class/net/{}/statistics/tx_bytes", iface);

    let rx = std::fs::read_to_string(rx_path)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0);

    let tx = std::fs::read_to_string(tx_path)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0);

    (rx, tx)
}

/// Writes a framed byte payload (4-byte length prefix + payload) in a single contiguous write/sendto syscall.
fn send_framed_bytes(stream: &mut UnixStream, payload: &[u8]) -> std::io::Result<()> {
    let len = u32::try_from(payload.len())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "payload too large"))?
        .to_le_bytes();
    let mut packet = Vec::with_capacity(4 + payload.len());
    packet.extend_from_slice(&len);
    packet.extend_from_slice(payload);
    stream.write_all(&packet)
}

/// Reads a framed byte payload from the stream into a 16-byte aligned buffer required by rkyv.
fn recv_framed_bytes(stream: &mut UnixStream) -> std::io::Result<AlignedVec> {
    let mut len_bytes = [0u8; 4];
    stream.read_exact(&mut len_bytes)?;
    let len = u32::from_le_bytes(len_bytes) as usize;
    if len > MAX_IPC_FRAME_LEN {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "IPC frame exceeds maximum allowed size",
        ));
    }
    let mut buf = AlignedVec::with_capacity(len);
    buf.resize(len, 0);
    stream.read_exact(buf.as_mut_slice())?;
    Ok(buf)
}

/// Forcefully kills and reaps a child process using pidfd on Linux (avoiding PID recycling races)
/// or portable rustix kill_process/waitpid across other UNIX systems, retrying if interrupted (EINTR).
fn kill_and_reap(pid: Pid, #[cfg(target_os = "linux")] pidfd: Option<OwnedFd>) {
    #[cfg(target_os = "linux")]
    if let Some(fd) = pidfd {
        let _ = pidfd_send_signal(&fd, Signal::KILL);
        while let Err(Errno::INTR) = waitid(WaitId::PidFd(fd.as_fd()), WaitIdOptions::EXITED) {}
        return;
    }

    let _ = kill_process(pid, Signal::KILL);
    while let Err(Errno::INTR) = waitpid(Some(pid), WaitOptions::empty()) {}
}

/// Returns the (st_dev, st_ino) identity of `/run/netns/<netns>` if it currently exists.
fn stat_netns(netns: &str) -> Option<(u64, u64)> {
    if !is_valid_identifier(netns) {
        return None;
    }
    let path_str = format!("/run/netns/{}", netns);
    let st = rustix::fs::stat(path_str.as_str()).ok()?;
    Some((st.st_dev as u64, st.st_ino as u64))
}

/// Native systemd service notification helper implementing the sd_notify protocol.
pub struct SystemdNotifier {
    fd: Option<OwnedFd>,
}

impl Default for SystemdNotifier {
    fn default() -> Self {
        Self::new()
    }
}

impl SystemdNotifier {
    pub fn new() -> Self {
        let socket_env = match std::env::var("NOTIFY_SOCKET") {
            Ok(val) if !val.is_empty() => val,
            _ => return Self { fd: None },
        };

        let Ok(fd) =
            socket_with(AddressFamily::UNIX, SocketType::DGRAM, SocketFlags::CLOEXEC, None)
        else {
            return Self { fd: None };
        };

        let bytes = socket_env.as_bytes();
        // Ensure path fits inside sun_path (leaving space for leading null if abstract, or trailing null if path)
        let addr = if let Some(stripped) = bytes.strip_prefix(b"@") {
            // In Linux abstract namespace, sun_path starts with a null byte followed by the socket name
            #[cfg(any(target_os = "linux", target_os = "android"))]
            {
                let Ok(a) = SocketAddrUnix::new_abstract_name(stripped) else {
                    return Self { fd: None };
                };
                a
            }
            #[cfg(not(any(target_os = "linux", target_os = "android")))]
            {
                let _ = stripped;
                return Self { fd: None };
            }
        } else {
            let Ok(a) = SocketAddrUnix::new(socket_env.as_str()) else {
                return Self { fd: None };
            };
            a
        };

        if connect(&fd, &addr).is_err() {
            return Self { fd: None };
        }

        Self { fd: Some(fd) }
    }

    pub fn notify(&self, state: &str) {
        if let Some(ref fd) = self.fd {
            #[cfg(not(target_vendor = "apple"))]
            let flags = SendFlags::NOSIGNAL;
            #[cfg(target_vendor = "apple")]
            let flags = SendFlags::empty();

            let _ = send(fd, state.as_bytes(), flags);
        }
    }

    pub fn notify_status(&self, status: &str) {
        self.notify(&format!("STATUS={}\n", status));
    }

    pub fn notify_stopping(&self) {
        self.notify("STOPPING=1\nSTATUS=Stopping bwmon-publisher...\n");
    }

    /// Raw fd of the notify socket, so forked worker children can
    /// close their inherited copy.
    pub fn raw_fd(&self) -> Option<RawFd> {
        self.fd.as_ref().map(|fd| fd.as_raw_fd())
    }
}

/// Long-lived worker process tracking an active network namespace
struct NetnsWorker {
    /// Some(pid) while child is un-reaped; None if already reaped via waitid/waitpid(WNOHANG)
    pid: Option<Pid>,
    /// Process file descriptor on Linux for race-free signaling and reaping
    #[cfg(target_os = "linux")]
    pidfd: Option<OwnedFd>,
    stream: UnixStream,
    ns_dev: u64,
    ns_ino: u64,
}

impl NetnsWorker {
    /// Checks if the child worker has exited without blocking.
    /// If exited, marks `self.pid = None` (and drops `pidfd` on Linux) so `Drop` will not kill/wait a recycled PID.
    fn has_exited(&mut self) -> Option<Pid> {
        let pid = self.pid?;

        #[cfg(target_os = "linux")]
        if let Some(ref pidfd) = self.pidfd {
            loop {
                match waitid(
                    WaitId::PidFd(pidfd.as_fd()),
                    WaitIdOptions::EXITED | WaitIdOptions::NOHANG,
                ) {
                    Ok(None) => return None,
                    Ok(Some(_)) => {
                        self.pid = None;
                        self.pidfd = None;
                        return Some(pid);
                    }
                    Err(Errno::INTR) => continue,
                    Err(_) => {
                        // ECHILD or unexpected error: child no longer exists
                        self.pid = None;
                        self.pidfd = None;
                        return Some(pid);
                    }
                }
            }
        }

        loop {
            match waitpid(Some(pid), WaitOptions::NOHANG) {
                Ok(None) => return None,
                Ok(Some(_)) => {
                    self.pid = None;
                    return Some(pid);
                }
                Err(Errno::INTR) => continue,
                Err(_) => {
                    // ECHILD or unexpected error: child no longer exists
                    self.pid = None;
                    return Some(pid);
                }
            }
        }
    }
}

impl Drop for NetnsWorker {
    fn drop(&mut self) {
        // Close communication channel and reap child process immediately if still running
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
        if let Some(pid) = self.pid.take() {
            // Guarantee instantaneous termination so waitid/waitpid never blocks deactivation
            kill_and_reap(
                pid,
                #[cfg(target_os = "linux")]
                self.pidfd.take(),
            );
        }
    }
}

/// Reaps any workers whose child process was killed or terminated, removing them from the map.
/// Returns the number of dead workers removed.
fn reap_dead_workers(workers: &mut HashMap<String, NetnsWorker>, verbose: bool) -> usize {
    let mut removed = 0;
    workers.retain(|ns, worker| {
        if let Some(dead_pid) = worker.has_exited() {
            if verbose {
                eprintln!(
                    "Warning: worker for namespace '{}' (pid {}) terminated unexpectedly",
                    ns,
                    dead_pid.as_raw_nonzero()
                );
            }
            removed += 1;
            false
        } else {
            true
        }
    });
    removed
}

/// Event loop executed by the persistent forked child worker inside the target network namespace.
fn child_worker_loop(mut stream: UnixStream) {
    while let Ok(buf) = recv_framed_bytes(&mut stream) {
        let archived_query = match rkyv::access::<rkyv::Archived<Vec<String>>, Error>(&buf) {
            Ok(q) => q,
            Err(_) => break,
        };

        let mut stats: Vec<DeviceStat> = Vec::with_capacity(archived_query.len());
        for dev in archived_query.as_slice() {
            let (rx, tx) = read_device_stats(dev.as_str());
            stats.push(DeviceStat { iface: dev.as_str().to_string(), rx, tx });
        }

        // Serialize all collected stats using rkyv 0.8
        let serialized = match rkyv::to_bytes::<Error>(&stats) {
            Ok(b) => b,
            Err(_) => break,
        };

        // Emits exactly ONE sendto() syscall per tick for all devices in this namespace
        if send_framed_bytes(&mut stream, serialized.as_slice()).is_err() {
            break;
        }
    }

    unsafe {
        libc::_exit(0);
    }
}

/// Spawns a persistent worker child for the given namespace once.
/// If the namespace cannot be entered, returns None without erroring out.
fn spawn_netns_worker(
    netns: &str,
    existing_workers: &HashMap<String, NetnsWorker>,
    notify_fd: Option<RawFd>,
    verbose: bool,
) -> Option<NetnsWorker> {
    if !is_valid_identifier(netns) {
        return None;
    }

    // Only check standard /run/netns/<netns>
    let path_str = format!("/run/netns/{}", netns);
    let Ok(ns_fd) =
        rustix::fs::open(path_str.as_str(), OFlags::RDONLY | OFlags::CLOEXEC, Mode::empty())
    else {
        // Namespace cannot be opened: do not error out or write anything
        return None;
    };

    let ns_stat = rustix::fs::fstat(&ns_fd).ok()?;

    let (parent_stream, child_stream) = UnixStream::pair().ok()?;
    let _ = parent_stream.set_read_timeout(Some(Duration::from_millis(500)));
    let _ = parent_stream.set_write_timeout(Some(Duration::from_millis(500)));

    let parent_pid = getpid();
    let raw_pid = unsafe { libc::fork() };
    if raw_pid < 0 {
        return None;
    }

    if raw_pid == 0 {
        // Child worker process
        drop(parent_stream);

        unsafe {
            // Reset signal dispositions to default in child.
            // Prevents child from inheriting parent's signal-hook handlers which catch SIGTERM.
            libc::signal(libc::SIGTERM, libc::SIG_DFL);
            libc::signal(libc::SIGINT, libc::SIG_DFL);
            libc::signal(libc::SIGCHLD, libc::SIG_DFL);
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);

            // close the systemd notify socket copied across fork.
            // Children never emit notifications, and holding the datagram
            // socket open pins it for the worker's whole lifetime.
            if let Some(fd) = notify_fd {
                libc::close(fd);
            }

            // Close inherited sibling worker sockets so child does not hold active peer references
            for worker in existing_workers.values() {
                libc::close(worker.stream.as_raw_fd());
                #[cfg(target_os = "linux")]
                if let Some(ref pidfd) = worker.pidfd {
                    libc::close(pidfd.as_raw_fd());
                }
            }
        }

        #[cfg(target_os = "linux")]
        {
            // Terminate child if parent process exits unexpectedly
            if set_parent_process_death_signal(Some(Signal::TERM)).is_err()
                || getppid() != Some(parent_pid)
            {
                drop(ns_fd);
                unsafe {
                    libc::_exit(1);
                }
            }

            // Switch to target network namespace
            if unsafe { libc::setns(ns_fd.as_raw_fd(), libc::CLONE_NEWNET) } != 0 {
                drop(ns_fd);
                unsafe {
                    libc::_exit(1);
                }
            }
            drop(ns_fd);

            // Remount private /sys once at worker startup
            let ok = unsafe {
                libc::unshare(libc::CLONE_NEWNS) == 0
                    && libc::mount(
                        c"none".as_ptr(),
                        c"/".as_ptr(),
                        std::ptr::null(),
                        libc::MS_REC | libc::MS_PRIVATE,
                        std::ptr::null(),
                    ) == 0
                    && libc::mount(
                        c"sysfs".as_ptr(),
                        c"/sys".as_ptr(),
                        c"sysfs".as_ptr(),
                        0,
                        std::ptr::null(),
                    ) == 0 // stacking over old /sys is fine
            };
            if !ok {
                unsafe {
                    libc::_exit(3);
                }
            } // parent's handshake read fails → namespace omitted
        }

        #[cfg(not(target_os = "linux"))]
        {
            let _ = parent_pid;
            drop(ns_fd);
            unsafe {
                libc::_exit(1);
            }
        }

        // Notify parent that namespace was successfully entered and /sys remounted
        #[allow(unreachable_code)]
        {
            let mut ready_stream = child_stream;
            if ready_stream.write_all(b"O\n").is_err() {
                unsafe {
                    libc::_exit(2);
                }
            }

            child_worker_loop(ready_stream);
            unsafe {
                libc::_exit(0);
            }
        }
    }

    // Parent process closes child ns_fd and child stream
    drop(ns_fd);
    drop(child_stream);

    let pid = Pid::from_raw(raw_pid)?;

    // Open a pidfd immediately while the child is still unreaped by the parent
    #[cfg(target_os = "linux")]
    let pidfd = pidfd_open(pid, PidfdFlags::empty()).ok();

    let mut worker = NetnsWorker {
        pid: Some(pid),
        #[cfg(target_os = "linux")]
        pidfd,
        stream: parent_stream,
        ns_dev: ns_stat.st_dev as u64,
        ns_ino: ns_stat.st_ino as u64,
    };

    // Read initialization handshake from child (if this fails, dropping `worker` kills and reaps via pidfd)
    let mut ready_buf = [0u8; 2];
    if worker.stream.read_exact(&mut ready_buf).is_err() || &ready_buf != b"O\n" {
        return None;
    }

    if verbose {
        eprintln!("Spawned worker (pid {}) for namespace '{}'", pid.as_raw_nonzero(), netns);
    }

    Some(worker)
}

/// Sends device queries to an existing persistent worker process over the IPC socket.
fn query_worker(
    worker: &mut NetnsWorker,
    devices: &[String],
) -> Option<HashMap<String, (u64, u64)>> {
    let devices_vec = devices.to_vec();
    let query_bytes = rkyv::to_bytes::<Error>(&devices_vec).ok()?;
    send_framed_bytes(&mut worker.stream, query_bytes.as_slice()).ok()?;

    let response_buf = recv_framed_bytes(&mut worker.stream).ok()?;
    let archived_stats =
        rkyv::access::<rkyv::Archived<Vec<DeviceStat>>, Error>(&response_buf).ok()?;

    let mut results = HashMap::with_capacity(archived_stats.len());
    for stat in archived_stats.as_slice() {
        results.insert(stat.iface.as_str().to_string(), (stat.rx.to_native(), stat.tx.to_native()));
    }

    Some(results)
}

/// Performs an atomic file write using tempfile in the destination directory,
/// synchronizes buffers via fsync, and renames over the target path.
fn write_atomic(outfile: &Path, content: &str) -> std::io::Result<()> {
    let parent_dir = match outfile.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    if !parent_dir.exists() {
        std::fs::create_dir_all(parent_dir)?;
    }

    let mut temp =
        tempfile::Builder::new().prefix(".bwmon.").suffix(".tmp").tempfile_in(parent_dir)?;

    // Ensure permissions allow non-root status readers (e.g. Argos GNOME extension)
    let perms = std::fs::Permissions::from_mode(0o644);
    temp.as_file().set_permissions(perms)?;

    temp.write_all(content.as_bytes())?;
    temp.as_file().sync_all()?;
    temp.persist(outfile).map_err(|err| err.error)?;

    Ok(())
}

/// Collects counters across root and named namespaces, writing the state file atomically.
/// Returns the number of currently active, monitored devices written to the state file,
/// or `Ok(None)` if interrupted by a termination signal before publishing.
fn collect_and_publish(
    entries: &[DeviceEntry],
    workers: &mut HashMap<String, NetnsWorker>,
    outfile: &Path,
    term: &AtomicBool,
    notify_fd: Option<RawFd>,
    verbose: bool,
) -> std::io::Result<Option<usize>> {
    // Group all devices by namespace: guarantees only ONE worker per unique namespace name
    let mut netns_groups: HashMap<String, Vec<String>> = HashMap::new();
    for entry in entries {
        if entry.netns != "-" {
            let devs = netns_groups.entry(entry.netns.clone()).or_default();
            if !devs.contains(&entry.iface) {
                devs.push(entry.iface.clone());
            }
        }
    }

    // Reap any workers that terminated since the last tick
    reap_dead_workers(workers, verbose);

    // Prune persistent workers for namespaces removed from the configuration
    // or namespaces that were deleted/recreated on the host (/run/netns/<ns> inode changed).
    workers.retain(|ns, worker| {
        if !netns_groups.contains_key(ns) {
            return false;
        }
        match stat_netns(ns) {
            Some((dev, ino)) if dev == worker.ns_dev && ino == worker.ns_ino => true,
            _ => {
                if verbose {
                    eprintln!(
                        "Notice: namespace '{}' was removed or recreated; dropping old worker",
                        ns
                    );
                }
                false
            }
        }
    });

    // Specific namespace: query counters via persistent workers
    let mut netns_stats: HashMap<(String, String), (u64, u64)> = HashMap::new();
    let mut accessible_namespaces: HashSet<String> = HashSet::new();

    // Query each namespace worker, respawning immediately in the same tick if a worker died
    for (ns, devs) in &netns_groups {
        if term.load(Ordering::Relaxed) {
            return Ok(None);
        }

        for attempt in 0..2 {
            if !workers.contains_key(ns)
                && let Some(worker) = spawn_netns_worker(ns, workers, notify_fd, verbose)
            {
                workers.insert(ns.clone(), worker);
            }

            let Some(worker) = workers.get_mut(ns) else {
                break;
            };

            match query_worker(worker, devs) {
                Some(stats) => {
                    accessible_namespaces.insert(ns.clone());
                    for (dev, (rx, tx)) in stats {
                        netns_stats.insert((ns.clone(), dev), (rx, tx));
                    }
                    break;
                }
                None => {
                    // Worker crashed or was killed mid-query: reap/drop it and retry immediately
                    let dead_pid = worker.has_exited().or(worker.pid);
                    if verbose
                        && let Some(pid) = dead_pid {
                            eprintln!(
                                "Warning: worker for namespace '{}' (pid {}) failed or was killed; {}",
                                ns,
                                pid.as_raw_nonzero(),
                                if attempt == 0 {
                                    "respawning..."
                                } else {
                                    "giving up for this tick"
                                }
                            );
                        }
                    workers.remove(ns);
                }
            }
        }
    }

    // Build the full state content in memory
    let mut buffer = String::new();
    let mut monitored_count = 0;

    for entry in entries {
        if entry.netns == "-" {
            // Init namespace: read directly in-process without forking
            let (rx, tx) = read_device_stats(&entry.iface);
            // Output format mirrors shell script: <netns> <iface> <rx> <tx>
            buffer.push_str(&format!("{} {} {} {}\n", entry.netns, entry.iface, rx, tx));
            monitored_count += 1;
        } else if accessible_namespaces.contains(&entry.netns) {
            let (rx, tx) = netns_stats
                .get(&(entry.netns.clone(), entry.iface.clone()))
                .copied()
                .unwrap_or((0, 0));
            // Output format mirrors shell script: <netns> <iface> <rx> <tx>
            buffer.push_str(&format!("{} {} {} {}\n", entry.netns, entry.iface, rx, tx));
            monitored_count += 1;
        }
        // If namespace cannot be entered: do not error out or write anything
    }

    // Atomic swap guarantees the Argos script never reads a half-written file
    write_atomic(outfile, &buffer)?;

    Ok(Some(monitored_count))
}

fn main() {
    let cli = Cli::parse();

    if !cli.config.exists() {
        eprintln!("Error: {} not found.", cli.config.display());
        std::process::exit(1);
    }

    // flag::register sets the flag to true when SIGINT/SIGTERM/SIGCHLD is delivered
    let term = Arc::new(AtomicBool::new(false));
    let sigchld = Arc::new(AtomicBool::new(false));

    // Register async-signal-safe termination and child-exit hooks
    if let Err(e) = signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&term)) {
        eprintln!("Failed to register SIGINT handler: {}", e);
        std::process::exit(1);
    }
    if let Err(e) = signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&term)) {
        eprintln!("Failed to register SIGTERM handler: {}", e);
        std::process::exit(1);
    }
    if let Err(e) = signal_hook::flag::register(signal_hook::consts::SIGCHLD, Arc::clone(&sigchld))
    {
        eprintln!("Failed to register SIGCHLD handler: {}", e);
        std::process::exit(1);
    }

    if !cli.interval.is_finite() || cli.interval <= 0.0 {
        eprintln!("interval must be a positive finite number");
        std::process::exit(2);
    }
    if cli.verbose {
        eprintln!(
            "Starting bwmon-publisher (config: {}, outfile: {}, interval: {} s)",
            cli.config.display(),
            cli.outfile.display(),
            cli.interval
        );
    }

    let notifier = SystemdNotifier::new();
    let notify_fd = notifier.raw_fd();
    let mut ready_sent = false;
    let mut last_reported_count: Option<usize> = None;

    let interval = Duration::from_secs_f64(cli.interval.max(0.05));
    let mut workers: HashMap<String, NetnsWorker> = HashMap::new();

    while !term.load(Ordering::Relaxed) {
        let tick_start = Instant::now();
        sigchld.store(false, Ordering::Relaxed);

        match parse_config(&cli.config, cli.verbose) {
            Ok(entries) => {
                match collect_and_publish(
                    &entries,
                    &mut workers,
                    &cli.outfile,
                    &term,
                    notify_fd,
                    cli.verbose,
                ) {
                    Ok(Some(count)) => {
                        // Notify systemd when startup/first write finishes or when monitored device count changes
                        if !ready_sent {
                            notifier
                                .notify(&format!("READY=1\nSTATUS=Monitored devices: {}\n", count));
                            ready_sent = true;
                            last_reported_count = Some(count);
                        } else if last_reported_count != Some(count) {
                            notifier.notify_status(&format!("Monitored devices: {}", count));
                            last_reported_count = Some(count);
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        if cli.verbose {
                            eprintln!("Error writing state file: {}", e);
                        }
                    }
                }
            }
            Err(e) => {
                if cli.verbose {
                    eprintln!("Error reading config file '{}': {}", cli.config.display(), e);
                }
            }
        }

        if cli.once || term.load(Ordering::Relaxed) {
            break;
        }

        // Sleep remainder of interval, staying responsive to SIGINT/SIGTERM and child exits (SIGCHLD)
        let elapsed = tick_start.elapsed();
        if elapsed < interval {
            let mut remaining = interval - elapsed;
            let sleep_step = Duration::from_millis(50);
            while remaining > Duration::ZERO && !term.load(Ordering::Relaxed) {
                if sigchld.swap(false, Ordering::Relaxed)
                    && reap_dead_workers(&mut workers, cli.verbose) > 0
                {
                    // Immediately run a collection tick to respawn the killed worker
                    break;
                }
                let this_step = remaining.min(sleep_step);
                std::thread::sleep(this_step);
                remaining = remaining.saturating_sub(this_step);
            }
        }
    }

    // Inform systemd that shutdown sequence has started
    notifier.notify_stopping();

    // Workers map drops here, shutting down streams and immediately reaping all children
    drop(workers);

    if cli.verbose {
        eprintln!("bwmon-publisher terminated gracefully.");
    }
}
