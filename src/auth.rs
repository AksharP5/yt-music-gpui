#[cfg(unix)]
use crate::os::exit_with_parent;
#[cfg(any(not(target_os = "macos"), test))]
use crate::os::private_file;
#[cfg(target_os = "linux")]
use crate::os::sealed_file;
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use reqwest::header::HeaderValue;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha1::{Digest, Sha1};
use std::{
    ffi::OsStr,
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[cfg(unix)]
use std::{
    fs::OpenOptions,
    io::Write,
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    },
};
#[cfg(any(not(target_os = "macos"), test))]
use std::{io::Seek, process::Output};

/// Sign-in copies a browser session into the Secret Service on Linux and the Keychain on macOS.
pub const ACCOUNT_LOGIN_ENABLED: bool = cfg!(any(target_os = "linux", target_os = "macos"));
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
#[cfg(not(target_os = "macos"))]
const KEYRING_ATTRIBUTES: [&str; 4] = [
    "application",
    "yt-music-gpui",
    "credential",
    "youtube-session",
];
const KEYRING_PREFIX: &[u8] = b"yt-music-session-v1:";
/// The app's generic password item in the user's default keychain.
#[cfg(target_os = "macos")]
use security_framework_sys::base::errSecItemNotFound;
#[cfg(target_os = "macos")]
const KEYCHAIN_SERVICE: &str = "yt-music-gpui";
#[cfg(target_os = "macos")]
const KEYCHAIN_ACCOUNT: &str = "youtube-session";

/// What the keyring returned for the saved account.
enum KeyringLookup {
    Found(Vec<u8>),
    Absent,
    /// The keyring could not be read: it is locked, refused access or is not running.
    Failed,
}

impl KeyringLookup {
    #[cfg(any(not(target_os = "macos"), test))]
    fn from_secret_tool(output: &Output) -> Self {
        // A silent status 1 means the keyring has no account.
        if output.status.code() == Some(1) && output.stdout.is_empty() && output.stderr.is_empty() {
            Self::Absent
        } else if output.status.success() {
            Self::Found(output.stdout.clone())
        } else {
            Self::Failed
        }
    }
}

#[derive(Debug)]
pub(crate) struct RejectedSession;

impl std::fmt::Display for RejectedSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("YouTube Music did not accept the account session. Connect again.")
    }
}

impl std::error::Error for RejectedSession {}

/// No Secret Service answers on the session bus, so nothing can be saved in a keyring.
#[derive(Debug)]
pub(crate) struct KeyringUnavailable;

impl std::fmt::Display for KeyringUnavailable {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("No desktop keyring is running")
    }
}

impl std::error::Error for KeyringUnavailable {}

/// secret-tool names the Secret Service bus name only when it cannot reach that service.
#[cfg(not(target_os = "macos"))]
fn secret_service_missing(output: &Output) -> bool {
    !output.status.success()
        && String::from_utf8_lossy(&output.stderr).contains("org.freedesktop.secrets")
}

pub(crate) fn session_was_rejected(error: &anyhow::Error) -> bool {
    error.is::<RejectedSession>()
        || error.chain().any(|cause| {
            cause.downcast_ref::<reqwest::Error>().is_some_and(|error| {
                matches!(
                    error.status(),
                    Some(reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN)
                )
            })
        })
}

pub(crate) fn reject_logged_out(response: &Value) -> Result<()> {
    let logged_out = response
        .pointer("/responseContext/serviceTrackingParams")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|service| service["params"].as_array())
        .flatten()
        .any(|parameter| {
            matches!(parameter["key"].as_str(), Some("logged_in" | "yt_li"))
                && parameter["value"] == "0"
        });
    if logged_out
        || response.get("LOGGED_IN") == Some(&json!(false))
        || response.pointer("/responseContext/mainAppWebResponseContext/loggedOut")
            == Some(&json!(true))
    {
        return Err(RejectedSession.into());
    }
    Ok(())
}

// Never derive Debug for reusable account credentials.
#[derive(Clone, Serialize, Deserialize)]
pub struct Session {
    cookie: String,
    playback_cookie: String,
    pub authuser: String,
    pub delegated_session_id: Option<String>,
    data_sync_id: String,
    pub client_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) source_profile: Option<BrowserProfile>,
}

/// What the account extractor reads from its stdin: one JSON line with the
/// Netscape cookie jar and the account to play as.
pub struct PlaybackCredentials {
    pub(crate) message: Vec<u8>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct BrowserProfile {
    pub browser: String,
    pub name: String,
    path: PathBuf,
}

#[derive(Deserialize)]
struct Cookie {
    name: String,
    value: String,
    domain: String,
    path: String,
    secure: bool,
    expires: u64,
}

impl Session {
    /// A saved account usually has a keyring marker or a private session file.
    pub(crate) fn has_saved_account() -> bool {
        crate::dirs::data().is_some_and(|directory| {
            directory.join(MARKER).is_file() || directory.join(SESSION_FILE).is_file()
        })
    }

