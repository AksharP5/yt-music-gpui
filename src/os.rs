//! Process and descriptor primitives the rest of the app builds on.
//! os_windows.rs has the same interface for Windows.

use anyhow::Result;
#[cfg(target_os = "linux")]
use anyhow::ensure;
use std::{
    ffi::c_int,
    fs::File,
    io::{self, ErrorKind},
    ops::{Deref, DerefMut},
    os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd},
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Child, ChildStdout, Command},
    time::Instant,
};
#[cfg(target_os = "linux")]
use std::{fs, io::Write, os::unix::fs::PermissionsExt};

/// mpv's IPC connection.
pub use std::os::unix::net::UnixStream as IpcStream;

/// What `wait_readable` waits on.
pub type Waitable<'a> = BorrowedFd<'a>;

pub trait AsWaitable {
    fn as_waitable(&self) -> Waitable<'_>;
}

impl<T: AsFd> AsWaitable for T {
    fn as_waitable(&self) -> Waitable<'_> {
        self.as_fd()
    }
}

/// Where mpv listens for the player: a socket in the private runtime directory.
pub fn ipc_path(runtime: &Path) -> Result<PathBuf> {
    Ok(runtime.join("mpv.sock"))
}

/// A crash dump must not persist reusable account credentials.
pub fn disable_core_dumps() -> io::Result<()> {
    let limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    if unsafe { libc::setrlimit(libc::RLIMIT_CORE, &limit) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// An anonymous, owner-only RAM file. Closing the last descriptor destroys it.
#[cfg(target_os = "linux")]
pub fn private_file() -> Result<File> {
    let fd = unsafe {
        libc::memfd_create(
            c"yt-music-gpui".as_ptr(),
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
        )
    };
    ensure!(fd >= 0, "Could not create private in-memory storage");
    let file = unsafe { File::from_raw_fd(fd) };
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

/// macOS has no anonymous RAM files. This one is created owner-only in the
/// per-user temporary directory and unlinked at once, so no path reaches it.
#[cfg(target_os = "macos")]
pub fn private_file() -> Result<File> {
    Ok(tempfile::tempfile()?)
}

/// A private RAM file holding `bytes` that nobody can change afterwards.
#[cfg(target_os = "linux")]
pub fn sealed_file(bytes: &[u8]) -> Result<File> {
    let mut file = private_file()?;
    file.write_all(bytes)?;
    let seals = libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL;
    ensure!(
        unsafe { libc::fcntl(file.as_raw_fd(), libc::F_ADD_SEALS, seals) } >= 0,
        "Could not protect private in-memory storage"
    );
    Ok(file)
}

/// Ask the kernel to send `signal` to the child if the spawning thread dies
/// without running its destructors (for example when the app is killed).
#[cfg(target_os = "linux")]
pub fn exit_with_parent(command: &mut Command, signal: c_int) {
    let parent = std::process::id() as libc::pid_t;
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, signal) != 0 {
                return Err(io::Error::last_os_error());
            }
            // The parent died before the hook was armed, so nothing would stop the child.
            if libc::getppid() != parent {
                return Err(io::Error::from_raw_os_error(libc::ESRCH));
            }
            Ok(())
        });
    }
}

/// macOS has no parent-death signal. A killed app instead closes the child's
/// pipes: the resident resolver exits at the end of its stdin, and a one-shot
/// extractor exits once it has written its single answer.
#[cfg(target_os = "macos")]
pub fn exit_with_parent(_: &mut Command, _: c_int) {}

/// A helper in a process group of its own, so it can be killed together
/// with everything it started. It also dies with the spawning thread where
/// `exit_with_parent` can arrange that.
pub struct Group(Child);

impl Group {
    pub fn spawn(command: &mut Command) -> io::Result<Self> {
        command.process_group(0);
        exit_with_parent(command, libc::SIGKILL);
        command.spawn().map(Self)
    }

    /// SIGKILL the helper and everything it started.
    pub fn kill(&self) {
        unsafe { libc::kill(-(self.0.id() as libc::pid_t), libc::SIGKILL) };
    }
}

impl Deref for Group {
    type Target = Child;

    fn deref(&self) -> &Child {
        &self.0
    }
}

impl DerefMut for Group {
    fn deref_mut(&mut self) -> &mut Child {
        &mut self.0
    }
}

/// A child's stdout, which `read` returns `WouldBlock` from instead of waiting.
pub type ChildOutput = ChildStdout;

pub fn child_output(stdout: ChildStdout) -> io::Result<ChildOutput> {
    set_nonblocking(stdout.as_fd())?;
    Ok(stdout)
}

/// What `exit_notice` returns.
pub type ExitNotice = OwnedFd;

/// A descriptor that becomes readable once `child` exits.
#[cfg(target_os = "linux")]
pub fn exit_notice(child: &Child) -> io::Result<ExitNotice> {
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, child.id(), 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd as _) })
}

