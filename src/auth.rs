use anyhow::{Context, Result, bail, ensure};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha1::{Digest, Sha1};
use std::{
    fs,
    io::Write,
    net::{TcpStream, ToSocketAddrs},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tungstenite::{Message, WebSocket};

pub const ORIGIN: &str = "https://music.youtube.com";
pub type SharedSession = Arc<RwLock<Option<Session>>>;

// Session values must never implement Debug or appear in error messages or logs.
#[derive(Clone, Serialize, Deserialize)]
pub struct Session {
    cookie: String,
    pub authuser: String,
    pub delegated_session_id: Option<String>,
    pub client_version: String,
}

#[derive(Deserialize)]
struct BrowserAccount {
    authuser: String,
    delegated_session_id: Option<String>,
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
    pub fn load() -> Result<Option<Self>> {
        let path = directory()?.join("session.json");
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).context("Could not read your Google session"),
        };
        let session: Self = serde_json::from_slice(&bytes)
            .context("The saved Google session is unreadable. Connect your account again.")?;
        session.validate()?;
        Ok(Some(session))
    }

    pub fn from_cookie_file(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path).context("Could not read the selected cookie file")?;
        let mut cookies = Vec::new();
        for line in text.lines() {
            let line = line.strip_prefix("#HttpOnly_").unwrap_or(line);
            if line.starts_with('#') || line.trim().is_empty() {
                continue;
            }
            let fields: Vec<_> = line.split('\t').collect();
            if fields.len() != 7 || !youtube_domain(fields[0]) {
                continue;
            }
            let expires = fields[4].parse::<u64>().unwrap_or(0);
            if expires > 0 && expires < timestamp() {
                continue;
            }
            cookies.push(format!("{}={}", fields[5], fields[6]));
        }
        let session = Self {
            cookie: cookies.join("; "),
            authuser: "0".into(),
            delegated_session_id: None,
            client_version: "1.20261013.15.00".into(),
        };
        session.validate()?;
        Ok(session)
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
        ensure!(
            !self.authuser.is_empty() && self.authuser.bytes().all(|c| c.is_ascii_digit()),
            "Invalid Google account index"
        );
        Ok(())
    }

    fn sapisid(&self) -> Option<&str> {
        let cookie = |name| {
            self.cookie
                .split(';')
                .filter_map(|part| part.trim().split_once('='))
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
        Ok(request
            .header("Cookie", &self.cookie)
            .header("Authorization", self.authorization()?)
            .header("X-Goog-AuthUser", &self.authuser)
            .header("X-Origin", ORIGIN))
    }

    pub fn playback_cookies() -> Result<PathBuf> {
        Ok(directory()?.join("cookies.txt"))
    }
}

pub fn shared(session: Option<Session>) -> SharedSession {
    Arc::new(RwLock::new(session))
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

pub fn directory() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".local/share")))
        .context("Could not find your user data directory")?;
    Ok(base.join("yt-music-gpui/account"))
}