    /// Called on the account worker. The OS keyring may ask the user to unlock it.
    #[cfg(not(target_os = "macos"))]
    pub fn load(operation: &Operation) -> Result<Option<Self>> {
        let mut command = Command::new("/usr/bin/secret-tool");
        command
            .arg("lookup")
            .args(KEYRING_ATTRIBUTES)
            .stdin(Stdio::null());
        let output =
            AccountProcess::output(&mut command, Some(operation), Duration::from_secs(30))?;
        Self::from_storage(
            KeyringLookup::from_secret_tool(&output),
            crate::dirs::data().as_deref(),
        )
    }
    /// Called on the account worker. macOS may ask the user to unlock the
    /// keychain, or to allow this build of the app to read its item.
    #[cfg(target_os = "macos")]
    pub fn load(_: &Operation) -> Result<Option<Self>> {
        use security_framework::passwords::{PasswordOptions, generic_password};
        let lookup = match generic_password(PasswordOptions::new_generic_password(
            KEYCHAIN_SERVICE,
            KEYCHAIN_ACCOUNT,
        )) {
            Ok(value) => KeyringLookup::Found(value),
            Err(error) if error.code() == errSecItemNotFound => KeyringLookup::Absent,
            Err(_) => KeyringLookup::Failed,
        };
        Self::from_storage(lookup, crate::dirs::data().as_deref())
    }
    /// The keyring wins; the session file is read only when the keyring has no account.
    fn from_storage(lookup: KeyringLookup, directory: Option<&Path>) -> Result<Option<Self>> {
        let Some(directory) = directory else {
            return Self::from_lookup(lookup, None);
        };
        match Self::from_lookup(lookup, Some(&directory.join(MARKER)))? {
            Some(session) => Ok(Some(session)),
            None => read_session_file(&directory.join(SESSION_FILE)),
        }
    }
    /// Without a keyring service, a lookup fails even for people who never signed in.
    /// The marker tells a failure that may hide a saved account apart from that.
    fn from_lookup(lookup: KeyringLookup, marker: Option<&Path>) -> Result<Option<Self>> {
        let value = match lookup {
            KeyringLookup::Found(value) => value,
            KeyringLookup::Absent => {
                // The keyring has no account, for example after deleting it in a keyring app.
                if let Some(marker) = marker {
                    let _ = remove_if_present(marker);
                }
                return Ok(None);
            }
            KeyringLookup::Failed if !marker.is_some_and(Path::exists) => return Ok(None),
            KeyringLookup::Failed => bail!("Could not unlock the saved YouTube Music account"),
        };
        let session = Self::from_saved(&value)?;
        // Accounts saved before the marker existed get it on their next start.
        record_saved_account(marker);
        Ok(Some(session))
    }
    fn from_saved(value: &[u8]) -> Result<Self> {
        let decoded;
        let value = if let Some(encoded) = value.strip_prefix(KEYRING_PREFIX) {
            decoded = STANDARD
                .decode(encoded)
                .context("The saved account is unreadable. Connect again.")?;
            decoded.as_slice()
        } else {
            value
        };
        let session: Self = serde_json::from_slice(value)
            .context("The saved account is unreadable. Connect your account again.")?;
        session.validate()?;
        Ok(session)
    }
    fn saved_value(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let mut value = KEYRING_PREFIX.to_vec();
        value.extend_from_slice(STANDARD.encode(serde_json::to_vec(self)?).as_bytes());
        Ok(value)
    }
    /// Saves to the keyring. Without one, it updates a session file the user already chose.
    pub(crate) fn save(&self) -> Result<()> {
        let value = self.saved_value()?;
        let directory = crate::dirs::data();
        match store_in_keyring(&value) {
            Ok(()) => {
                let Some(directory) = directory else {
                    return Ok(());
                };
                record_saved_account(Some(&directory.join(MARKER)));
                remove_if_present(&directory.join(SESSION_FILE)).context(
                    "Saved the account in the keyring, but could not remove the session file",
                )
            }
            Err(error)
                if error.is::<KeyringUnavailable>()
                    && directory.as_ref().is_some_and(|directory| {
                        fs::symlink_metadata(directory.join(SESSION_FILE)).is_ok()
                    }) =>
            {
                self.save_without_keyring()
            }
            Err(error) => Err(error),
        }
    }
    /// Only after the user explicitly chose a file over the missing keyring.
    pub(crate) fn save_without_keyring(&self) -> Result<()> {
        let directory = crate::dirs::data().context("Could not find your user data directory")?;
        write_session_file(&directory.join(SESSION_FILE), &self.saved_value()?)?;
        // The marker means the keyring holds the account; it no longer does.
        remove_if_present(&directory.join(MARKER)).context("Could not forget the saved account")
    }
    pub fn disconnect() -> Result<()> {
        #[cfg(not(target_os = "macos"))]
        clear_keyring(&KEYRING_ATTRIBUTES)?;
        #[cfg(target_os = "macos")]
        delete_from_keychain()?;
        crate::dirs::data().map_or(Ok(()), |directory| forget_saved(&directory))
    }
    pub(crate) fn resume_scope(&self) -> String {
        format!("{:x}", Sha1::digest(self.data_sync_id.as_bytes()))
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
    pub(crate) fn same_credentials(&self, other: &Self) -> bool {
        self.cookie == other.cookie
            && self.playback_cookie == other.playback_cookie
            && self.authuser == other.authuser
            && self.data_sync_id == other.data_sync_id
            && self.delegated_session_id == other.delegated_session_id
            && self.client_version == other.client_version
    }
    pub(crate) fn require_same_account(&self, refreshed: &Self) -> Result<()> {
        self.validate_playback_identity()?;
        refreshed.validate_playback_identity()?;
        ensure!(
            self.data_sync_id == refreshed.data_sync_id
                && self.delegated_session_id == refreshed.delegated_session_id,
            "The saved browser profile now has a different YouTube account. Reconnect to choose an account."
        );
        Ok(())
    }
    /// Ordinary API responses may omit proof, but any supplied identity must match.
    pub(crate) fn check_response_identity(&self, response: &Value) -> Result<()> {
        self.server_identity_confirmed(response)?;
        Ok(())
    }
    pub fn verify_server_identity(&self, response: &Value) -> Result<()> {
        ensure!(
            self.server_identity_confirmed(response)?,
            "YouTube Music did not confirm the selected account"
        );
        Ok(())
    }
    /// The shared API client must reject redirects before forwarding credentials.
    pub fn verify_account_identity(
        &self,
        response: &Value,
        client: &reqwest::blocking::Client,
    ) -> Result<()> {
        // Missing API proof may use the page bootstrap. Conflicting proof must never retry.
        if self.server_identity_confirmed(response)? {
            return Ok(());
        }
        let mut request = self.apply(
            client
                .get(format!("{ORIGIN}/"))
                .header("Origin", ORIGIN)
                .header("Cache-Control", "no-cache")
                .header("Referer", format!("{ORIGIN}/")),
        )?;
        if let Some(delegated) = &self.delegated_session_id {
            request = request.header("X-Goog-PageId", delegated);
        }
        let indexed = request
            .try_clone()
            .context("Could not prepare account verification")?
            .query(&[("authuser", &self.authuser)]);
        let mut page = indexed
            .send()
            .map_err(reqwest::Error::without_url)
            .context("Could not verify the selected YouTube Music account")?;
        // YouTube strips authuser from the root URL. Retry only its exact canonical
        // root, keeping the account headers and requiring the same identity below.
        if page.status() == reqwest::StatusCode::FOUND
            && matches!(
                page.headers().get("Location").map(HeaderValue::as_bytes),
                Some(b"/" | b"https://music.youtube.com/")
            )
        {
            page = request
                .send()
                .map_err(reqwest::Error::without_url)
                .context("Could not verify the selected YouTube Music account")?;
        }
        if matches!(
            page.status(),
            reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
        ) {
            return Err(RejectedSession.into());
        }
        ensure!(
            page.status() == reqwest::StatusCode::OK,
            "YouTube Music did not return an account verification page (HTTP {})",
            page.status().as_u16()
        );
        self.verify_html_identity(page)
    }
    fn verify_html_identity(&self, page: impl Read) -> Result<()> {
        self.verify_server_identity(&page_config(page)?)
    }
    fn server_identity_confirmed(&self, response: &Value) -> Result<bool> {
        self.validate_playback_identity()?;
        reject_logged_out(response)?;
        ensure!(
            response.get("LOGGED_IN").is_none_or(|value| value == true)
                && response
                    .pointer("/responseContext/mainAppWebResponseContext/loggedOut")
                    .is_none_or(|value| value == false),
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
            .context("The saved account has no playback identity. Connect your account again.")?;
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
        Ok(identities.iter().any(Option::is_some))
    }
    fn validate(&self) -> Result<()> {
        ensure!(
            !self.cookie.contains(['\r', '\n']) && self.cookie.len() <= 64 * 1024,
            "Invalid Google session cookies"
        );
        ensure!(
            self.sapisid().is_some(),
            "Sign in to YouTube Music in your browser first, then retry."
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
        let mut message = serde_json::to_vec(&json!({
            "cookies": self.playback_cookie,
            "account": {
                "authuser": self.authuser,
                "delegated_session_id": self.delegated_session_id,
                "data_sync_id": self.data_sync_id,
            },
        }))?;
        message.push(b'\n');
        Ok(PlaybackCredentials { message })
    }
}

struct AccountProcess(Child);

/// An empty, non-secret file recording that the keyring holds an account.
const MARKER: &str = "account-saved";
/// The session, only when the user chose to save it without a keyring.
const SESSION_FILE: &str = "account-session";

#[cfg(target_os = "linux")]
fn store_in_keyring(value: &[u8]) -> Result<()> {
    // secret-tool's stdin reader truncates input beyond 8192 bytes.
    ensure!(
        value.len() <= 8192,
        "The account session exceeds the desktop keyring helper's storage limit"
    );
    // A RAM stdin file cannot block on a full pipe while the keyring is locked.
    let mut input = sealed_file(value)?;
    input.rewind()?;
    let mut command = Command::new("/usr/bin/secret-tool");
    command
        .args(["store", "--label=YouTube Music GPUI"])
        .args(KEYRING_ATTRIBUTES)
        .stdin(Stdio::from(input));
    let output = AccountProcess::output(&mut command, None, Duration::from_secs(30))?;
    if secret_service_missing(&output) {
        return Err(KeyringUnavailable.into());
    }
    ensure!(
        output.status.success(),
        "The desktop keyring could not save the account"
    );
    Ok(())
}

/// The item's access list holds only the app that created it, so another
/// program, or a changed build of this one, must ask the user first. An
/// existing item is replaced, never updated: an update keeps the old access
/// list, which another program could have opened to everyone.
#[cfg(target_os = "macos")]
fn store_in_keyring(value: &[u8]) -> Result<()> {
    delete_from_keychain()?;
    add_to_keychain(value).context("The macOS Keychain could not save the account")
}

#[cfg(target_os = "macos")]
fn add_to_keychain(value: &[u8]) -> security_framework::base::Result<()> {
    use core_foundation::{
        base::TCFType, data::CFData, dictionary::CFDictionary, string::CFString,
    };
    use security_framework_sys::{
        base::errSecSuccess,
        item::{
            kSecAttrAccount, kSecAttrLabel, kSecAttrService, kSecClass, kSecClassGenericPassword,
            kSecValueData,
        },
        keychain_item::SecItemAdd,
    };

    // The keys and class are Security framework constants. The dictionary
    // retains its values, and SecItemAdd needs no returned item reference.
    let status = unsafe {
        let attributes = [
            (
                kSecClass,
                CFString::wrap_under_get_rule(kSecClassGenericPassword).into_CFType(),
            ),
            (
                kSecAttrService,
                CFString::new(KEYCHAIN_SERVICE).into_CFType(),
            ),
            (
                kSecAttrAccount,
                CFString::new(KEYCHAIN_ACCOUNT).into_CFType(),
            ),
            (
                kSecAttrLabel,
                CFString::new("YouTube Music GPUI").into_CFType(),
            ),
            (kSecValueData, CFData::from_buffer(value).into_CFType()),
        ]
        .map(|(key, value)| (CFString::wrap_under_get_rule(key), value));
        let attributes = CFDictionary::from_CFType_pairs(&attributes);
        SecItemAdd(attributes.as_concrete_TypeRef(), std::ptr::null_mut())
    };
    // A duplicate can appear between delete and add. Updating it would
    // preserve another program's access list.
    if status == errSecSuccess {
        Ok(())
    } else {
        Err(security_framework::base::Error::from_code(status))
    }
}

#[cfg(target_os = "macos")]
fn delete_from_keychain() -> Result<()> {
    match security_framework::passwords::delete_generic_password(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)
    {
        Err(error) if error.code() != errSecItemNotFound => {
            Err(error).context("Could not remove the saved account from the Keychain")
        }
        _ => Ok(()),
    }
}

#[cfg(windows)]
fn store_in_keyring(_: &[u8]) -> Result<()> {
    bail!("Sign-in isn't available on this system yet")
}

/// Replaces the file atomically, so it is never readable by others or half written.
#[cfg(unix)]
fn write_session_file(path: &Path, value: &[u8]) -> Result<()> {
    let directory = path.parent().context("Missing data directory")?;
    fs::create_dir_all(directory).context("Could not create the music data directory")?;
    match fs::symlink_metadata(path) {
        Ok(metadata) => ensure!(
            metadata.file_type().is_file(),
            "{} is not a regular file. Remove it and sign in again.",
            path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("Could not check the saved account file"),
    }
    // A new temporary file is created exclusively with 0600, then renamed over the path.
    // rename replaces the path itself and never follows a link planted there later.
    let mut file = tempfile::Builder::new()
        .prefix(".account-session-")
        .permissions(fs::Permissions::from_mode(0o600))
        .tempfile_in(directory)
        .context("Could not create the saved account file")?;
    file.write_all(value)?;
    file.as_file().sync_all()?;
    file.persist(path)
        .context("Could not save the account file")?;
    File::open(directory)?.sync_all()?;
    Ok(())
}

#[cfg(unix)]
fn read_session_file(path: &Path) -> Result<Option<Session>> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        file => file.context("Could not open the saved account file")?,
    };
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0,
        "{} must be a file only you can read. Remove it and sign in again.",
        path.display()
    );
    let mut value = Vec::new();
    file.take(64 * 1024).read_to_end(&mut value)?;
    Session::from_saved(&value).map(Some)
}

/// Sign-in is Linux-only so far, so nothing else has a session file to read or write.
#[cfg(windows)]
fn write_session_file(_: &Path, _: &[u8]) -> Result<()> {
    bail!("Sign-in isn't available on this system yet")
}

#[cfg(windows)]
fn read_session_file(_: &Path) -> Result<Option<Session>> {
    Ok(None)
}

/// Removes everything the app saved about the account outside the keyring.
fn forget_saved(directory: &Path) -> Result<()> {
    remove_if_present(&directory.join(SESSION_FILE))
        .context("Could not remove the saved account file")?;
    remove_if_present(&directory.join(MARKER)).context("Could not forget the saved account")
}

fn remove_if_present(path: &Path) -> std::io::Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

/// Best effort: a missing marker only hides a later keyring error, so it never fails sign-in.
fn record_saved_account(marker: Option<&Path>) {
    let Some(marker) = marker else {
        return;
    };
    // create_new refuses an existing path, including a planted symlink, in one step.
    let _ = marker
        .parent()
        .map_or(Ok(()), fs::create_dir_all)
        .and_then(|()| {
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(marker)
                .map(drop)
        });
}

#[cfg(not(target_os = "macos"))]
fn clear_keyring(attributes: &[&str]) -> Result<()> {
    let mut command = Command::new("/usr/bin/secret-tool");
    command.arg("clear").args(attributes).stdin(Stdio::null());
    let output = AccountProcess::output(&mut command, None, Duration::from_secs(30))?;
    // secret-tool returns a silent status 1 when a previous cleanup removed the item.
    // Without a Secret Service there is no keyring item to remove.
    let absent = output.status.code() == Some(1)
        && output.stdout.is_empty()
        && (output.stderr.is_empty() || secret_service_missing(&output));
    ensure!(
        output.status.success() || absent,
        "Could not remove the saved account from the keyring"
    );
    Ok(())
}

/// The most an account helper may print.
const OUTPUT_LIMIT: u64 = 1024 * 1024;

impl AccountProcess {
    fn output(
        command: &mut Command,
        operation: Option<&Operation>,
        timeout: Duration,
    ) -> Result<std::process::Output> {
        ensure!(
            !operation.is_some_and(Operation::is_cancelled),
            "Account connection cancelled"
        );
        // macOS has no RAM-backed file, and the browser reader prints the
        // decrypted session, so its output stays in pipes, drained by threads.
        #[cfg(target_os = "macos")]
        {
            fn drain(pipe: Option<impl Read + Send + 'static>) -> thread::JoinHandle<Vec<u8>> {
                thread::spawn(move || {
                    let mut bytes = Vec::new();
                    if let Some(pipe) = pipe {
                        // One byte past the limit is enough to refuse the output.
                        let _ = pipe.take(OUTPUT_LIMIT + 1).read_to_end(&mut bytes);
                    }
                    bytes
                })
            }
            command.stdout(Stdio::piped()).stderr(Stdio::piped());
            let mut process = Self::spawn(command)?;
            let stdout = drain(process.0.stdout.take());
            let stderr = drain(process.0.stderr.take());
            let status = process.wait(timeout, operation)?;
            let collect = |reader: thread::JoinHandle<Vec<u8>>| -> Result<Vec<u8>> {
                let bytes = reader
                    .join()
                    .map_err(|_| anyhow::anyhow!("Could not read the account helper"))?;
                ensure!(
                    bytes.len() as u64 <= OUTPUT_LIMIT,
                    "The account helper returned oversized data"
                );
                Ok(bytes)
            };
            Ok(std::process::Output {
                status,
                stdout: collect(stdout)?,
                stderr: collect(stderr)?,
            })
        }
        // A keyring response can exceed pipe capacity while the user unlocks it.
        #[cfg(not(target_os = "macos"))]
        {
            let stdout = private_file()?;
            let stderr = private_file()?;
            command
                .stdout(stdout.try_clone()?)
                .stderr(stderr.try_clone()?);
            let mut process = Self::spawn(command)?;
            let status = process.wait(timeout, operation)?;
            let read_output = |mut file: File| -> Result<Vec<u8>> {
                ensure!(
                    file.metadata()?.len() <= OUTPUT_LIMIT,
                    "The account helper returned oversized data"
                );
                file.rewind()?;
                let mut bytes = Vec::new();
                file.read_to_end(&mut bytes)?;
                Ok(bytes)
            };
            Ok(std::process::Output {
                status,
                stdout: read_output(stdout)?,
                stderr: read_output(stderr)?,
            })
        }
    }
    fn spawn(command: &mut Command) -> Result<Self> {
        // Account helpers run only where sign-in is available.
        #[cfg(unix)]
        exit_with_parent(command, libc::SIGTERM);
        Ok(Self(
            command
                .spawn()
                .context("Could not start the account helper")?,
        ))
    }
    fn wait(
        &mut self,
        timeout: Duration,
        operation: Option<&Operation>,
    ) -> Result<std::process::ExitStatus> {
        let deadline = Instant::now() + timeout;
        loop {
            ensure!(
                !operation.is_some_and(Operation::is_cancelled),
                "Account connection cancelled"
            );
            if let Some(status) = self.0.try_wait()? {
                return Ok(status);
            }
            ensure!(Instant::now() < deadline, "The account helper timed out");
            thread::sleep(Duration::from_millis(25));
        }
    }
}
impl Drop for AccountProcess {
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
pub(crate) fn private_runtime() -> Result<PathBuf> {
    #[cfg(target_os = "linux")]
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .context("A private desktop runtime directory is required for account connection")?;
    // macOS gives each user a private temporary directory in $TMPDIR.
    #[cfg(target_os = "macos")]
    let runtime = std::env::temp_dir();
    // Windows gives each user a %LOCALAPPDATA% that only they and
    // administrators can open, and new folders inherit that.
    #[cfg(windows)]
    let runtime = {
        let runtime = crate::dirs::data()
            .context("A private local app data folder is required for playback")?
            .join("runtime");
        fs::create_dir_all(&runtime)?;
        runtime
    };
    validate_private_runtime(&runtime)?;
    Ok(runtime)
}

pub(crate) fn validate_private_runtime(runtime: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(runtime)?;
    ensure!(
        runtime.is_absolute() && metadata.is_dir(),
        "The desktop runtime directory must be private to your user"
    );
    #[cfg(unix)]
    ensure!(
        metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o077 == 0,
        "The desktop runtime directory must be private to your user"
    );
    // macOS has no tmpfs. Credentials reach helpers over stdin. Only a cookie
    // database the browser holds locked is copied there, owner-only, and
    // deleted as soon as it is read.
    #[cfg(target_os = "linux")]
    {
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
    }
    Ok(())
}

const ALREADY_RUNNING: &str =
    "YouTube Music is already running. Close its window before opening another instance.";

#[cfg(unix)]
pub fn lock_instance() -> Result<File> {
    lock_file(&private_runtime()?.join("yt-music-gpui.lock"))
}

/// Windows has no flock; a named mutex in this login session does the same.
#[cfg(windows)]
pub fn lock_instance() -> Result<std::os::windows::io::OwnedHandle> {
    crate::os::instance_mutex("yt-music-gpui")
        .context("Could not lock the music application")?
        .context(ALREADY_RUNNING)
}

#[cfg(unix)]
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
            bail!(ALREADY_RUNNING);
        }
        return Err(error).context("Could not lock the music application");
    }
    Ok(file)
}

