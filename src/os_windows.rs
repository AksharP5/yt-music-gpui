//! Windows versions of the primitives in os.rs, with the same interface.

use anyhow::{Context, Result};
use std::{
    ffi::OsStr,
    fs::File,
    io::{self, ErrorKind, Read, Write},
    ops::{Deref, DerefMut},
    os::windows::{
        ffi::OsStrExt,
        io::{AsHandle, AsRawHandle, BorrowedHandle, FromRawHandle, OwnedHandle},
    },
    path::{Path, PathBuf},
    process::{Child, ChildStdout, Command},
    ptr::{null, null_mut},
    sync::{
        Arc,
        mpsc::{self, TryRecvError},
    },
    thread,
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{
        ERROR_ALREADY_EXISTS, ERROR_BROKEN_PIPE, ERROR_IO_PENDING, ERROR_OPERATION_ABORTED, FALSE,
        GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE, TRUE, WAIT_FAILED,
        WAIT_OBJECT_0, WAIT_TIMEOUT,
    },
    Storage::FileSystem::{
        CreateFileW, FILE_FLAG_OVERLAPPED, OPEN_EXISTING, ReadFile, SECURITY_IDENTIFICATION,
        SECURITY_SQOS_PRESENT, WriteFile,
    },
    System::{
        ErrorReporting::{WER_FAULT_REPORTING_FLAG_NOHEAP, WerSetFlags},
        IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED},
        JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_BASIC_LIMIT_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JobObjectExtendedLimitInformation, SetInformationJobObject, TerminateJobObject,
        },
        Threading::{
            CreateEventW, CreateMutexW, INFINITE, ResetEvent, SetEvent, WaitForMultipleObjects,
            WaitForSingleObject,
        },
    },
};

/// Windows Error Reporting must not copy credentials from the heap into a
/// crash report. Reports still carry stacks; nothing here keeps account data
/// on a stack for long.
pub fn disable_core_dumps() -> io::Result<()> {
    let result = unsafe { WerSetFlags(WER_FAULT_REPORTING_FLAG_NOHEAP) };
    if result < 0 {
        return Err(io::Error::from_raw_os_error(result));
    }
    Ok(())
}

/// Windows has no anonymous RAM files. This one lives in the per-user temp
/// folder, which only the user and administrators can open. tempfile opens
/// it unshared, marked temporary so it stays in the cache, and deleted when
/// the last handle closes.
pub fn private_file() -> Result<File> {
    Ok(tempfile::tempfile()?)
}

/// A helper in a job object of its own, so it can be killed together with
/// everything it started. The job is closed with the app, which kills it, so
/// helpers never outlive the app.
///
/// The helper joins the job just after it starts. Anything it starts before
/// that would escape, but Python and yt-dlp take far longer to start anything.
pub struct Group {
    child: Child,
    job: OwnedHandle,
}

impl Group {
    pub fn spawn(command: &mut Command) -> io::Result<Self> {
        let job = unsafe { CreateJobObjectW(null(), null()) };
        if job.is_null() {
            return Err(io::Error::last_os_error());
        }
        let job = unsafe { OwnedHandle::from_raw_handle(job) };
        let limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION {
            BasicLimitInformation: JOBOBJECT_BASIC_LIMIT_INFORMATION {
                LimitFlags: JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                ..Default::default()
            },
            ..Default::default()
        };
        if unsafe {
            SetInformationJobObject(
                job.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                size_of_val(&limits) as u32,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        let mut child = command.spawn()?;
        if unsafe { AssignProcessToJobObject(job.as_raw_handle(), child.as_raw_handle()) } == 0 {
            let error = io::Error::last_os_error();
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
        Ok(Self { child, job })
    }

    /// Terminate the helper and everything it started.
    pub fn kill(&self) {
        unsafe { TerminateJobObject(self.job.as_raw_handle(), 1) };
    }
}

impl Deref for Group {
    type Target = Child;

    fn deref(&self) -> &Child {
        &self.child
    }
}

impl DerefMut for Group {
    fn deref_mut(&mut self) -> &mut Child {
        &mut self.child
    }
}

/// What `wait_readable` waits on: an event or a process.
pub type Waitable<'a> = BorrowedHandle<'a>;

pub trait AsWaitable {
    fn as_waitable(&self) -> Waitable<'_>;
}

/// What `exit_notice` returns.
pub type ExitNotice = OwnedHandle;

impl AsWaitable for ExitNotice {
    fn as_waitable(&self) -> Waitable<'_> {
        self.as_handle()
    }
}

/// A process handle is signalled once the process exits.
pub fn exit_notice(child: &Child) -> io::Result<ExitNotice> {
    child.as_handle().try_clone_to_owned()
}

fn event() -> io::Result<OwnedHandle> {
    // Manual reset: waiting never clears it, so `wait_readable` can check
    // every handle after the wait returns.
    let event = unsafe { CreateEventW(null(), TRUE, FALSE, null()) };
    if event.is_null() {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedHandle::from_raw_handle(event) })
}

/// An event other threads set to interrupt `wait_readable`.
/// Wakeups coalesce: any number of `wake` calls before a `drain` count as one.
pub struct Waker(OwnedHandle);

impl Waker {
    pub fn new() -> io::Result<Self> {
        event().map(Self)
    }

