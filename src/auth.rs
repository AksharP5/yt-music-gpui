use anyhow::{Context, Result, ensure};
use reqwest::header::HeaderValue;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha1::{Digest, Sha1};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{
            fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
            net::UnixStream,
            process::CommandExt,
        },
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub const ACCOUNT_LOGIN_ENABLED: bool = true;
pub const ORIGIN: &str = "https://music.youtube.com";
pub type SharedSession = Arc<RwLock<Option<Arc<Session>>>>;

#[derive(Clone, Default)]
pub struct Operation(Arc<AtomicBool>);
impl Operation {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
    pub fn matches(&self, other: &Self) -> bool {
        !self.is_cancelled() && Arc::ptr_eq(&self.0, &other.0)
    }
}
const KEYRING_ATTRIBUTES: [&str; 4] = [
    "application",
    "yt-music-gpui",
    "credential",
    "youtube-session",
];

// Never derive Debug for reusable account credentials.
#[derive(Clone, Serialize, Deserialize)]
pub struct Session {
    cookie: String,
    playback_cookie: String,
    pub authuser: String,
    pub delegated_session_id: Option<String>,
    data_sync_id: String,
    pub client_version: String,
}

pub struct PlaybackCredentials {
    pub(crate) cookies: File,
    pub(crate) identity: File,
}

#[derive(Deserialize)]
struct BrowserAccount {
    authuser: String,
    delegated_session_id: Option<String>,
    data_sync_id: String,
    client_version: String,
}
#[derive(Deserialize)]
struct Cookie {
    name: String,
    value: String,
    domain: String,
    path: String,
    secure: bool,
    #[serde(default)]
    expires: f64,
}