fn page_config(page: impl Read) -> Result<Value> {
    const MAX_PAGE_BYTES: usize = 4 * 1024 * 1024;
    // The first ytcfg.set call is the JSON bootstrap used by yt-dlp's extract_ytcfg.
    // Later calls can contain JavaScript expressions; never execute or merge those.
    // It arrives in the first ~75 KB, while the server keeps rendering the rest
    // of the page for another second, so stop reading once it is complete.
    let mut page = page.take((MAX_PAGE_BYTES + 1) as u64);
    let mut html = Vec::new();
    let mut chunk = [0; 16 * 1024];
    let config = loop {
        let read = page
            .read(&mut chunk)
            .context("Could not read the account verification page")?;
        html.extend_from_slice(&chunk[..read]);
        ensure!(
            html.len() <= MAX_PAGE_BYTES,
            "YouTube Music returned an oversized account verification page"
        );
        let complete = read == 0;
        // A read can end inside a multi-byte character; parse the valid prefix.
        let text = match std::str::from_utf8(&html) {
            Ok(text) => text,
            Err(error) if !complete && error.error_len().is_none() => {
                std::str::from_utf8(&html[..error.valid_up_to()])
                    .context("YouTube Music returned an unreadable account page")?
            }
            Err(error) => {
                return Err(error).context("YouTube Music returned an unreadable account page");
            }
        };
        let Some((_, bootstrap)) = text.split_once("ytcfg.set(") else {
            ensure!(!complete, "YouTube Music returned no account configuration");
            continue;
        };
        let bootstrap = bootstrap.trim_start();
        let mut values = serde_json::Deserializer::from_str(bootstrap).into_iter::<Value>();
        let config = match values.next() {
            Some(Err(error)) if error.is_eof() && !complete => continue,
            Some(value) => {
                value.context("YouTube Music returned an invalid account configuration")?
            }
            None if !complete => continue,
            None => bail!("YouTube Music returned no account configuration"),
        };
        let rest = bootstrap
            .get(values.byte_offset()..)
            .context("YouTube Music returned an invalid account configuration")?
            .trim_start();
        if rest.is_empty() && !complete {
            continue;
        }
        ensure!(
            rest.starts_with(')'),
            "YouTube Music returned an invalid account configuration"
        );
        break config;
    };
    if config["LOGGED_IN"] == false {
        return Err(RejectedSession.into());
    }
    ensure!(
        config["LOGGED_IN"] == true,
        "YouTube Music did not accept the account session. Connect again."
    );
    ensure!(
        config.get("DATASYNC_ID").is_some(),
        "YouTube Music did not confirm the selected account"
    );
    Ok(config)
}