    #[expect(
        clippy::unreachable,
        reason = "setting an owned event cannot fail, and silently losing a wakeup would hang the player"
    )]
    pub fn wake(&self) {
        if unsafe { SetEvent(self.0.as_raw_handle()) } == 0 {
            unreachable!(
                "The owned wakeup must remain settable: {}",
                io::Error::last_os_error()
            );
        }
    }

    /// Clear pending wakeups so the next wait blocks again.
    pub fn drain(&self) -> io::Result<()> {
        if unsafe { ResetEvent(self.0.as_raw_handle()) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

impl AsWaitable for Waker {
    fn as_waitable(&self) -> Waitable<'_> {
        self.0.as_handle()
    }
}

/// A child's stdout, which `read` returns `WouldBlock` from instead of
/// waiting. Windows cannot poll a pipe, so a thread reads it and sets an
/// event whenever output arrives. The thread ends when the child closes its
/// end, which killing its group does.
pub struct ChildOutput {
    chunks: mpsc::Receiver<io::Result<Vec<u8>>>,
    ready: Arc<Waker>,
    pending: Vec<u8>,
}

pub fn child_output(mut stdout: ChildStdout) -> io::Result<ChildOutput> {
    let ready = Arc::new(Waker::new()?);
    let (sender, chunks) = mpsc::channel();
    let notify = ready.clone();
    thread::Builder::new()
        .name("helper-output".into())
        .spawn(move || {
            let mut buffer = [0; 8192];
            loop {
                let chunk = match stdout.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => Ok(buffer[..count].to_vec()),
                    Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                    Err(error) => Err(error),
                };
                let failed = chunk.is_err();
                if sender.send(chunk).is_err() || failed {
                    break;
                }
                notify.wake();
            }
            // The reader sees the end only after the sender is gone.
            drop(sender);
            notify.wake();
        })?;
    Ok(ChildOutput {
        chunks,
        ready,
        pending: Vec::new(),
    })
}

impl Read for ChildOutput {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pending.is_empty() {
            let chunk = match self.chunks.try_recv() {
                Ok(chunk) => chunk,
                Err(TryRecvError::Disconnected) => return Ok(0),
                Err(TryRecvError::Empty) => {
                    // Clear the event before looking again, so output that
                    // arrives in between sets it again.
                    self.ready.drain()?;
                    match self.chunks.try_recv() {
                        Ok(chunk) => chunk,
                        Err(TryRecvError::Disconnected) => return Ok(0),
                        Err(TryRecvError::Empty) => return Err(ErrorKind::WouldBlock.into()),
                    }
                }
            };
            self.pending = chunk?;
        }
        let count = buf.len().min(self.pending.len());
        buf[..count].copy_from_slice(&self.pending[..count]);
        self.pending.drain(..count);
        if !self.pending.is_empty() {
            self.ready.wake();
        }
        Ok(count)
    }
}

impl AsWaitable for ChildOutput {
    fn as_waitable(&self) -> Waitable<'_> {
        self.ready.0.as_handle()
    }
}

fn wide(text: &OsStr) -> Vec<u16> {
    text.encode_wide().chain([0]).collect()
}

/// Round up fractional milliseconds before passing a timeout to Win32.
fn milliseconds(timeout: Option<Duration>) -> u32 {
    timeout.map_or(INFINITE, |timeout| {
        timeout
            .as_nanos()
            .div_ceil(1_000_000)
            .try_into()
            .unwrap_or(u32::MAX)
            .min(INFINITE - 1)
    })
}