impl Session {
    /// Called on the account worker. The OS keyring may ask the user to unlock it.
    pub fn load() -> Result<Option<Self>> {
        let output = Command::new("/usr/bin/secret-tool")
            .arg("lookup")
            .args(KEYRING_ATTRIBUTES)
            .stdin(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .context("The OS keyring is unavailable")?;
        if !output.status.success() && output.stdout.is_empty() && output.stderr.is_empty() {
            return Ok(None);
        }
        ensure!(
            output.status.success(),
            "Could not unlock the saved YouTube Music account"
        );
        let session: Self = serde_json::from_slice(&output.stdout)
            .context("The keyring session is unreadable. Connect your account again.")?;
        session.validate()?;
        Ok(Some(session))
    }
    pub(crate) fn save(&self) -> Result<()> {
        self.validate()?;
        // A RAM stdin file cannot block on a full pipe while the keyring is locked.
        let mut input = sealed_file(&serde_json::to_vec(self)?)?;
        input.rewind()?;
        let mut command = Command::new("/usr/bin/secret-tool");
        command
            .args(["store", "--label=YouTube Music GPUI"])
            .args(KEYRING_ATTRIBUTES)
            .stdin(Stdio::from(input))
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut process = KeyringProcess::spawn(&mut command)?;
        ensure!(
            process.wait(Duration::from_secs(30))?.success(),
            "The OS keyring could not save the account. No plaintext fallback is used."
        );
        Ok(())
    }
    pub fn disconnect() -> Result<()> {
        let mut command = Command::new("/usr/bin/secret-tool");
        command
            .arg("clear")
            .args(KEYRING_ATTRIBUTES)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut process = KeyringProcess::spawn(&mut command)?;
        ensure!(
            process.wait(Duration::from_secs(30))?.success(),
            "Could not remove the saved account from the keyring"
        );
        Ok(())
    }
    pub fn validate_playback_identity(&self) -> Result<()> {
        ensure!(
            self.authuser.parse::<u32>().is_ok()
                && !self.authuser.is_empty()
                && self.authuser.bytes().all(|c| c.is_ascii_digit()),
            "Invalid Google account index"
        );
        let (first, second) = self
            .data_sync_id
            .split_once("||")
            .context("The saved account has no playback identity. Connect your account again.")?;
        let valid_id = |id: &str| {
            !id.is_empty()
                && id.len() <= 256
                && id
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
        };
        ensure!(
            valid_id(first) && (second.is_empty() || valid_id(second)),
            "Invalid Google playback identity. Connect your account again."
        );
        let delegated = (!second.is_empty()).then_some(first);
        ensure!(
            delegated == self.delegated_session_id.as_deref(),
            "The Google playback identity does not match the selected account. Connect again."
        );
        Ok(())
    }
    pub fn verify_server_identity(&self, response: &Value) -> Result<()> {
        self.validate_playback_identity()?;
        ensure!(
            response["LOGGED_IN"] != false
                && response.pointer("/responseContext/mainAppWebResponseContext/loggedOut")
                    != Some(&json!(true)),
            "YouTube Music did not accept the account session. Connect again."
        );
        let identities = [
            response.get("DATASYNC_ID"),
            response.pointer("/responseContext/mainAppWebResponseContext/datasyncId"),
        ];
        ensure!(
            identities
                .iter()
                .flatten()
                .all(|value| value.as_str() == Some(self.data_sync_id.as_str())),
            "YouTube Music returned a different account"
        );
        ensure!(
            identities.iter().any(Option::is_some),
            "YouTube Music did not confirm the selected account"
        );
        if let Some(index) = response.get("SESSION_INDEX") {
            ensure!(
                index.as_str() == Some(self.authuser.as_str())
                    || index.as_u64() == self.authuser.parse().ok(),
                "YouTube Music returned a different account"
            );
        }
        let (first, second) = self
            .data_sync_id
            .split_once("||")
            .expect("validated playback identity");
        let user = if second.is_empty() { first } else { second };
        for (key, expected) in [
            ("USER_SESSION_ID", json!(user)),
            ("DELEGATED_SESSION_ID", json!(self.delegated_session_id)),
        ] {
            if let Some(value) = response.get(key) {
                ensure!(
                    value == &expected,
                    "YouTube Music returned a different account"
                );
            }
        }
        Ok(())
    }
    fn validate(&self) -> Result<()> {
        ensure!(
            !self.cookie.contains(['\r', '\n']) && self.cookie.len() <= 64 * 1024,
            "Invalid Google session cookies"
        );
        ensure!(
            self.sapisid().is_some(),
            "Sign in to Google in the account connection window first."
        );
        self.validate_playback_identity()?;
        Ok(())
    }
    fn sapisid(&self) -> Option<&str> {
        let cookie = |name| {
            self.cookie
                .split(';')
                .filter_map(|p| p.trim().split_once('='))
                .find_map(|(key, value)| (key == name && !value.is_empty()).then_some(value))
        };
        cookie("__Secure-3PAPISID").or_else(|| cookie("SAPISID"))
    }
    pub fn authorization(&self) -> Result<String> {
        self.authorization_at(timestamp())
    }
    fn authorization_at(&self, timestamp: u64) -> Result<String> {
        let sapisid = self
            .sapisid()
            .context("Google session expired. Connect your account again.")?;
        let digest = Sha1::digest(format!("{timestamp} {sapisid} {ORIGIN}"));
        Ok(format!("SAPISIDHASH {timestamp}_{digest:x}"))
    }
    pub fn apply(
        &self,
        request: reqwest::blocking::RequestBuilder,
    ) -> Result<reqwest::blocking::RequestBuilder> {
        let mut cookie =
            HeaderValue::from_str(&self.cookie).context("Invalid session cookie header")?;
        cookie.set_sensitive(true);
        let mut authorization = HeaderValue::from_str(&self.authorization()?)
            .context("Invalid account authorization")?;
        authorization.set_sensitive(true);
        Ok(request
            .header("Cookie", cookie)
            .header("Authorization", authorization)
            .header("X-Goog-AuthUser", &self.authuser)
            .header("X-Origin", ORIGIN))
    }
    pub fn playback_credentials(&self) -> Result<PlaybackCredentials> {
        self.validate()?;
        let identity = serde_json::to_vec(&json!({
            "authuser": self.authuser,
            "delegated_session_id": self.delegated_session_id,
            "data_sync_id": self.data_sync_id,
        }))?;
        Ok(PlaybackCredentials {
            cookies: self.playback_file()?,
            identity: sealed_file(&identity)?,
        })
    }
    /// Closing the last descriptor destroys the anonymous sealed RAM file.
    pub fn playback_file(&self) -> Result<File> {
        sealed_file(self.playback_cookie.as_bytes())
    }
}

pub(crate) fn sealed_file(bytes: &[u8]) -> Result<File> {
    let fd = unsafe {
        libc::memfd_create(
            c"yt-music-session".as_ptr(),
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
        )
    };
    ensure!(fd >= 0, "Could not create private playback credentials");
    let mut file = unsafe { File::from_raw_fd(fd) };
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    file.write_all(bytes)?;
    let seals = libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL;
    ensure!(
        unsafe { libc::fcntl(file.as_raw_fd(), libc::F_ADD_SEALS, seals) } >= 0,
        "Could not protect playback credentials"
    );
    Ok(file)
}

struct KeyringProcess(Child);
impl KeyringProcess {
    fn spawn(command: &mut Command) -> Result<Self> {
        let parent = std::process::id() as libc::pid_t;
        unsafe {
            command.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::getppid() != parent {
                    libc::_exit(0);
                }
                Ok(())
            });
        }
        Ok(Self(
            command.spawn().context("The OS keyring is unavailable")?,
        ))
    }
    fn wait(&mut self, timeout: Duration) -> Result<std::process::ExitStatus> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.0.try_wait()? {
                return Ok(status);
            }
            ensure!(
                Instant::now() < deadline,
                "The OS keyring operation timed out"
            );
            thread::sleep(Duration::from_millis(25));
        }
    }
}
impl Drop for KeyringProcess {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_some() {
            return;
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

pub fn shared(session: Option<Session>) -> SharedSession {
    Arc::new(RwLock::new(session.map(Arc::new)))
}
fn timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn youtube_domain(domain: &str) -> bool {
    let domain = domain.trim_start_matches('.');
    domain == "youtube.com" || domain.ends_with(".youtube.com")
}

struct LoginBrowser {
    child: Child,
    _profile: tempfile::TempDir,
}
impl Drop for LoginBrowser {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_some() {
            return;
        }
        unsafe {
            libc::kill(self.child.id() as libc::pid_t, libc::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if self.child.try_wait().ok().flatten().is_some() {
                return;
            }
            thread::sleep(Duration::from_millis(25));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct BrowserPipe {
    stream: UnixStream,
    pending: Vec<u8>,
    next_id: u64,
}
impl BrowserPipe {
    fn call(&mut self, method: &str, params: Value, session: Option<&str>) -> Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        let mut request = json!({"id":id,"method":method,"params":params});
        if let Some(session) = session {
            request["sessionId"] = json!(session);
        }
        let mut message = serde_json::to_vec(&request)?;
        message.push(0);
        self.stream.write_all(&message)?;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            while let Some(end) = self.pending.iter().position(|c| *c == 0) {
                let line: Vec<_> = self.pending.drain(..=end).collect();
                let value: Value = serde_json::from_slice(&line[..end])
                    .context("Invalid account connection response")?;
                if value["id"] == id {
                    ensure!(
                        value.get("error").is_none(),
                        "The account connection could not complete this operation"
                    );
                    return Ok(value["result"].clone());
                }
            }
            ensure!(
                Instant::now() < deadline,
                "The sign-in browser stopped responding"
            );
            let mut bytes = [0; 16384];
            let count = self
                .stream
                .read(&mut bytes)
                .context("Lost the private sign-in connection")?;
            ensure!(count > 0, "The sign-in window closed");
            self.pending.extend_from_slice(&bytes[..count]);
            ensure!(
                self.pending.len() <= 1024 * 1024,
                "Oversized account connection response"
            );
        }
    }
}

fn private_runtime() -> Result<PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .context("A private desktop runtime directory is required for account connection")?;
    let metadata = fs::symlink_metadata(&runtime)?;
    ensure!(
        metadata.is_dir()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0,
        "The desktop runtime directory must be private to your user"
    );
    let runtime_name = std::ffi::CString::new(runtime.as_os_str().as_encoded_bytes())?;
    let mut filesystem = std::mem::MaybeUninit::<libc::statfs>::uninit();
    ensure!(
        unsafe { libc::statfs(runtime_name.as_ptr(), filesystem.as_mut_ptr()) } == 0,
        "Could not inspect the private runtime directory"
    );
    ensure!(
        unsafe { filesystem.assume_init() }.f_type == libc::TMPFS_MAGIC,
        "Sign-in requires a RAM-backed private runtime directory"
    );
    Ok(runtime)
}

pub fn lock_instance() -> Result<File> {
    lock_file(&private_runtime()?.join("yt-music-gpui.lock"))
}

fn lock_file(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .context("Could not open the private music application lock")?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o777 == 0o600
            && metadata.nlink() == 1,
        "The music application lock must be a private regular file"
    );
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::WouldBlock {
            return Err(anyhow::anyhow!(
                "YouTube Music is already running. Close its window before opening another instance."
            ));
        }
        return Err(error).context("Could not lock the music application");
    }
    Ok(file)
}

