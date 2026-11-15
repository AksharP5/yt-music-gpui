#[cfg(unix)]
use std::{fs::File, io::Read, os::fd::AsRawFd, thread};
use std::{io, sync::OnceLock};

// Keep the descriptor alive until process exit, including during stalled cleanup.
#[cfg(unix)]
static WAKE: OnceLock<File> = OnceLock::new();

#[cfg(unix)]
extern "C" fn terminate(_: libc::c_int) {
    #[cfg(target_os = "linux")]
    let errno = unsafe { libc::__errno_location() };
    #[cfg(target_os = "macos")]
    let errno = unsafe { libc::__error() };
    // listen() sets WAKE before installing this handler.
    let Some(wake) = WAKE.get() else {
        return;
    };
    let saved_errno = unsafe { *errno };
    let increment = 1_u64.to_ne_bytes();
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

#[cfg(unix)]
pub fn listen() -> io::Result<async_channel::Receiver<()>> {
    let (mut wakeups, wake) = signal_channel()?;
    WAKE.set(wake)
        .map_err(|_| io::Error::other("Shutdown signal handlers are already installed"))?;
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

/// Windows sends Ctrl+C, Ctrl+Break and closing the console to this
/// handler, on a thread of its own.
#[cfg(windows)]
pub fn listen() -> io::Result<async_channel::Receiver<()>> {
    use std::sync::atomic::{AtomicBool, Ordering};
    use windows_sys::{
        Win32::{
            Foundation::{FALSE, TRUE},
            System::Console::SetConsoleCtrlHandler,
        },
        core::BOOL,
    };
    static SENDER: OnceLock<async_channel::Sender<()>> = OnceLock::new();
    static NOTIFIED: AtomicBool = AtomicBool::new(false);
    unsafe extern "system" fn handle(_: u32) -> BOOL {
        // A repeated event must still terminate if GPUI or a destructor hangs.
        if NOTIFIED.swap(true, Ordering::SeqCst) {
            return FALSE;
        }
        // listen() sets SENDER before installing this handler.
        if let Some(sender) = SENDER.get() {
            let _ = sender.try_send(());
        }
        TRUE
    }
    let (sender, receiver) = async_channel::bounded(1);
    SENDER
        .set(sender)
        .map_err(|_| io::Error::other("Shutdown handlers are already installed"))?;
    if unsafe { SetConsoleCtrlHandler(Some(handle), TRUE) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(receiver)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{BufRead, BufReader},
        process::{Child, Command, Stdio},
        thread,
        time::Duration,
    };
    #[cfg(windows)]
    use windows_sys::Win32::{
        Foundation::STATUS_CONTROL_C_EXIT,
        System::{Console::CTRL_BREAK_EVENT, Threading::CREATE_NEW_PROCESS_GROUP},
    };

    struct SignalProcess(Child);

    impl Drop for SignalProcess {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[cfg(unix)]
    const EVENTS: [libc::c_int; 2] = [libc::SIGTERM, libc::SIGINT];
    // Ctrl+C cannot target one process; Ctrl+Break reaches its process group.
    #[cfg(windows)]
    const EVENTS: [u32; 1] = [CTRL_BREAK_EVENT];

    #[cfg(unix)]
    fn send(child: &Child, signal: libc::c_int) {
        assert_eq!(unsafe { libc::kill(child.id() as i32, signal) }, 0);
    }

    #[cfg(windows)]
    fn send(child: &Child, event: u32) {
        use windows_sys::Win32::System::Console::GenerateConsoleCtrlEvent;
        assert_ne!(unsafe { GenerateConsoleCtrlEvent(event, child.id()) }, 0);
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

        for event in EVENTS {
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "shutdown::tests::signals_notify_then_terminate_stalled_shutdown",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .stdout(Stdio::piped());
            #[cfg(windows)]
            std::os::windows::process::CommandExt::creation_flags(
                &mut command,
                CREATE_NEW_PROCESS_GROUP,
            );
            let mut child = SignalProcess(command.spawn().unwrap());
            let mut output = BufReader::new(child.0.stdout.take().unwrap()).lines();
            assert!(output.by_ref().any(|line| line.unwrap() == "READY"));
            send(&child.0, event);
            assert!(output.by_ref().any(|line| line.unwrap() == "QUITTING"));
            send(&child.0, event);
            let status = child.0.wait().unwrap();
            #[cfg(unix)]
            assert_eq!(
                std::os::unix::process::ExitStatusExt::signal(&status),
                Some(event)
            );
            #[cfg(windows)]
            assert_eq!(status.code(), Some(STATUS_CONTROL_C_EXIT));
        }
    }
}