impl BrowserProfile {
    /// Opens the configured profile for an explicit sign-in request.
    /// That request authorizes reading its session when the user returns.
    pub fn open(operation: &Operation) -> Result<Self> {
        ensure!(ACCOUNT_LOGIN_ENABLED, "Account connection is unavailable");
        #[cfg(target_os = "linux")]
        ensure!(
            Path::new("/usr/bin/secret-tool").is_file(),
            "Account connection requires the desktop OS keyring"
        );
        let desktop = default_browser(operation)?;
        let profile = serde_json::from_slice(&browser_helper(
            "profile",
            &[desktop.as_ref()],
            None,
            operation,
        )?)
        .context("Could not identify the default browser profile")?;
        ensure!(!operation.is_cancelled(), "Account connection cancelled");
        // The opener reuses the normal browser. It is user-owned and must outlive us.
        #[cfg(target_os = "macos")]
        let opener = "/usr/bin/open";
        #[cfg(not(target_os = "macos"))]
        let opener = "/usr/bin/xdg-open";
        let mut opener = Command::new(opener)
            .arg(ORIGIN)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("Could not open your default browser")?;
        thread::spawn(move || {
            let _ = opener.wait();
        });
        Ok(profile)
    }

    /// Read the named profile after consent, including refresh of its confirmed account.
    pub fn read_session(&self, operation: &Operation) -> Result<Session> {
        #[cfg(target_os = "macos")]
        let safe_storage = safe_storage_password(&self.browser)?;
        #[cfg(not(target_os = "macos"))]
        let safe_storage: Option<Vec<u8>> = None;
        let cookies: Vec<Cookie> = serde_json::from_slice(&browser_helper(
            "cookies",
            &[self.browser.as_ref(), self.path.as_os_str()],
            safe_storage.as_deref(),
            operation,
        )?)
        .context("The browser returned an unreadable YouTube session")?;
        let (cookie, playback_cookie) = cookie_headers(&cookies)?;
        let mut header = HeaderValue::from_str(&cookie).context("Invalid YouTube cookies")?;
        header.set_sensitive(true);
        ensure!(!operation.is_cancelled(), "Account connection cancelled");
        let client = reqwest::blocking::Client::builder()
            .user_agent(
                "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 Chrome/130.0.0.0 Safari/537.36",
            )
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        let page = client
            .get(ORIGIN)
            .header("Cookie", header)
            .header("Cache-Control", "no-cache")
            .send()
            .map_err(reqwest::Error::without_url)
            .context("Could not verify the browser's YouTube session")?;
        ensure!(
            page.status() == reqwest::StatusCode::OK,
            "YouTube Music did not accept the browser session (HTTP {}). Finish signing in and retry.",
            page.status().as_u16()
        );
        let config = page_config(page)?;
        ensure!(!operation.is_cancelled(), "Account connection cancelled");
        let mut session = Session::from_browser(cookie, playback_cookie, &config)?;
        session.source_profile = Some(self.clone());
        Ok(session)
    }
}

/// macOS encrypts a Chromium-family browser's cookies with a password in the
/// user's keychain. The app reads it itself, so macOS asks on the app's behalf
/// and "Always Allow" trusts only this app, never a shared tool like Python.
#[cfg(target_os = "macos")]
fn safe_storage_password(browser: &str) -> Result<Option<Vec<u8>>> {
    use security_framework::passwords::{PasswordOptions, generic_password};
    let (service, account) = match browser {
        "Chromium" => ("Chromium Safe Storage", "Chromium"),
        "Google Chrome" => ("Chrome Safe Storage", "Chrome"),
        "Brave" => ("Brave Safe Storage", "Brave"),
        "Helium" => ("Helium Storage Key", "Helium"),
        "Ardia" => ("Ardia Safe Storage", "Ardia"),
        _ => return Ok(None),
    };
    generic_password(PasswordOptions::new_generic_password(service, account))
        .map(Some)
        .with_context(|| {
            format!("Could not read the browser's \"{service}\" key from your Keychain. Allow access and retry.")
        })
}

/// The bundle identifier of the app that opens web links.
#[cfg(target_os = "macos")]
fn default_browser(_: &Operation) -> Result<String> {
    use objc2_app_kit::NSWorkspace;
    use objc2_foundation::{NSBundle, NSString, NSURL};
    let url = NSURL::URLWithString(&NSString::from_str(ORIGIN))
        .context("Could not identify your default browser")?;
    NSWorkspace::sharedWorkspace()
        .URLForApplicationToOpenURL(&url)
        .and_then(|app| NSBundle::bundleWithURL(&app)?.bundleIdentifier())
        .map(|identifier| identifier.to_string())
        .context("Could not identify your default browser")
}

#[cfg(not(target_os = "macos"))]
fn default_browser(operation: &Operation) -> Result<String> {
    let mut command = Command::new("/usr/bin/xdg-settings");
    command.args(["get", "default-web-browser"]);
    let read = |command: &mut Command| -> Result<String> {
        command.stdin(Stdio::null());
        let output = AccountProcess::output(command, Some(operation), Duration::from_secs(5))?;
        ensure!(
            output.status.success(),
            "Could not identify your default browser"
        );
        Ok(std::str::from_utf8(&output.stdout)?.trim().to_owned())
    };
    let desktop = read(&mut command)?;
    if desktop != "omarchy-browser.desktop" {
        return Ok(desktop);
    }
    // Omarchy's link handler delegates to this configured browser resolver.
    let desktop = read(&mut Command::new("omarchy-cmd-default-browser"))
        .context("Could not identify the browser configured in Omarchy")?;
    ensure!(
        !desktop.is_empty() && desktop != "omarchy-browser.desktop",
        "Omarchy did not identify its configured browser"
    );
    Ok(desktop)
}