/// A descriptor that becomes readable once `child` exits: a kqueue watching it.
#[cfg(target_os = "macos")]
pub fn exit_notice(child: &Child) -> io::Result<ExitNotice> {
    let queue = unsafe { libc::kqueue() };
    if queue < 0 {
        return Err(io::Error::last_os_error());
    }
    let queue = unsafe { OwnedFd::from_raw_fd(queue) };
    if unsafe { libc::fcntl(queue.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let event = |ident, filter, fflags| libc::kevent {
        ident,
        filter,
        flags: libc::EV_ADD,
        fflags,
        data: 0,
        udata: std::ptr::null_mut(),
    };
    let register = |event: libc::kevent| {
        let result = unsafe {
            libc::kevent(
                queue.as_raw_fd(),
                &event,
                1,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
            )
        };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    };
    match register(event(child.id() as _, libc::EVFILT_PROC, libc::NOTE_EXIT)) {
        // The child already exited, so the queue must report that at once.
        Err(error) if error.raw_os_error() == Some(libc::ESRCH) => {
            register(event(0, libc::EVFILT_USER, libc::NOTE_TRIGGER))?
        }
        result => result?,
    }
    Ok(queue)
}

fn set_nonblocking(fd: BorrowedFd) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    if flags < 0
        || unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// A descriptor other threads make readable to interrupt `wait_readable`.
/// Wakeups coalesce: any number of `wake` calls before a `drain` count as one.
#[cfg(target_os = "linux")]
pub struct Waker(OwnedFd);

#[cfg(target_os = "linux")]
impl Waker {
    pub fn new() -> io::Result<Self> {
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(unsafe { OwnedFd::from_raw_fd(fd) }))
    }

    pub fn wake(&self) {
        let increment = 1_u64.to_ne_bytes();
        loop {
            let result = unsafe {
                libc::write(
                    self.0.as_raw_fd(),
                    increment.as_ptr().cast(),
                    increment.len(),
                )
            };
            if result == increment.len() as isize {
                return;
            }
            let error = io::Error::last_os_error();
            match error.kind() {
                ErrorKind::Interrupted => continue,
                // A saturated eventfd is already readable, so no wakeup is lost.
                ErrorKind::WouldBlock => return,
                _ => unreachable!("The owned wakeup must remain writable: {error}"),
            }
        }
    }

    /// Clear pending wakeups so the next wait blocks again.
    pub fn drain(&self) -> io::Result<()> {
        let mut count = [0_u8; 8];
        loop {
            let result =
                unsafe { libc::read(self.0.as_raw_fd(), count.as_mut_ptr().cast(), count.len()) };
            if result == count.len() as isize {
                return Ok(());
            }
            let error = io::Error::last_os_error();
            match error.kind() {
                ErrorKind::Interrupted => continue,
                ErrorKind::WouldBlock => return Ok(()),
                _ => return Err(error),
            }
        }
    }
}

#[cfg(target_os = "linux")]
impl AsFd for Waker {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

/// macOS has no eventfd, so wakeups are bytes in a nonblocking pipe.
#[cfg(target_os = "macos")]
pub struct Waker {
    read: OwnedFd,
    write: OwnedFd,
}

#[cfg(target_os = "macos")]
impl Waker {
    pub fn new() -> io::Result<Self> {
        let (read, write) = io::pipe()?;
        let (read, write) = (OwnedFd::from(read), OwnedFd::from(write));
        set_nonblocking(read.as_fd())?;
        set_nonblocking(write.as_fd())?;
        Ok(Self { read, write })
    }

    pub fn wake(&self) {
        loop {
            let result = unsafe { libc::write(self.write.as_raw_fd(), [1_u8].as_ptr().cast(), 1) };
            if result == 1 {
                return;
            }
            let error = io::Error::last_os_error();
            match error.kind() {
                ErrorKind::Interrupted => continue,
                // A full pipe is already readable, so no wakeup is lost.
                ErrorKind::WouldBlock => return,
                _ => unreachable!("The owned wakeup must remain writable: {error}"),
            }
        }
    }

    /// Clear pending wakeups so the next wait blocks again.
    pub fn drain(&self) -> io::Result<()> {
        let mut bytes = [0_u8; 64];
        loop {
            let result = unsafe {
                libc::read(
                    self.read.as_raw_fd(),
                    bytes.as_mut_ptr().cast(),
                    bytes.len(),
                )
            };
            if result > 0 {
                continue;
            }
            if result == 0 {
                return Ok(());
            }
            let error = io::Error::last_os_error();
            match error.kind() {
                ErrorKind::Interrupted => continue,
                ErrorKind::WouldBlock => return Ok(()),
                _ => return Err(error),
            }
        }
    }
}

#[cfg(target_os = "macos")]
impl AsFd for Waker {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.read.as_fd()
    }
}

/// Block until one of `fds` is readable or hung up, or until `deadline`.
/// Each result is true when that descriptor has something to report.
pub fn wait_readable<const N: usize>(
    fds: [Option<Waitable>; N],
    deadline: Option<Instant>,
) -> io::Result<[bool; N]> {
    let mut descriptors = fds.map(|fd| libc::pollfd {
        fd: fd.map_or(-1, |fd| fd.as_raw_fd()),
        events: libc::POLLIN,
        revents: 0,
    });
    loop {
        #[cfg(target_os = "linux")]
        let result = {
            let timeout = deadline.map(|deadline| {
                let remaining = deadline.saturating_duration_since(Instant::now());
                libc::timespec {
                    tv_sec: remaining.as_secs() as _,
                    tv_nsec: remaining.subsec_nanos() as _,
                }
            });
            unsafe {
                libc::ppoll(
                    descriptors.as_mut_ptr(),
                    descriptors.len() as _,
                    timeout.as_ref().map_or(std::ptr::null(), |timeout| timeout),
                    std::ptr::null(),
                )
            }
        };
        // macOS has no ppoll. Round up so a wait never ends just before its deadline.
        #[cfg(target_os = "macos")]
        let result = {
            let timeout = deadline.map_or(-1, |deadline| {
                let remaining = deadline.saturating_duration_since(Instant::now());
                remaining
                    .as_nanos()
                    .div_ceil(1_000_000)
                    .try_into()
                    .unwrap_or(c_int::MAX)
            });
            unsafe { libc::poll(descriptors.as_mut_ptr(), descriptors.len() as _, timeout) }
        };
        if result >= 0 {
            return Ok(descriptors.map(|descriptor| descriptor.revents != 0));
        }
        let error = io::Error::last_os_error();
        if error.kind() != ErrorKind::Interrupted {
            return Err(error);
        }
    }
}
