use std::{
    fs::File,
    io::{self, Read},
    os::fd::AsRawFd,
    sync::OnceLock,
    thread,
};

// Keep the descriptor alive until process exit, including during stalled cleanup.
static WAKE: OnceLock<File> = OnceLock::new();

extern "C" fn terminate(_: libc::c_int) {
    #[cfg(target_os = "linux")]
    let errno = unsafe { libc::__errno_location() };
    #[cfg(target_os = "macos")]
    let errno = unsafe { libc::__error() };
    let saved_errno = unsafe { *errno };
    let increment = 1_u64.to_ne_bytes();
    let wake = WAKE.get().unwrap();
    loop {
        let result =
            unsafe { libc::write(wake.as_raw_fd(), increment.as_ptr().cast(), increment.len()) };
        if result >= 0 || unsafe { *errno } != libc::EINTR {
            break;
        }
    }
    unsafe { *errno = saved_errno };
}

/// The ends the signal handler writes to and the shutdown thread reads from.
/// Each handler runs once, so its write can never block.
#[cfg(target_os = "linux")]
fn signal_channel() -> io::Result<(File, File)> {
    use std::os::fd::FromRawFd;
    let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let wake = unsafe { File::from_raw_fd(fd) };
    Ok((wake.try_clone()?, wake))
}

#[cfg(target_os = "macos")]
fn signal_channel() -> io::Result<(File, File)> {
    let (read, write) = io::pipe()?;
    Ok((
        std::os::fd::OwnedFd::from(read).into(),
        std::os::fd::OwnedFd::from(write).into(),
    ))
}

pub fn listen() -> io::Result<async_channel::Receiver<()>> {
    let (mut wakeups, wake) = signal_channel()?;
    WAKE.set(wake)
        .expect("install shutdown signal handlers only once");
    let (sender, receiver) = async_channel::bounded(1);
    thread::Builder::new()
        .name("shutdown-signal".into())
        .spawn(move || match wakeups.read_exact(&mut [0; 8]) {
            Ok(()) => {
                let _ = sender.send_blocking(());
            }
            Err(error) => eprintln!("Could not receive shutdown signal: {error}"),
        })?;
    for signal in [libc::SIGTERM, libc::SIGINT] {
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = terminate as *const () as usize;
        // A repeated signal must still terminate if GPUI or a destructor hangs.
        action.sa_flags = libc::SA_RESETHAND | libc::SA_RESTART;
        unsafe { libc::sigemptyset(&mut action.sa_mask) };
        if unsafe { libc::sigaction(signal, &action, std::ptr::null_mut()) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(receiver)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{BufRead, BufReader},
        os::unix::process::ExitStatusExt,
        process::{Child, Command, Stdio},
        time::Duration,
    };

    struct SignalProcess(Child);

    impl Drop for SignalProcess {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    fn signals_notify_then_terminate_stalled_shutdown() {
        const CHILD: &str = "YT_MUSIC_SIGNAL_TEST_CHILD";
        if std::env::var_os(CHILD).is_some() {
            // Bound subprocess failures so a broken handler cannot hang the suite.
            thread::spawn(|| {
                thread::sleep(Duration::from_secs(10));
                std::process::exit(1);
            });
            let signal = listen().unwrap();
            println!("READY");
            signal.recv_blocking().unwrap();
            println!("QUITTING");
            loop {
                thread::park();
            }
        }

        for signal in [libc::SIGTERM, libc::SIGINT] {
            let mut child = SignalProcess(
                Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "shutdown::tests::signals_notify_then_terminate_stalled_shutdown",
                        "--nocapture",
                    ])
                    .env(CHILD, "1")
                    .stdout(Stdio::piped())
                    .spawn()
                    .unwrap(),
            );
            let mut output = BufReader::new(child.0.stdout.take().unwrap()).lines();
            assert!(output.by_ref().any(|line| line.unwrap() == "READY"));
            assert_eq!(unsafe { libc::kill(child.0.id() as i32, signal) }, 0);
            assert!(output.by_ref().any(|line| line.unwrap() == "QUITTING"));
            assert_eq!(unsafe { libc::kill(child.0.id() as i32, signal) }, 0);
            assert_eq!(child.0.wait().unwrap().signal(), Some(signal));
        }
    }
}