/// Block until one of `handles` is signalled, or until `deadline`.
/// Each result is true when that handle is signalled.
pub fn wait_readable<const N: usize>(
    handles: [Option<Waitable>; N],
    deadline: Option<Instant>,
) -> io::Result<[bool; N]> {
    let raw: Vec<HANDLE> = handles
        .iter()
        .flatten()
        .map(|handle| handle.as_raw_handle())
        .collect();
    if raw.is_empty() {
        let timeout = milliseconds(
            deadline.map(|deadline| deadline.saturating_duration_since(Instant::now())),
        );
        thread::sleep(Duration::from_millis(timeout.into()));
        return Ok([false; N]);
    }
    loop {
        let timeout = milliseconds(
            deadline.map(|deadline| deadline.saturating_duration_since(Instant::now())),
        );
        let result =
            unsafe { WaitForMultipleObjects(raw.len() as u32, raw.as_ptr(), FALSE, timeout) };
        if result == WAIT_FAILED {
            return Err(io::Error::last_os_error());
        }
        // Win32 timeout accuracy follows clock ticks and can expire early.
        if result != WAIT_TIMEOUT || !deadline.is_some_and(|deadline| Instant::now() < deadline) {
            break;
        }
    }
    // The wait names only the first signalled handle, so check each one.
    Ok(handles.map(|handle| {
        handle.is_some_and(|handle| unsafe {
            WaitForSingleObject(handle.as_raw_handle(), 0) == WAIT_OBJECT_0
        })
    }))
}

/// mpv's IPC pipe. Pipe names are global, so the name is unguessable: no
/// other program can create it first. mpv then creates it as the first
/// instance and admits only this user, at this integrity level or higher.
pub fn ipc_path(_: &Path) -> Result<PathBuf> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).context("Could not name the private playback pipe")?;
    let name: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(PathBuf::from(format!(r"\\.\pipe\yt-music-gpui-{name}")))
}

/// A session-wide named mutex, held until the handle closes. None when
/// another process already holds it.
pub fn instance_mutex(name: &str) -> io::Result<Option<OwnedHandle>> {
    let name = wide(OsStr::new(&format!(r"Local\{name}")));
    let mutex = unsafe { CreateMutexW(null(), FALSE, name.as_ptr()) };
    if mutex.is_null() {
        return Err(io::Error::last_os_error());
    }
    let existed = io::Error::last_os_error().raw_os_error() == Some(ERROR_ALREADY_EXISTS as i32);
    let mutex = unsafe { OwnedHandle::from_raw_handle(mutex) };
    Ok((!existed).then_some(mutex))
}

/// A connection to mpv's IPC pipe, with a Unix socket's read and write
/// timeouts. Windows cannot poll a pipe, so one overlapped read is always
/// outstanding and its event tells `wait_readable` when mpv has written.
pub struct IpcStream {
    pipe: OwnedHandle,
    // The kernel writes into this while a read is pending, so it stays at a
    // fixed address and only `&mut self` methods touch it.
    read: Box<PendingRead>,
    write_event: OwnedHandle,
    read_timeout: Option<Duration>,
    write_timeout: Option<Duration>,
}

struct PendingRead {
    overlapped: OVERLAPPED,
    event: OwnedHandle,
    buffer: [u8; 16384],
    state: ReadState,
}

enum ReadState {
    Pending,
    Ready { start: usize, end: usize },
    Ended(Option<io::Error>),
}

// The raw pointers in OVERLAPPED are only used through `&mut self`.
unsafe impl Send for IpcStream {}
unsafe impl Sync for IpcStream {}

impl IpcStream {
    pub fn connect(path: impl AsRef<Path>) -> io::Result<Self> {
        let name = wide(path.as_ref().as_os_str());
        let pipe = unsafe {
            CreateFileW(
                name.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                null(),
                OPEN_EXISTING,
                // mpv may identify this client but never act as it.
                FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                null_mut(),
            )
        };
        if pipe == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        Self::new(unsafe { OwnedHandle::from_raw_handle(pipe) })
    }

    fn new(pipe: OwnedHandle) -> io::Result<Self> {
        let mut stream = Self {
            pipe,
            read: Box::new(PendingRead {
                overlapped: OVERLAPPED::default(),
                event: event()?,
                buffer: [0; 16384],
                state: ReadState::Ended(None),
            }),
            write_event: event()?,
            read_timeout: None,
            write_timeout: None,
        };
        stream.start_read();
        Ok(stream)
    }