fn private_directory(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("Missing account directory")?;
    private_directory(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist(path)
        .context("Could not save your Google session")?;
    Ok(())
}

/// Runs only after the user selects Connect account. The browser profile belongs
/// to this app; no other browser profiles or stored passwords are inspected.
pub fn connect(cancelled: &AtomicBool) -> Result<Session> {
    let profile = directory()?.join("browser");
    private_directory(&profile)?;
    let browser = ["chromium", "google-chrome-stable", "google-chrome"]
        .into_iter()
        .find_map(|name| {
            std::env::var_os("PATH")
                .into_iter()
                .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
                .map(|path| path.join(name))
                .find(|path| path.is_file())
        })
        .context("Account connection needs Chromium or Google Chrome.")?;
    let mut child = Command::new(browser)
        .arg(format!("--user-data-dir={}", profile.display()))
        .args([
            "--remote-debugging-port=0",
            "--remote-debugging-address=127.0.0.1",
            "--no-first-run",
            "--no-default-browser-check",
            "--app=https://music.youtube.com",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("Could not open the Google sign-in window")?;
    let client = Client::builder()
        .timeout(Duration::from_secs(4))
        .no_proxy()
        .build()?;
    let deadline = Instant::now() + Duration::from_secs(15 * 60);
    loop {
        if cancelled.load(Ordering::Relaxed) {
            bail!("Account connection cancelled");
        }
        if child.try_wait()?.is_some() {
            bail!("The account connection window closed before sign-in finished.");
        }
        if Instant::now() > deadline {
            bail!("Account connection timed out. Try again when you are ready to sign in.");
        }
        if let Ok(port_file) = fs::read_to_string(profile.join("DevToolsActivePort"))
            && let Some(port) = port_file
                .lines()
                .next()
                .and_then(|line| line.parse::<u16>().ok())
            && let Ok(session) = capture(&client, port)
        {
            return Ok(session);
        }
        thread::sleep(Duration::from_secs(2));
    }
}

fn capture(client: &Client, port: u16) -> Result<Session> {
    let targets: Vec<Value> = client
        .get(format!("http://127.0.0.1:{port}/json/list"))
        .send()?
        .error_for_status()?
        .json()?;
    let target = targets
        .iter()
        .find(|target| {
            target["type"] == "page"
                && target["url"]
                    .as_str()
                    .is_some_and(|url| url.starts_with(&format!("{ORIGIN}/")))
        })
        .context("Finish Google sign-in first")?;
    let url = target["webSocketDebuggerUrl"]
        .as_str()
        .context("No account connection endpoint")?;
    let parsed = reqwest::Url::parse(url)?;
    ensure!(
        parsed.scheme() == "ws"
            && parsed.host_str() == Some("127.0.0.1")
            && parsed.port() == Some(port),
        "Unexpected account connection endpoint"
    );
    let address = ("127.0.0.1", port)
        .to_socket_addrs()?
        .next()
        .context("No local browser address")?;
    let stream = TcpStream::connect_timeout(&address, Duration::from_secs(3))?;
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    stream.set_write_timeout(Some(Duration::from_secs(3)))?;
    let (mut socket, _) = tungstenite::client(url, stream)
        .map_err(|_| anyhow::anyhow!("Could not connect to the sign-in window"))?;
    let account = cdp(
        &mut socket,
        1,
        "Runtime.evaluate",
        json!({"expression":"(() => { const c = window.ytcfg; return c?.get('LOGGED_IN') ? { authuser: String(c.get('SESSION_INDEX') ?? 0), delegated_session_id: c.get('DELEGATED_SESSION_ID') ?? null, client_version: c.get('INNERTUBE_CLIENT_VERSION') } : null; })()", "returnByValue":true}),
    )?;
    let account: BrowserAccount = serde_json::from_value(account["result"]["value"].clone())
        .context("Finish Google sign-in first")?;
    let cookies = cdp(
        &mut socket,
        2,
        "Network.getCookies",
        json!({"urls":[ORIGIN, "https://www.youtube.com"]}),
    )?;
    let cookies: Vec<Cookie> = serde_json::from_value(cookies["cookies"].clone())
        .context("Could not read the signed-in music session")?;
    let cookies: Vec<_> = cookies
        .into_iter()
        .filter(|c| {
            youtube_domain(&c.domain) && (c.expires <= 0. || c.expires > timestamp() as f64)
        })
        .collect();
    let session = Session {
        cookie: cookies
            .iter()
            .map(|c| format!("{}={}", c.name, c.value))
            .collect::<Vec<_>>()
            .join("; "),
        authuser: account.authuser,
        delegated_session_id: account.delegated_session_id,
        client_version: account.client_version,
    };
    session.validate()?;
    // Do not enable writes or replace a saved session until the service confirms it.
    crate::catalog::Catalog::with_session(shared(Some(session.clone())))?.account()?;
    let mut netscape = "# Netscape HTTP Cookie File\n".to_owned();
    for cookie in &cookies {
        netscape.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
            cookie.domain,
            if cookie.domain.starts_with('.') {
                "TRUE"
            } else {
                "FALSE"
            },
            cookie.path,
            if cookie.secure { "TRUE" } else { "FALSE" },
            cookie.expires.max(0.) as u64,
            cookie.name,
            cookie.value
        ));
    }
    write_private(&Session::playback_cookies()?, netscape.as_bytes())?;
    write_private(
        &directory()?.join("session.json"),
        &serde_json::to_vec(&session)?,
    )?;
    // Close the dedicated login browser after export so it has no steady-state cost.
    let _ = socket.send(Message::Text(
        json!({"id":3,"method":"Browser.close"}).to_string().into(),
    ));
    Ok(session)
}

fn cdp(socket: &mut WebSocket<TcpStream>, id: u64, method: &str, params: Value) -> Result<Value> {
    socket.send(Message::Text(
        json!({"id":id,"method":method,"params":params})
            .to_string()
            .into(),
    ))?;
    loop {
        let message = socket.read()?;
        if let Message::Text(text) = message {
            let value: Value = serde_json::from_str(&text)?;
            if value["id"] == id {
                ensure!(
                    value.get("error").is_none(),
                    "The sign-in window could not complete this operation"
                );
                return Ok(value["result"].clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cookie_names_are_exact_and_authorization_is_fresh() {
        let session = Session {
            cookie: "NOT_SAPISID=wrong; SAPISID=correct".into(),
            authuser: "0".into(),
            delegated_session_id: None,
            client_version: String::new(),
        };
        assert_eq!(session.sapisid(), Some("correct"));
        assert_ne!(
            session.authorization_at(100).unwrap(),
            session.authorization_at(101).unwrap()
        );
        let session = Session {
            cookie: "__Secure-3PAPISID=preferred; SAPISID=other".into(),
            ..session
        };
        assert_eq!(session.sapisid(), Some("preferred"));
    }
    #[test]
    fn imports_http_only_cookies_for_youtube_only() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(file,"# Netscape HTTP Cookie File\n#HttpOnly_.youtube.com\tTRUE\t/\tTRUE\t0\tSAPISID\tvalue\n.evil-youtube.com\tTRUE\t/\tTRUE\t0\tSECRET\twrong").unwrap();
        let session = Session::from_cookie_file(file.path()).unwrap();
        assert_eq!(session.cookie, "SAPISID=value");
    }
}