/// `secret` reaches the helper on its stdin.
fn browser_helper(
    mode: &str,
    values: &[&OsStr],
    secret: Option<&[u8]>,
    operation: &Operation,
) -> Result<Vec<u8>> {
    // Rust owns cleanup even if cancellation kills the helper during a locked-DB read.
    let snapshot = (mode == "cookies")
        .then(|| {
            tempfile::Builder::new()
                .prefix("yt-music-session-")
                .tempdir_in(private_runtime()?)
                .context("Could not prepare the private browser session reader")
        })
        .transpose()?;
    // The Safe Storage password unlocks every cookie and password the browser
    // keeps, so only the app's own Python or Apple's ever receives it, never
    // a copy in the home folder.
    #[cfg(target_os = "macos")]
    let python = crate::player::app_python().unwrap_or_else(|| "/usr/bin/python3".into());
    #[cfg(not(target_os = "macos"))]
    let python = PathBuf::from("/usr/bin/python3");
    let mut command = Command::new(python);
    command
        .args(["-I", "-B", "-c", include_str!("browser_session.py"), mode])
        .args(values);
    match secret {
        Some(secret) => {
            // A few bytes fit in the pipe, so this never waits for the helper.
            let (input, mut writer) = std::io::pipe()?;
            std::io::Write::write_all(&mut writer, secret)?;
            command.stdin(input);
        }
        None => {
            command.stdin(Stdio::null());
        }
    }
    if let Some(snapshot) = &snapshot {
        command.arg(snapshot.path());
    }
    // Chromium-family reads may wait for the keyring to unlock their Safe Storage key.
    let output = AccountProcess::output(&mut command, Some(operation), Duration::from_secs(30))?;
    ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(output.stdout)
}

fn cookie_headers(cookies: &[Cookie]) -> Result<(String, String)> {
    ensure!(cookies.len() <= 256, "Too many YouTube cookies");
    let mut headers = Vec::new();
    let mut playback = "# Netscape HTTP Cookie File\n".to_owned();
    for cookie in cookies {
        ensure!(
            matches!(
                cookie.domain.as_str(),
                ".youtube.com"
                    | "youtube.com"
                    | ".music.youtube.com"
                    | "music.youtube.com"
                    | ".www.youtube.com"
                    | "www.youtube.com"
            ) && cookie.path == "/"
                && !cookie.name.is_empty()
                && cookie
                    .name
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&c))
                && cookie
                    .value
                    .bytes()
                    .all(|c| (0x21..=0x7e).contains(&c) && !b"\",;\\".contains(&c)),
            "The browser returned invalid YouTube cookies"
        );
        if cookie.expires > 0 && cookie.expires <= timestamp() {
            continue;
        }
        if matches!(
            cookie.domain.as_str(),
            ".youtube.com" | ".music.youtube.com" | "music.youtube.com"
        ) {
            headers.push(format!("{}={}", cookie.name, cookie.value));
        }
        playback.push_str(&format!(
            "{}\t{}\t/\t{}\t{}\t{}\t{}\n",
            cookie.domain,
            if cookie.domain.starts_with('.') {
                "TRUE"
            } else {
                "FALSE"
            },
            if cookie.secure { "TRUE" } else { "FALSE" },
            cookie.expires,
            cookie.name,
            cookie.value
        ));
    }
    let header = headers.join("; ");
    ensure!(
        header.len() <= 64 * 1024 && playback.len() <= 128 * 1024,
        "The YouTube session is oversized"
    );
    ensure!(
        header
            .split(';')
            .filter_map(|p| p.trim().split_once('='))
            .any(
                |(key, value)| matches!(key, "SAPISID" | "__Secure-3PAPISID") && !value.is_empty()
            ),
        "No signed-in YouTube session was found. Sign in in a normal browser window, then retry."
    );
    Ok((header, playback))
}