    /// Two connected ends, as with `UnixStream::pair`.
    #[cfg(test)]
    pub fn pair() -> io::Result<(Self, Self)> {
        use windows_sys::Win32::{
            Foundation::ERROR_PIPE_CONNECTED,
            Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX},
            System::Pipes::{
                ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_TYPE_BYTE, PIPE_WAIT,
            },
        };
        let path = ipc_path(Path::new("")).map_err(io::Error::other)?;
        let server = unsafe {
            CreateNamedPipeW(
                wide(path.as_os_str()).as_ptr(),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED | FILE_FLAG_FIRST_PIPE_INSTANCE,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                65536,
                65536,
                0,
                null(),
            )
        };
        if server == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let server = unsafe { OwnedHandle::from_raw_handle(server) };
        let client = Self::connect(&path)?;
        let connected = event()?;
        let mut overlapped = OVERLAPPED {
            hEvent: connected.as_raw_handle(),
            ..Default::default()
        };
        if unsafe { ConnectNamedPipe(server.as_raw_handle(), &mut overlapped) } == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(ERROR_IO_PENDING as i32) {
                let mut count = 0;
                let pipe = server.as_raw_handle();
                if unsafe { GetOverlappedResult(pipe, &overlapped, &mut count, TRUE) } == 0 {
                    return Err(io::Error::last_os_error());
                }
            } else if error.raw_os_error() != Some(ERROR_PIPE_CONNECTED as i32) {
                return Err(error);
            }
        }
        Ok((client, Self::new(server)?))
    }

    pub fn set_read_timeout(&mut self, timeout: Option<Duration>) -> io::Result<()> {
        self.read_timeout = timeout;
        Ok(())
    }

    pub fn set_write_timeout(&mut self, timeout: Option<Duration>) -> io::Result<()> {
        self.write_timeout = timeout;
        Ok(())
    }

    fn start_read(&mut self) {
        let read = &mut *self.read;
        read.overlapped = OVERLAPPED {
            hEvent: read.event.as_raw_handle(),
            ..Default::default()
        };
        let started = unsafe {
            ReadFile(
                self.pipe.as_raw_handle(),
                read.buffer.as_mut_ptr(),
                read.buffer.len() as u32,
                null_mut(),
                &mut read.overlapped,
            )
        };
        if started == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(ERROR_IO_PENDING as i32) {
                // Nothing is pending, so signal the end ourselves.
                read.state = ReadState::Ended(
                    (error.raw_os_error() != Some(ERROR_BROKEN_PIPE as i32)).then_some(error),
                );
                unsafe { SetEvent(read.event.as_raw_handle()) };
                return;
            }
        }
        // A read that finished at once has set the event too.
        read.state = ReadState::Pending;
    }
}

impl Read for IpcStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            match &mut self.read.state {
                ReadState::Ready { start, end } => {
                    let (start, end) = (*start, *end);
                    let count = buf.len().min(end - start);
                    buf[..count].copy_from_slice(&self.read.buffer[start..start + count]);
                    if start + count == end {
                        self.start_read();
                    } else {
                        self.read.state = ReadState::Ready {
                            start: start + count,
                            end,
                        };
                    }
                    // An empty completion is not the end of the stream.
                    if count > 0 || buf.is_empty() {
                        return Ok(count);
                    }
                    continue;
                }
                ReadState::Ended(error) => return error.take().map_or(Ok(0), Err),
                ReadState::Pending => {}
            }
            let event = self.read.event.as_raw_handle();
            match unsafe { WaitForSingleObject(event, milliseconds(self.read_timeout)) } {
                WAIT_OBJECT_0 => {}
                WAIT_TIMEOUT => return Err(ErrorKind::TimedOut.into()),
                _ => return Err(io::Error::last_os_error()),
            }
            let mut count = 0;
            let done = unsafe {
                GetOverlappedResult(
                    self.pipe.as_raw_handle(),
                    &self.read.overlapped,
                    &mut count,
                    FALSE,
                )
            };
            self.read.state = if done != 0 {
                ReadState::Ready {
                    start: 0,
                    end: count as usize,
                }
            } else {
                let error = io::Error::last_os_error();
                ReadState::Ended(
                    (error.raw_os_error() != Some(ERROR_BROKEN_PIPE as i32)).then_some(error),
                )
            };
        }
    }
}