fn start_browser(url: &str, headless: bool) -> Result<(LoginBrowser, BrowserPipe)> {
    // Bypass distribution launch scripts so flags/extensions from another profile
    // cannot be inherited by the sign-in window.
    let binary = ["/usr/lib/chromium/chromium", "/opt/google/chrome/chrome"]
        .into_iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
        .context("Account connection needs Chromium or Google Chrome")?;
    let profile = tempfile::Builder::new()
        .prefix("yt-music-login-")
        .tempdir_in(private_runtime()?)?;
    let (parent, child) = UnixStream::pair()?;
    parent.set_read_timeout(Some(Duration::from_secs(5)))?;
    parent.set_write_timeout(Some(Duration::from_secs(5)))?;
    let fd = child.as_raw_fd();
    let parent_pid = std::process::id() as libc::pid_t;
    let mut command = Command::new(binary);
    command
        .arg(format!("--user-data-dir={}", profile.path().display()))
        .args([
            "--remote-debugging-pipe",
            "--disable-extensions",
            "--disable-sync",
            "--disable-background-mode",
            "--password-store=gnome-libsecret",
            "--disable-crash-reporter",
            "--no-first-run",
            "--no-default-browser-check",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command.arg(format!("--app={url}"));
    if headless {
        command.arg("--headless=new");
    }
    unsafe {
        command.pre_exec(move || {
            for target in [3, 4] {
                if libc::dup2(fd, target) < 0 || libc::fcntl(target, libc::F_SETFD, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::getppid() != parent_pid {
                libc::_exit(0);
            }
            Ok(())
        });
    }
    let browser = LoginBrowser {
        child: command
            .spawn()
            .context("Could not open the isolated Google sign-in window")?,
        _profile: profile,
    };
    drop(child);
    Ok((
        browser,
        BrowserPipe {
            stream: parent,
            pending: Vec::new(),
            next_id: 0,
        },
    ))
}

/// Explicit user action only. Credentials stay in the OS keyring and RAM.
pub fn connect(operation: &Operation) -> Result<(Session, crate::catalog::Account)> {
    ensure!(
        ACCOUNT_LOGIN_ENABLED,
        "Account connection is disabled until the security review is complete."
    );
    ensure!(
        Path::new("/usr/bin/secret-tool").is_file(),
        "Account connection requires the desktop OS keyring"
    );
    let (mut browser, mut pipe) = start_browser(ORIGIN, false)?;
    let deadline = Instant::now() + Duration::from_secs(15 * 60);
    loop {
        ensure!(!operation.is_cancelled(), "Account connection cancelled");
        ensure!(
            browser.child.try_wait()?.is_none(),
            "The Google sign-in window closed before connection finished"
        );
        ensure!(
            Instant::now() < deadline,
            "Account connection timed out. Try again when you are ready to sign in."
        );
        if let Some((session, account)) = capture(&mut pipe)? {
            ensure!(!operation.is_cancelled(), "Account connection cancelled");
            let _ = pipe.call("Browser.close", json!({}), None);
            return Ok((session, account));
        }
        thread::sleep(Duration::from_secs(2));
    }
}

fn capture(pipe: &mut BrowserPipe) -> Result<Option<(Session, crate::catalog::Account)>> {
    let targets = pipe.call("Target.getTargets", json!({}), None)?;
    let target = targets["targetInfos"]
        .as_array()
        .context("No sign-in browser targets")?
        .iter()
        .find(|t| {
            t["type"] == "page"
                && t["url"]
                    .as_str()
                    .is_some_and(|s| s.starts_with(&format!("{ORIGIN}/")))
        });
    let Some(target) = target else {
        return Ok(None);
    };
    let attached = pipe.call(
        "Target.attachToTarget",
        json!({"targetId":target["targetId"],"flatten":true}),
        None,
    )?;
    let id = attached["sessionId"]
        .as_str()
        .context("Could not attach to the sign-in window")?;
    let result = (|| {
        let account=pipe.call("Runtime.evaluate",json!({"expression":"(() => { const c = window.ytcfg; return c?.get('LOGGED_IN') ? { authuser: String(c.get('SESSION_INDEX') ?? 0), delegated_session_id: c.get('DELEGATED_SESSION_ID') ?? null, data_sync_id: c.get('DATASYNC_ID'), client_version: c.get('INNERTUBE_CLIENT_VERSION') } : null; })()","returnByValue":true}),Some(id))?;
        if account["result"]["value"].is_null() {
            return Ok(None);
        }
        let account: BrowserAccount = serde_json::from_value(account["result"]["value"].clone())
            .context("Google did not return a valid music account")?;
        let data = pipe.call("Network.getCookies", json!({"urls":[ORIGIN]}), Some(id))?;
        let cookies: Vec<Cookie> = serde_json::from_value(data["cookies"].clone())
            .context("Google did not return a music session")?;
        let cookies: Vec<_> = cookies
            .into_iter()
            .filter(|c| {
                youtube_domain(&c.domain) && (c.expires <= 0. || c.expires > timestamp() as f64)
            })
            .collect();
        let mut playback_cookie = "# Netscape HTTP Cookie File\n".to_owned();
        for c in &cookies {
            playback_cookie.push_str(&format!(
                "{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
                c.domain,
                if c.domain.starts_with('.') {
                    "TRUE"
                } else {
                    "FALSE"
                },
                c.path,
                if c.secure { "TRUE" } else { "FALSE" },
                c.expires.max(0.) as u64,
                c.name,
                c.value
            ));
        }
        let session = Session {
            cookie: cookies
                .iter()
                .map(|c| format!("{}={}", c.name, c.value))
                .collect::<Vec<_>>()
                .join("; "),
            playback_cookie,
            authuser: account.authuser,
            delegated_session_id: account.delegated_session_id,
            data_sync_id: account.data_sync_id,
            client_version: account.client_version,
        };
        session.validate()?;
        let account =
            crate::catalog::Catalog::with_session(Some(Arc::new(session.clone())))?.account()?;
        Ok(Some((session, account)))
    })();
    let _ = pipe.call("Target.detachFromTarget", json!({"sessionId":id}), None);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_one_instance_can_own_account_state() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("application.lock");
        let owner = lock_file(&path).unwrap();
        assert!(lock_file(&path).is_err());
        drop(owner);
        assert!(lock_file(&path).is_ok());

        let link = directory.path().join("linked.lock");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(lock_file(&link).is_err());
    }
    fn session(cookie: &str) -> Session {
        Session {
            cookie: cookie.into(),
            playback_cookie: "# Netscape HTTP Cookie File\n".into(),
            authuser: "0".into(),
            delegated_session_id: None,
            data_sync_id: "synthetic-user||".into(),
            client_version: String::new(),
        }
    }
    #[test]
    fn a_timed_out_keyring_process_is_terminated_and_reaped() {
        let mut command = Command::new("/usr/bin/sleep");
        command.arg("30");
        let mut process = KeyringProcess::spawn(&mut command).unwrap();
        let pid = process.0.id() as libc::pid_t;
        assert!(process.wait(Duration::from_millis(20)).is_err());
        drop(process);
        assert_eq!(
            unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) },
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
    }
    #[test]
    fn exact_cookie_names_and_fresh_authorization() {
        let s = session("NOT_SAPISID=wrong; SAPISID=correct");
        assert_eq!(s.sapisid(), Some("correct"));
        assert_ne!(
            s.authorization_at(100).unwrap(),
            s.authorization_at(101).unwrap()
        );
        assert_eq!(
            session("__Secure-3PAPISID=preferred; SAPISID=other").sapisid(),
            Some("preferred")
        );
    }
    #[test]
    fn cookies_are_restricted_to_youtube_domains() {
        for domain in ["youtube.com", ".youtube.com", "music.youtube.com"] {
            assert!(youtube_domain(domain));
        }
        for domain in ["evil-youtube.com", "youtube.com.evil.test"] {
            assert!(!youtube_domain(domain));
        }
    }
    #[test]
    fn playback_identity_requires_consistent_primary_or_brand_context() {
        let mut s = session("SAPISID=synthetic");
        s.authuser = "2".into();
        assert!(s.validate_playback_identity().is_ok());
        s.data_sync_id = "brand||user".into();
        assert!(s.validate_playback_identity().is_err());
        s.delegated_session_id = Some("brand".into());
        assert!(s.validate_playback_identity().is_ok());
        s.data_sync_id.clear();
        assert!(s.validate_playback_identity().is_err());
    }
    #[test]
    fn credentials_are_redacted_in_request_debug() {
        let request = session("SAPISID=sensitive-cookie")
            .apply(reqwest::blocking::Client::new().get(ORIGIN))
            .unwrap()
            .build()
            .unwrap();
        let debug = format!("{request:?}");
        assert!(!debug.contains("sensitive-cookie"));
        assert!(request.headers()["cookie"].is_sensitive());
        assert!(request.headers()["authorization"].is_sensitive());
    }
    #[test]
    fn playback_credentials_are_anonymous_and_read_only() {
        let credentials = session("SAPISID=value").playback_credentials().unwrap();
        for mut file in [credentials.cookies, credentials.identity] {
            assert_eq!(file.metadata().unwrap().permissions().mode() & 0o777, 0o600);
            assert!(
                fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))
                    .unwrap()
                    .to_string_lossy()
                    .contains("memfd:")
            );
            assert_ne!(
                unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
                0
            );
            assert!(file.write_all(b"changed").is_err());
        }
    }
    #[test]
    fn browser_pipe_handles_events_and_fragmented_messages() {
        let (parent, mut child) = UnixStream::pair().unwrap();
        let worker = thread::spawn(move || {
            let mut b = [0; 4096];
            loop {
                let count = child.read(&mut b).unwrap();
                assert!(count > 0);
                if b[..count].contains(&0) {
                    break;
                }
            }
            child
                .write_all(b"{\"method\":\"event\"}\0{\"id\":1,\"res")
                .unwrap();
            child.write_all(b"ult\":{\"ok\":true}}\0").unwrap();
        });
        let mut pipe = BrowserPipe {
            stream: parent,
            pending: Vec::new(),
            next_id: 0,
        };
        assert_eq!(pipe.call("Test", json!({}), None).unwrap()["ok"], true);
        worker.join().unwrap();
    }
    #[test]
    #[ignore = "requires Chromium and a private desktop runtime directory"]
    fn live_private_browser_transport() {
        let (mut browser, mut pipe) = start_browser("about:blank", true).unwrap();
        let version = pipe.call("Browser.getVersion", json!({}), None).unwrap();
        assert!(version["product"].as_str().unwrap().contains("Chrome"));
        let args = fs::read(format!("/proc/{}/cmdline", browser.child.id())).unwrap();
        let args = String::from_utf8_lossy(&args);
        assert!(args.contains("--disable-extensions"));
        assert!(!args.contains("--remote-debugging-port"));
        assert!(!args.contains("--load-extension"));
        assert!(!browser._profile.path().join("DevToolsActivePort").exists());
        let _ = pipe.call("Browser.close", json!({}), None);
        let _ = browser.child.try_wait();
    }
}