impl Session {
    fn from_browser(cookie: String, playback_cookie: String, config: &Value) -> Result<Self> {
        ensure!(
            config["LOGGED_IN"] == true,
            "Sign in to YouTube Music in your browser, then retry."
        );
        let authuser = match &config["SESSION_INDEX"] {
            Value::String(index) => index.clone(),
            Value::Number(index) if index.is_u64() => index.to_string(),
            _ => anyhow::bail!("YouTube Music did not identify the Google account"),
        };
        let data_sync_id = config["DATASYNC_ID"]
            .as_str()
            .context("YouTube Music did not identify the account")?
            .to_owned();
        let (first, second) = data_sync_id
            .split_once("||")
            .context("YouTube Music returned an invalid account identity")?;
        let delegated_session_id = (!second.is_empty()).then(|| first.to_owned());
        let client_version = config["INNERTUBE_CLIENT_VERSION"]
            .as_str()
            .filter(|version| !version.is_empty())
            .context("YouTube Music returned no client version")?
            .to_owned();
        let session = Self {
            cookie,
            playback_cookie,
            authuser,
            delegated_session_id,
            data_sync_id,
            client_version,
            source_profile: None,
        };
        session.validate()?;
        session.verify_server_identity(config)?;
        Ok(session)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn private_runtime_rejects_shared_directories_and_symlinks() {
        let parent = tempfile::tempdir().unwrap();
        fs::set_permissions(parent.path(), fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            validate_private_runtime(parent.path())
                .unwrap_err()
                .to_string(),
            "The desktop runtime directory must be private to your user"
        );
        fs::set_permissions(parent.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let link = parent.path().join("link");
        std::os::unix::fs::symlink(parent.path(), &link).unwrap();
        assert_eq!(
            validate_private_runtime(&link).unwrap_err().to_string(),
            "The desktop runtime directory must be private to your user"
        );
    }

    #[test]
    #[ignore = "requires the configured desktop browser and Omarchy resolver; reads profile metadata only"]
    fn live_default_browser_profile_resolves_omarchy_launcher() -> Result<()> {
        let operation = Operation::default();
        let desktop = default_browser(&operation)?;
        ensure!(
            desktop != "omarchy-browser.desktop",
            "Launcher was not resolved"
        );
        let _: BrowserProfile = serde_json::from_slice(&browser_helper(
            "profile",
            &[desktop.as_ref()],
            None,
            &operation,
        )?)?;
        Ok(())
    }
    #[test]
    #[cfg(unix)]
    fn browser_reader_uses_configured_profile_and_scoped_read_only_wal() {
        let mut source = private_file().unwrap();
        source
            .write_all(include_bytes!("browser_session.py"))
            .unwrap();
        source.rewind().unwrap();
        let output = Command::new("/usr/bin/python3")
            .args(["-I", "-c", include_str!("browser_session_tests.py")])
            .stdin(source)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    #[test]
    fn browser_cookie_header_respects_destination_and_rejects_injection() {
        let cookies = serde_json::from_value::<Vec<Cookie>>(json!([
            {"name":"SAPISID","value":"synthetic","domain":".youtube.com","path":"/","secure":true,"expires":0},
            {"name":"WWW","value":"synthetic","domain":"www.youtube.com","path":"/","secure":true,"expires":0},
            {"name":"ROOT_ONLY","value":"synthetic","domain":"youtube.com","path":"/","secure":true,"expires":0},
            {"name":"OLD","value":"synthetic","domain":".youtube.com","path":"/","secure":true,"expires":1}
        ])).unwrap();
        let (header, playback) = cookie_headers(&cookies).unwrap();
        assert_eq!(header, "SAPISID=synthetic");
        assert!(playback.contains("www.youtube.com\tFALSE\t/\tTRUE\t0\tWWW\tsynthetic\n"));
        assert!(!playback.contains("OLD"));
        for (key, value) in [
            ("domain", ".google.com"),
            ("domain", "evil.youtube.com"),
            ("name", "SAPISID\n"),
            ("value", "a; injected=b"),
            ("value", "a\tb"),
            ("path", "/other"),
        ] {
            let mut cookie = json!({"name":"SAPISID","value":"synthetic","domain":".youtube.com","path":"/","secure":true,"expires":0});
            cookie[key] = json!(value);
            assert!(
                cookie_headers(&[serde_json::from_value(cookie).unwrap()]).is_err(),
                "{key}"
            );
        }
        assert!(cookie_headers(&[]).is_err());
    }
    #[test]
    fn browser_bootstrap_requires_server_account_index_and_full_identity() {
        let primary = json!({"LOGGED_IN":true,"SESSION_INDEX":2,"DATASYNC_ID":"primary-user||","INNERTUBE_CLIENT_VERSION":"1.test"});
        let candidate =
            Session::from_browser("SAPISID=synthetic".into(), String::new(), &primary).unwrap();
        assert_eq!(candidate.authuser, "2");
        assert!(candidate.delegated_session_id.is_none());
        let mut brand = primary.clone();
        brand["DATASYNC_ID"] = json!("brand-id||primary-user");
        brand["DELEGATED_SESSION_ID"] = json!("brand-id");
        let candidate =
            Session::from_browser("SAPISID=synthetic".into(), String::new(), &brand).unwrap();
        assert_eq!(candidate.delegated_session_id.as_deref(), Some("brand-id"));
        for (key, value) in [
            ("SESSION_INDEX", Value::Null),
            ("DATASYNC_ID", Value::Null),
            ("LOGGED_IN", json!(false)),
            ("DELEGATED_SESSION_ID", json!("other-brand")),
            ("USER_SESSION_ID", json!("other-user")),
            ("INNERTUBE_CLIENT_VERSION", json!("")),
        ] {
            let mut config = primary.clone();
            config[key] = value;
            assert!(
                Session::from_browser("SAPISID=synthetic".into(), String::new(), &config).is_err(),
                "{key}"
            );
        }
    }
    #[cfg(unix)]
    #[test]
    fn only_one_instance_can_own_account_state() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("application.lock");
        let owner = lock_file(&path).unwrap();
        assert!(lock_file(&path).is_err());
        drop(owner);
        // Tests that spawn processes can fork while `owner` is open; the child keeps the
        // lock's file description until it execs, so release can lag by a moment.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while lock_file(&path).is_err() {
            assert!(
                std::time::Instant::now() < deadline,
                "released lock stayed held"
            );
            std::thread::sleep(Duration::from_millis(10));
        }

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
            source_profile: None,
        }
    }
    #[cfg(unix)]
    #[test]
    fn a_timed_out_keyring_process_is_terminated_and_reaped() {
        let mut command = Command::new("sleep");
        command.arg("30");
        let mut process = AccountProcess::spawn(&mut command).unwrap();
        let pid = process.0.id() as libc::pid_t;
        assert!(process.wait(Duration::from_millis(20), None).is_err());
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
    #[cfg(unix)]
    #[test]
    fn keyring_capture_handles_large_output_and_preserves_exit_status() {
        let mut command = Command::new("/bin/sh");
        command.args([
            "-c",
            "head -c 131072 /dev/zero; printf 'synthetic error' >&2; exit 7",
        ]);
        let output = AccountProcess::output(
            &mut command,
            Some(&Operation::default()),
            Duration::from_secs(2),
        )
        .unwrap();
        assert_eq!(output.status.code(), Some(7));
        assert_eq!(output.stdout, vec![0; 131072]);
        assert_eq!(output.stderr, b"synthetic error");

        let mut command = Command::new("/usr/bin/head");
        command.args(["-c", "1048577", "/dev/zero"]);
        let error = AccountProcess::output(
            &mut command,
            Some(&Operation::default()),
            Duration::from_secs(2),
        )
        .unwrap_err();
        assert!(error.to_string().contains("oversized"));
    }
    #[cfg(unix)]
    #[test]
    fn keyring_cancellation_terminates_and_reaps_the_child() {
        let operation = Operation::default();
        let mut command = Command::new("sleep");
        command.arg("30");
        let mut process = AccountProcess::spawn(&mut command).unwrap();
        let pid = process.0.id() as libc::pid_t;
        operation.cancel();
        assert!(
            process
                .wait(Duration::from_secs(30), Some(&operation))
                .unwrap_err()
                .to_string()
                .contains("cancelled")
        );
        drop(process);
        assert_eq!(
            unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) },
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
        assert!(
            AccountProcess::output(&mut command, Some(&operation), Duration::from_secs(30))
                .is_err()
        );
    }
    #[test]
    #[ignore = "saves, reads and removes a synthetic account; runs only when HOME is a scratch home whose keychains are all inside it"]
    #[cfg(target_os = "macos")]
    fn live_keychain_saves_reads_and_forgets_the_account() {
        // The item has a fixed name, so a run against the real home would replace a saved account.
        let real_home =
            unsafe { std::ffi::CStr::from_ptr((*libc::getpwuid(libc::geteuid())).pw_dir) };
        let home = std::env::var_os("HOME").unwrap();
        assert_ne!(
            fs::canonicalize(&home).unwrap(),
            fs::canonicalize(OsStr::new(real_home.to_str().unwrap())).unwrap(),
            "Set HOME to a scratch home with its own login keychain"
        );
        // The Security framework searches the user's keychain list and adds to
        // the default keychain. Both follow HOME, unless its settings name others.
        let security = |arguments: &[&str]| {
            let output = Command::new("/usr/bin/security")
                .args(arguments)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap()
        };
        let home = fs::canonicalize(&home).unwrap();
        let keychains: Vec<_> = [
            security(&["list-keychains", "-d", "user"]),
            security(&["default-keychain", "-d", "user"]),
        ]
        .concat()
        .lines()
        .map(|line| PathBuf::from(line.trim().trim_matches('"')))
        .collect();
        assert!(
            keychains.len() >= 2
                && keychains
                    .iter()
                    .all(|path| fs::canonicalize(path).is_ok_and(|path| path.starts_with(&home))),
            "Every keychain the test can reach must be inside the scratch HOME: {keychains:?}"
        );
        Session::disconnect().unwrap();
        assert!(Session::load(&Operation::default()).unwrap().is_none());
        // Another program could plant the item first, open to every app and
        // with another label. Saving must replace it, never write into it.
        security(&[
            "add-generic-password",
            "-A",
            "-l",
            "Planted",
            "-s",
            KEYCHAIN_SERVICE,
            "-a",
            KEYCHAIN_ACCOUNT,
            "-w",
            "planted",
        ]);
        // Simulate another program adding the item after our delete.
        let value = session("SAPISID=first").saved_value().unwrap();
        assert_eq!(
            add_to_keychain(&value).unwrap_err().code(),
            security_framework_sys::base::errSecDuplicateItem
        );
        let password = security(&[
            "find-generic-password",
            "-s",
            KEYCHAIN_SERVICE,
            "-a",
            KEYCHAIN_ACCOUNT,
            "-w",
        ]);
        assert_eq!(password.trim_end(), "planted");
        session("SAPISID=first").save().unwrap();
        let dump = security(&["dump-keychain", "-a"]);
        let item = dump
            .split("keychain: ")
            .find(|item| item.contains("\"svce\"<blob>=\"yt-music-gpui\""))
            .unwrap();
        let decrypt = item
            .split("entry ")
            .find(|entry| entry.contains("decrypt"))
            .unwrap();
        assert!(
            !decrypt.contains("applications: <null>"),
            "Only the app may read the account: {decrypt}"
        );
        for cookie in ["SAPISID=first", "SAPISID=rotated"] {
            session(cookie).save().unwrap();
            let saved = Session::load(&Operation::default()).unwrap().unwrap();
            assert_eq!(saved.cookie, cookie);
        }
        let cookies = [
            "SAPISID",
            "SID",
            "HSID",
            "SSID",
            "__Secure-1PSID",
            "__Secure-3PSID",
        ]
        .map(|name| Cookie {
            name: name.into(),
            value: "synthetic".repeat(56),
            domain: ".youtube.com".into(),
            path: "/".into(),
            secure: true,
            expires: 0,
        });
        let (cookie, playback_cookie) = cookie_headers(&cookies).unwrap();
        let mut large = session(&cookie);
        large.playback_cookie = playback_cookie;
        assert!(large.saved_value().unwrap().len() > 8192);
        large.save().unwrap();
        let saved = Session::load(&Operation::default()).unwrap().unwrap();
        assert!(saved.same_credentials(&large));
        Session::disconnect().unwrap();
        assert!(Session::load(&Operation::default()).unwrap().is_none());
        Session::disconnect().unwrap();
    }
    #[test]
    #[ignore = "requires the desktop Secret Service; uses only a unique synthetic test entry"]
    #[cfg(target_os = "linux")]
    fn live_keyring_removal_is_idempotent() {
        let id = format!("synthetic-{}-{}", std::process::id(), timestamp());
        let attributes = ["application", "yt-music-gpui-tests", "credential", &id];
        let result = (|| -> Result<()> {
            clear_keyring(&attributes)?;
            let mut expected = session("SAPISID=synthetic-keyring-cookie");
            expected.playback_cookie = "# Netscape HTTP Cookie File\n.youtube.com\tTRUE\t/\tTRUE\t0\tSAPISID\tsynthetic-keyring-cookie\n".into();
            let encoded = expected.saved_value()?;
            ensure!(
                !encoded.contains(&b'\n') && !encoded.contains(&b'\t'),
                "Session JSON must escape control characters"
            );
            let mut input = sealed_file(&encoded)?;
            input.rewind()?;
            let mut store = Command::new("/usr/bin/secret-tool");
            store
                .args(["store", "--label=YouTube Music synthetic test"])
                .args(attributes)
                .stdin(input)
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            ensure!(
                AccountProcess::spawn(&mut store)?
                    .wait(Duration::from_secs(30), None)?
                    .success(),
                "Synthetic store failed"
            );
            let mut lookup = Command::new("/usr/bin/secret-tool");
            lookup.arg("lookup").args(attributes).stdin(Stdio::null());
            let output = AccountProcess::output(&mut lookup, None, Duration::from_secs(30))?;
            ensure!(
                output.status.success() && output.stdout == encoded,
                "Synthetic lookup failed"
            );
            let decoded = Session::from_saved(&output.stdout)?;
            ensure!(
                decoded.playback_cookie == expected.playback_cookie,
                "Playback cookies did not survive keyring storage"
            );
            clear_keyring(&attributes)?;
            clear_keyring(&attributes)?;
            let output = AccountProcess::output(&mut lookup, None, Duration::from_secs(30))?;
            ensure!(
                output.status.code() == Some(1)
                    && output.stdout.is_empty()
                    && output.stderr.is_empty(),
                "Synthetic item remains"
            );
            Ok(())
        })();
        let cleanup = clear_keyring(&attributes);
        result.unwrap();
        cleanup.unwrap();
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
    fn saved_session_encoding_preserves_cookie_bytes_and_reads_legacy_json() {
        let mut expected = session("SAPISID=synthetic");
        expected.playback_cookie = "# Netscape HTTP Cookie File\n.youtube.com\tTRUE\t/\tTRUE\t0\tSAPISID\tsynthetic\\value\n".into();
        let encoded = expected.saved_value().unwrap();
        assert!(
            !encoded
                .iter()
                .any(|byte| byte.is_ascii_control() || *byte == b'\\')
        );
        let legacy = serde_json::to_vec(&expected).unwrap();
        for value in [&encoded, &legacy] {
            let restored = Session::from_saved(value).unwrap();
            assert_eq!(restored.playback_cookie, expected.playback_cookie);
            assert_eq!(restored.cookie, expected.cookie);
        }
        assert!(Session::from_saved(b"yt-music-session-v1:invalid!").is_err());
    }
    #[test]
    #[cfg(target_os = "linux")]
    fn secret_tool_rejects_values_it_would_truncate() {
        let error = store_in_keyring(&[b'x'; 8193]).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("exceeds the desktop keyring helper's storage limit")
        );
    }
    #[cfg(unix)]
    #[test]
    fn keyring_failures_report_a_locked_account_only_after_one_was_saved() {
        use std::os::unix::process::ExitStatusExt;
        let output = |code: i32, stdout: &[u8], stderr: &[u8]| Output {
            status: std::process::ExitStatus::from_raw(code << 8),
            stdout: stdout.to_vec(),
            stderr: stderr.to_vec(),
        };
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("yt-music-gpui/account-saved");
        let no_service = output(
            1,
            b"",
            b"secret-tool: The name org.freedesktop.secrets was not provided by any .service files\n",
        );
        assert!(
            Session::from_lookup(KeyringLookup::from_secret_tool(&no_service), Some(&marker))
                .unwrap()
                .is_none(),
            "Someone who never signed in has no account to unlock"
        );
        assert!(
            Session::from_lookup(
                KeyringLookup::from_secret_tool(&output(1, b"", b"")),
                Some(&marker)
            )
            .unwrap()
            .is_none()
        );

        let saved = session("SAPISID=synthetic").saved_value().unwrap();
        assert!(
            Session::from_lookup(
                KeyringLookup::from_secret_tool(&output(0, &saved, b"")),
                Some(&marker)
            )
            .unwrap()
            .is_some()
        );
        assert!(
            marker.exists(),
            "A readable saved account records the marker"
        );
        assert_eq!(
            Session::from_lookup(KeyringLookup::from_secret_tool(&no_service), Some(&marker))
                .err()
                .map(|error| error.to_string())
                .as_deref(),
            Some("Could not unlock the saved YouTube Music account")
        );
        assert!(
            Session::from_lookup(
                KeyringLookup::from_secret_tool(&output(1, b"", b"")),
                Some(&marker)
            )
            .unwrap()
            .is_none()
        );
        assert!(
            !marker.exists(),
            "An account deleted outside the app forgets the marker"
        );

        let target = directory.path().join("elsewhere");
        std::os::unix::fs::symlink(&target, &marker).unwrap();
        record_saved_account(Some(&marker));
        assert!(!target.exists(), "The marker never follows a symlink");
    }
    #[cfg(unix)]
    #[test]
    fn session_file_is_owner_only_and_round_trips() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("yt-music-gpui").join(SESSION_FILE);
        for cookie in ["SAPISID=first", "SAPISID=rotated"] {
            write_session_file(&path, &session(cookie).saved_value().unwrap()).unwrap();
            assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
            let restored = read_session_file(&path).unwrap().unwrap();
            assert_eq!(restored.cookie, cookie);
        }
        assert_eq!(
            fs::read_dir(path.parent().unwrap()).unwrap().count(),
            1,
            "No temporary file is left behind"
        );
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            read_session_file(&path).is_err(),
            "A readable file is refused"
        );
    }
    #[cfg(unix)]
    #[test]
    fn session_file_never_follows_a_symlink() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(SESSION_FILE);
        let target = directory.path().join("elsewhere");
        fs::write(&target, session("SAPISID=planted").saved_value().unwrap()).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(read_session_file(&path).is_err());
        assert!(write_session_file(&path, &session("SAPISID=new").saved_value().unwrap()).is_err());
        assert!(
            fs::read(&target).unwrap() == session("SAPISID=planted").saved_value().unwrap(),
            "The link target is untouched"
        );
    }
    #[cfg(unix)]
    #[test]
    fn signing_out_removes_the_session_file_and_marker() {
        let directory = tempfile::tempdir().unwrap();
        write_session_file(
            &directory.path().join(SESSION_FILE),
            &session("SAPISID=synthetic").saved_value().unwrap(),
        )
        .unwrap();
        record_saved_account(Some(&directory.path().join(MARKER)));
        forget_saved(directory.path()).unwrap();
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
        forget_saved(directory.path()).unwrap();
    }
    #[cfg(unix)]
    #[test]
    fn a_keyring_account_wins_over_the_session_file() {
        use std::os::unix::process::ExitStatusExt;
        let output = |code: i32, stdout: &[u8], stderr: &[u8]| Output {
            status: std::process::ExitStatus::from_raw(code << 8),
            stdout: stdout.to_vec(),
            stderr: stderr.to_vec(),
        };
        let directory = tempfile::tempdir().unwrap();
        write_session_file(
            &directory.path().join(SESSION_FILE),
            &session("SAPISID=file").saved_value().unwrap(),
        )
        .unwrap();
        let keyring = session("SAPISID=keyring").saved_value().unwrap();
        let no_service = output(
            1,
            b"",
            b"secret-tool: The name org.freedesktop.secrets was not provided by any .service files\n",
        );
        for (lookup, expected) in [
            (no_service, "SAPISID=file"),
            (output(1, b"", b""), "SAPISID=file"),
            (output(0, &keyring, b""), "SAPISID=keyring"),
        ] {
            let restored = Session::from_storage(
                KeyringLookup::from_secret_tool(&lookup),
                Some(directory.path()),
            )
            .unwrap()
            .unwrap();
            assert_eq!(restored.cookie, expected);
        }
    }
    #[test]
    fn saved_profile_consent_round_trips_and_legacy_entries_have_no_source() {
        let mut expected = session("SAPISID=synthetic");
        let legacy = serde_json::to_vec(&expected).unwrap();
        assert!(
            Session::from_saved(&legacy)
                .unwrap()
                .source_profile
                .is_none()
        );
        expected.source_profile = Some(
            serde_json::from_value(json!({
                "browser":"Zen", "name":"Confirmed profile", "path":"/synthetic/confirmed-profile"
            }))
            .unwrap(),
        );
        let restored = Session::from_saved(&expected.saved_value().unwrap()).unwrap();
        let source = restored.source_profile.unwrap();
        assert_eq!(source.browser, "Zen");
        assert_eq!(source.name, "Confirmed profile");
        assert_eq!(source.path, Path::new("/synthetic/confirmed-profile"));
    }

    #[test]
    fn credential_comparison_covers_authority_without_profile_metadata() {
        let saved = session("SAPISID=synthetic");
        let mut candidate = saved.clone();
        candidate.source_profile = Some(BrowserProfile {
            browser: "Zen".into(),
            name: "Confirmed profile".into(),
            path: PathBuf::from("/synthetic/confirmed-profile"),
        });
        assert!(saved.same_credentials(&candidate));
        let changes: [fn(&mut Session); 6] = [
            |session| session.cookie.push_str("; SID=rotated"),
            |session| session.playback_cookie.push_str("rotated"),
            |session| session.authuser = "2".into(),
            |session| session.data_sync_id = "other-user||".into(),
            |session| session.delegated_session_id = Some("other-brand".into()),
            |session| session.client_version = "renewed".into(),
        ];
        for change in changes {
            let mut candidate = saved.clone();
            change(&mut candidate);
            assert!(!saved.same_credentials(&candidate));
        }
    }

    #[test]
    fn ordinary_responses_reject_only_explicit_logged_out_signals() {
        for response in [
            json!({}),
            json!({"LOGGED_IN":true}),
            json!({"contents":[]}),
            json!({"LOGGED_IN":"false"}),
            json!({"DATASYNC_ID":"unverified||"}),
            json!({"responseContext":{"mainAppWebResponseContext":{"loggedOut":false}}}),
            json!({"responseContext":{"serviceTrackingParams":[{
                "params":[{"key":"logged_in", "value":"1"}, {"key":"other", "value":"0"}]
            }]}}),
        ] {
            reject_logged_out(&response).unwrap();
        }
        for response in [
            json!({"LOGGED_IN":false}),
            json!({"responseContext":{"mainAppWebResponseContext":{"loggedOut":true}}}),
            json!({"responseContext":{"serviceTrackingParams":[{
                "params":[{"key":"logged_in", "value":"0"}]
            }]}}),
            json!({"responseContext":{"serviceTrackingParams":[{
                "params":[{"key":"yt_li", "value":"0"}]
            }]}}),
        ] {
            assert!(
                reject_logged_out(&response)
                    .unwrap_err()
                    .is::<RejectedSession>()
            );
        }
    }

    #[test]
    fn only_explicit_session_rejection_allows_browser_refresh() {
        let saved = session("SAPISID=synthetic");
        for rejected in [
            json!({"LOGGED_IN":false}),
            json!({"responseContext":{"mainAppWebResponseContext":{"loggedOut":true}}}),
            json!({"responseContext":{"serviceTrackingParams":[{
                "params":[{"key":"logged_in", "value":"0"}]
            }]}}),
        ] {
            assert!(session_was_rejected(
                &saved.verify_server_identity(&rejected).unwrap_err()
            ));
        }
        let rejected_html =
            page_config(b"ytcfg.set({\"LOGGED_IN\":false});".as_slice()).unwrap_err();
        assert!(session_was_rejected(&rejected_html));
        for invalid in [
            json!({}),
            json!({"LOGGED_IN":"true"}),
            json!({"DATASYNC_ID":"other||"}),
        ] {
            assert!(!session_was_rejected(
                &saved.verify_server_identity(&invalid).unwrap_err()
            ));
        }
        assert!(!session_was_rejected(&anyhow::anyhow!(
            "Network unavailable"
        )));
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
    fn optional_api_identity_accepts_missing_proof_and_rejects_supplied_conflicts() {
        let saved = session("SAPISID=synthetic");
        for response in [
            json!({}),
            json!({"contents":[]}),
            json!({"LOGGED_IN":true, "SESSION_INDEX":0}),
            json!({"responseContext":{"mainAppWebResponseContext":{
                "datasyncId":"synthetic-user||", "loggedOut":false
            }}}),
        ] {
            saved.check_response_identity(&response).unwrap();
        }
        for response in [
            json!({"DATASYNC_ID":"other||"}),
            json!({"DATASYNC_ID":null}),
            json!({"SESSION_INDEX":1}),
            json!({"SESSION_INDEX":false}),
            json!({"LOGGED_IN":"true"}),
        ] {
            let error = saved.check_response_identity(&response).unwrap_err();
            assert!(!session_was_rejected(&error));
        }
        let logout = saved
            .check_response_identity(&json!({"LOGGED_IN":false}))
            .unwrap_err();
        assert!(session_was_rejected(&logout));
    }

    #[test]
    fn account_menu_proof_distinguishes_absence_from_conflicting_identity() {
        let s = session("SAPISID=synthetic");
        assert!(!s.server_identity_confirmed(&json!({})).unwrap());
        let proof = json!({"responseContext":{"mainAppWebResponseContext":{
            "datasyncId":"synthetic-user||", "loggedOut":false
        }}});
        let client = reqwest::blocking::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        // A menu with complete proof needs no page request.
        s.verify_account_identity(&proof, &client).unwrap();
        for invalid in [
            json!({"DATASYNC_ID":"other||"}),
            json!({"DATASYNC_ID":null}),
            json!({"LOGGED_IN":false}),
            json!({"LOGGED_IN":"true"}),
            json!({"responseContext":{"mainAppWebResponseContext":{"loggedOut":true}}}),
            json!({"responseContext":{"serviceTrackingParams":[{
                "service":"GFEEDBACK", "params":[{"key":"logged_in","value":"0"}]
            }]}}),
            json!({"responseContext":{"serviceTrackingParams":[{
                "service":"CSI", "params":[{"key":"yt_li","value":"0"}]
            }]}}),
            json!({"SESSION_INDEX":1}),
            json!({"USER_SESSION_ID":"other"}),
            json!({"DELEGATED_SESSION_ID":"other"}),
        ] {
            let error = s.verify_account_identity(&invalid, &client).unwrap_err();
            assert!(
                error.to_string().contains("different account")
                    || error.to_string().contains("did not accept"),
                "Conflicting proof must fail before requesting HTML: {error:#}"
            );
        }
    }
    #[test]
    fn html_account_proof_matches_primary_and_brand_bootstraps() {
        for (identity, delegated, user) in [
            ("synthetic-user||", None, "synthetic-user"),
            ("brand||user", Some("brand"), "user"),
        ] {
            let mut s = session("SAPISID=synthetic");
            s.authuser = "2".into();
            s.data_sync_id = identity.into();
            s.delegated_session_id = delegated.map(str::to_owned);
            let config = json!({
                "LOGGED_IN":true, "DATASYNC_ID":identity, "SESSION_INDEX":"2",
                "USER_SESSION_ID":user, "DELEGATED_SESSION_ID":delegated,
                "nested":{"text":"quoted \" } ); ytcfg.set( and braces { }"}
            });
            // Current Music HTML has a JSON bootstrap followed by non-JSON ytcfg calls.
            let html = format!(
                "<script>var ytcfg={{}};\nytcfg.set({config});</script>\
                 <script>ytcfg.set({{'CSI_SERVICE_NAME': 'youtube_web_music'}})</script>"
            );
            s.verify_html_identity(html.as_bytes()).unwrap();
        }
    }
    #[test]
    fn html_account_proof_stops_reading_once_the_bootstrap_is_complete() {
        // The server renders the rest of the page for about a second after the
        // bootstrap. Byte-at-a-time reads also split the multi-byte character.
        struct Page(Vec<u8>, usize);
        impl Read for Page {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                let Some(&byte) = self.0.get(self.1) else {
                    return Err(std::io::Error::other("read past the account bootstrap"));
                };
                buffer[0] = byte;
                self.1 += 1;
                Ok(1)
            }
        }
        let s = session("SAPISID=synthetic");
        let config = json!({"LOGGED_IN":true, "DATASYNC_ID":"synthetic-user||", "name":"Beyoncé"});
        let unclosed = Page(format!("<script>ytcfg.set({config}").into_bytes(), 0);
        let error = s.verify_html_identity(unclosed).unwrap_err();
        assert!(format!("{error:#}").contains("read past"), "{error:#}");
        let page = Page(format!("<script>ytcfg.set({config})").into_bytes(), 0);
        s.verify_html_identity(page).unwrap();
    }
    #[test]
    fn html_account_proof_rejects_missing_conflicting_and_logged_out_identity() {
        let s = session("SAPISID=synthetic");
        let valid = json!({
            "LOGGED_IN":true, "DATASYNC_ID":"synthetic-user||", "SESSION_INDEX":0
        });
        for (key, value) in [
            ("LOGGED_IN", json!(false)),
            ("LOGGED_IN", json!("true")),
            ("DATASYNC_ID", json!("other||")),
            ("DATASYNC_ID", Value::Null),
            ("SESSION_INDEX", json!(1)),
            ("USER_SESSION_ID", json!("other")),
            ("DELEGATED_SESSION_ID", json!("brand")),
            (
                "responseContext",
                json!({"mainAppWebResponseContext":{"datasyncId":"other||"}}),
            ),
            (
                "responseContext",
                json!({"mainAppWebResponseContext":{"loggedOut":true}}),
            ),
        ] {
            let mut config = valid.clone();
            config[key] = value;
            let html = format!("<script>ytcfg.set({config});</script>");
            assert!(s.verify_html_identity(html.as_bytes()).is_err(), "{key}");
        }
        for missing in ["LOGGED_IN", "DATASYNC_ID"] {
            let mut config = valid.clone();
            config.as_object_mut().unwrap().remove(missing);
            let html = format!("<script>ytcfg.set({config});</script>");
            assert!(
                s.verify_html_identity(html.as_bytes()).is_err(),
                "{missing}"
            );
        }
    }
    #[test]
    fn html_account_proof_rejects_malformed_and_oversized_pages() {
        let s = session("SAPISID=synthetic");
        for html in [
            "<html>No bootstrap</html>",
            "ytcfg.set(",
            "ytcfg.set({'LOGGED_IN': true});",
            "ytcfg.set({\"LOGGED_IN\":true}",
            "ytcfg.set([])",
            "ytcfg.set({\"LOGGED_IN\":true}+other)",
        ] {
            assert!(s.verify_html_identity(html.as_bytes()).is_err());
        }
        let html = format!(
            "{}ytcfg.set({{\"LOGGED_IN\":true,\"DATASYNC_ID\":\"synthetic-user||\"}});",
            " ".repeat(4 * 1024 * 1024)
        );
        assert!(
            s.verify_html_identity(html.as_bytes())
                .unwrap_err()
                .to_string()
                .contains("oversized")
        );
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
}