impl Write for IpcStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let pipe = self.pipe.as_raw_handle();
        let mut overlapped = OVERLAPPED {
            hEvent: self.write_event.as_raw_handle(),
            ..Default::default()
        };
        let length = buf.len().min(u32::MAX as usize) as u32;
        if unsafe { WriteFile(pipe, buf.as_ptr(), length, null_mut(), &mut overlapped) } == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(ERROR_IO_PENDING as i32) {
                return Err(error);
            }
        }
        let event = self.write_event.as_raw_handle();
        if unsafe { WaitForSingleObject(event, milliseconds(self.write_timeout)) } == WAIT_TIMEOUT {
            unsafe { CancelIoEx(pipe, &overlapped) };
        }
        // Wait even after cancelling: the kernel uses `overlapped` until then.
        let mut count = 0;
        if unsafe { GetOverlappedResult(pipe, &overlapped, &mut count, TRUE) } == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(ERROR_OPERATION_ABORTED as i32) {
                return Err(ErrorKind::TimedOut.into());
            }
            return Err(error);
        }
        Ok(count as usize)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl AsWaitable for IpcStream {
    fn as_waitable(&self) -> Waitable<'_> {
        self.read.event.as_handle()
    }
}

impl Drop for IpcStream {
    fn drop(&mut self) {
        if matches!(self.read.state, ReadState::Pending) {
            let pipe = self.pipe.as_raw_handle();
            let mut count = 0;
            unsafe {
                CancelIoEx(pipe, &self.read.overlapped);
                GetOverlappedResult(pipe, &self.read.overlapped, &mut count, TRUE);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::BufRead, process::Stdio};
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE};

    #[test]
    fn readable_wait_timeouts_reach_the_absolute_deadline() {
        let wake = Waker::new().unwrap();
        for milliseconds in [1, 2, 7, 25, 31] {
            let deadline = Instant::now() + Duration::from_millis(milliseconds);
            assert_eq!(
                wait_readable([Some(wake.as_waitable())], Some(deadline)).unwrap(),
                [false]
            );
            assert!(
                Instant::now() >= deadline,
                "The {milliseconds} ms wait ended early"
            );
        }
    }

    /// A helper that starts a grandchild, prints its process ID and waits.
    fn helper_with_grandchild() -> (Group, u32) {
        let mut command = Command::new("powershell");
        command
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "(Start-Process -PassThru -WindowStyle Hidden ping '-n','60','127.0.0.1').Id; Start-Sleep 60",
            ])
            .stdout(Stdio::piped());
        let mut group = Group::spawn(&mut command).unwrap();
        let mut line = String::new();
        io::BufReader::new(group.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        (group, line.trim().parse().unwrap())
    }

    fn exits_within(pid: u32, milliseconds: u32) -> bool {
        let process = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, FALSE, pid) };
        if process.is_null() {
            return true;
        }
        let process = unsafe { OwnedHandle::from_raw_handle(process) };
        unsafe { WaitForSingleObject(process.as_raw_handle(), milliseconds) == WAIT_OBJECT_0 }
    }

    #[test]
    fn helpers_die_with_their_group_and_with_the_app() {
        let (mut group, grandchild) = helper_with_grandchild();
        assert!(!exits_within(grandchild, 0));
        group.kill();
        group.wait().unwrap();
        assert!(
            exits_within(grandchild, 5000),
            "kill left a grandchild running"
        );

        // Closing the job, as the app's exit does, kills everything in it.
        let (group, grandchild) = helper_with_grandchild();
        let child = group.id();
        drop(group);
        assert!(exits_within(child, 5000) && exits_within(grandchild, 5000));
    }

    #[test]
    fn child_output_wakes_a_wait_and_reports_the_end() {
        let mut child = Command::new("cmd")
            .args(["/c", "echo ready"])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut output = child_output(child.stdout.take().unwrap()).unwrap();
        let mut bytes = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let mut buffer = [0; 4];
            match output.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => bytes.extend_from_slice(&buffer[..count]),
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "output never arrived");
                    let [ready] =
                        wait_readable([Some(output.as_waitable())], Some(deadline)).unwrap();
                    assert!(ready || Instant::now() >= deadline);
                }
                Err(error) => panic!("{error}"),
            }
        }
        assert_eq!(bytes, b"ready\r\n");
        child.wait().unwrap();
    }

    #[test]
    fn only_one_holder_of_an_instance_mutex() {
        let name = format!("yt-music-gpui-test-{}", std::process::id());
        let first = instance_mutex(&name).unwrap().unwrap();
        assert!(instance_mutex(&name).unwrap().is_none());
        drop(first);
        assert!(instance_mutex(&name).unwrap().is_some());
    }
}
