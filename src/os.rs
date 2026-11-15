//! Process and descriptor primitives the rest of the app builds on.

use anyhow::{Result, ensure};
use std::{
    ffi::c_int,
    fs::{self, File},
    io::{self, ErrorKind, Write},
    os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd},
    os::unix::{fs::PermissionsExt, process::CommandExt},
    process::{Child, Command},
    time::Instant,
};

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

/// A private RAM file holding `bytes` that nobody can change afterwards.
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

/// SIGKILL a child started with `process_group(0)` and everything it spawned.
pub fn kill_group(child: &Child) {
    unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL) };
}

/// A descriptor that becomes readable once `child` exits.
pub fn exit_fd(child: &Child) -> io::Result<OwnedFd> {
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, child.id(), 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd as _) })
}

pub fn set_nonblocking(fd: BorrowedFd) -> io::Result<()> {
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
pub struct Waker(OwnedFd);

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

impl AsFd for Waker {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

/// Block until one of `fds` is readable or hung up, or until `deadline`.
/// Each result is true when that descriptor has something to report.
pub fn wait_readable<const N: usize>(
    fds: [Option<BorrowedFd>; N],
    deadline: Option<Instant>,
) -> io::Result<[bool; N]> {
    let mut descriptors = fds.map(|fd| libc::pollfd {
        fd: fd.map_or(-1, |fd| fd.as_raw_fd()),
        events: libc::POLLIN,
        revents: 0,
    });
    loop {
        let timeout = deadline.map(|deadline| {
            let remaining = deadline.saturating_duration_since(Instant::now());
            libc::timespec {
                tv_sec: remaining.as_secs() as _,
                tv_nsec: remaining.subsec_nanos() as _,
            }
        });
        let result = unsafe {
            libc::ppoll(
                descriptors.as_mut_ptr(),
                descriptors.len() as _,
                timeout.as_ref().map_or(std::ptr::null(), |timeout| timeout),
                std::ptr::null(),
            )
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
