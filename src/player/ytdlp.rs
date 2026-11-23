use super::native;
use super::stream::{PreparedTarget, ResolvedStream, normalization_gain};
use crate::auth::PlaybackCredentials;
use crate::os::{ChildOutput, ExitNotice, Group, child_output, exit_notice, private_file};
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::{
    fs::{self, File},
    io::{self, ErrorKind, Read, Seek, Write},
    path::{Path, PathBuf},
    process::{ChildStdin, Command, Stdio},
    sync::Arc,
    thread,
    time::{Duration, Instant, SystemTime},
};

const ACCOUNT_EXTRACTOR: &str = include_str!("../ytdlp_account.py");

/// The account extractor's directory and the credentials it reads from stdin.
pub(super) struct ExtractorCredentials {
    pub(super) plugin_dir: PathBuf,
    pub(super) source: PlaybackCredentials,
}

pub(super) fn write_account_extractor(runtime: &Path) -> Result<PathBuf> {
    let root = runtime.join("plugins");
    let directory = root.join("account/yt_dlp_plugins/extractor");
    fs::create_dir_all(&directory)?;
    fs::write(directory.join("account.py"), ACCOUNT_EXTRACTOR)?;
    Ok(root)
}

pub(super) const REVIEWED_YTDLP: &str = env!("REVIEWED_YTDLP");

pub(super) fn account_playback_supported(ytdlp: &Path) -> bool {
    ytdlp_command(ytdlp)
        .args(["--ignore-config", "--no-plugin-dirs", "--version"])
        .output()
        .is_ok_and(|output| {
            output.status.success()
                && String::from_utf8_lossy(&output.stdout).trim() == REVIEWED_YTDLP
        })
}

pub(super) fn account_credentials(
    source: Option<PlaybackCredentials>,
    supported: impl FnOnce() -> bool,
    warned: bool,
    notice: &mut Option<String>,
) -> Option<PlaybackCredentials> {
    if source.is_none() || supported() {
        *notice = None;
        return source;
    }
    if !warned {
        *notice = Some(format!(
            "Signed-in playback requires yt-dlp {REVIEWED_YTDLP}. Playing public streams as a guest with the installed version. Account-only songs may be unavailable."
        ));
    }
    None
}

fn extractor_arguments(command: &mut Command, credentials: Option<&ExtractorCredentials>) {
    command.args([
        "--ignore-config",
        "--no-playlist",
        "--no-warnings",
        "--quiet",
        "--get-url",
        // Extraction must not persist account data or signed URLs in a disk cache.
        "--no-cache-dir",
    ]);
    // Only Deno: yt-dlp runs it without file, process or network permissions.
    // Bun and QuickJS get full access, and Node's permission model can still
    // be escaped by player code, so YouTube's player JavaScript never runs there.
    command.args(["--js-runtimes", "deno"]);
    if let Some(deno) = private_tool("deno") {
        command
            .arg("--js-runtimes")
            .arg(format!("deno:{}", deno.display()));
    }
    command.arg("--no-plugin-dirs");
    // The account extractor reads cookies and identity from stdin; see send_credentials.
    if let Some(credentials) = credentials {
        command
            .arg("--plugin-dirs")
            .arg(&credentials.plugin_dir)
            .args(["--use-extractors", "^ytmusicaccount$"]);
    }
}

/// The credentials never touch argv, the environment or a file: the extractor
/// reads them from its own stdin pipe before its first request.
///
/// The line can exceed the pipe buffer, so a short-lived thread writes it and
/// the player thread never waits on the extractor. Killing the extractor
/// closes the pipe's read end, which ends the write with an error.
fn send_credentials(input: Arc<ChildStdin>, credentials: &ExtractorCredentials) -> Result<()> {
    let message = credentials.source.message.clone();
    thread::Builder::new()
        .name("credentials".into())
        .spawn(move || {
            let _ = (&*input).write_all(&message);
        })
        .context("Could not hand the account to the stream extractor")?;
    Ok(())
}

fn python_interpreter(ytdlp: &Path) -> Result<Command> {
    if let Some(python) = bundled_python(ytdlp) {
        return Ok(Command::new(python));
    }
    let mut header = [0; 1024];
    let count = File::open(ytdlp)?.read(&mut header)?;
    let line = header[..count]
        .split(|byte| *byte == b'\n')
        .next()
        .unwrap_or_default();
    let line = std::str::from_utf8(line)?
        .strip_prefix("#!")
        .context("Not a Python script")?;
    let words: Vec<_> = line.split_whitespace().collect();
    let executable = *words.first().context("Missing interpreter")?;
    let mut interpreter = 0;
    if Path::new(executable)
        .file_name()
        .is_some_and(|name| name == "env")
    {
        interpreter = if words.get(1) == Some(&"-S") { 2 } else { 1 };
    }
    let name = Path::new(
        words
            .get(interpreter)
            .context("Missing Python interpreter")?,
    )
    .file_name()
    .and_then(|name| name.to_str())
    .unwrap_or("");
    ensure!(
        name.strip_prefix("python").is_some_and(|version| {
            version
                .bytes()
                .all(|byte| byte.is_ascii_digit() || byte == b'.')
        }),
        "Not a Python interpreter"
    );
    let mut command = Command::new(executable);
    command.args(&words[1..]);
    Ok(command)
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ResolverResponse {
    Ready {
        ready: bool,
    },
    Stream {
        urls: Option<Vec<String>>,
        // The selected audio format's loudness fields, see ytdlp_resolver.py.
        #[serde(default)]
        loudness: Value,
        // The player response's caption tracks, see ytdlp_resolver.py.
        #[serde(default)]
        captions: Value,
    },
}

pub(super) const RESOLVER_TIMEOUT: Duration = Duration::from_secs(30);

pub(super) struct WarmResolver {
    pub(super) child: Group,
    pub(super) input: Arc<ChildStdin>,
    pub(super) output: ChildOutput,
    pub(super) bytes: Vec<u8>,
    pub(super) ready: bool,
    /// A signed-in resolver's request waits for `ready`: until then the
    /// account's credentials may still be on their way through stdin.
    pub(super) signed_in: bool,
    pub(super) request: Option<Vec<u8>>,
    pub(super) started: Instant,
}

impl WarmResolver {
    pub(super) fn start(ytdlp: &Path, credentials: Option<&ExtractorCredentials>) -> Result<Self> {
        let mut command = python_interpreter(ytdlp)?;
        command
            .args(["-I", "-B", "-u", "-c", include_str!("../ytdlp_resolver.py")])
            .arg(ytdlp);
        extractor_arguments(&mut command, credentials);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        // An extractor holds account cookies in memory. Its own process group keeps
        // it out of terminal signals, so it must not outlive the player thread.
        let mut child = Group::spawn(&mut command)?;
        let output = match (child.stdin.take(), child.stdout.take()) {
            (Some(input), Some(output)) => child_output(output).map(|output| (input, output)),
            _ => Err(io::Error::other("The resolver has no pipes")),
        };
        let (input, output) = match output {
            Ok(pipes) => pipes,
            Err(error) => {
                child.kill();
                let _ = child.wait();
                return Err(error).context("Could not read resolver output asynchronously");
            }
        };
        let resolver = Self {
            input: Arc::new(input),
            output,
            child,
            bytes: Vec::new(),
            ready: false,
            signed_in: credentials.is_some(),
            request: None,
            started: Instant::now(),
        };
        if let Some(credentials) = credentials {
            send_credentials(resolver.input.clone(), credentials)?;
        }
        Ok(resolver)
    }

    pub(super) fn send(&mut self, target: &PreparedTarget) -> Result<()> {
        let mut request =
            serde_json::to_vec(&json!({"url": target.key.url, "format": target.key.selector()}))?;
        request.push(b'\n');
        self.request = Some(request);
        if self.ready || !self.signed_in {
            self.flush()?;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        if let Some(request) = self.request.take() {
            (&*self.input).write_all(&request)?;
        }
        Ok(())
    }

    pub(super) fn read(&mut self) -> Result<Option<Option<native::Selection>>> {
        loop {
            if let Some(end) = self.bytes.iter().position(|byte| *byte == b'\n') {
                let line: Vec<_> = self.bytes.drain(..=end).collect();
                match serde_json::from_slice(&line)? {
                    ResolverResponse::Ready { ready: true } if !self.ready => {
                        self.ready = true;
                        self.flush()?;
                    }
                    ResolverResponse::Stream {
                        urls,
                        loudness,
                        captions,
                    } if self.ready => {
                        let gain_db =
                            normalization_gain(&loudness["audioConfig"], &loudness["format"]);
                        return Ok(Some(urls.map(|urls| native::Selection {
                            urls,
                            gain_db,
                            captions: native::captions(&captions),
                        })));
                    }
                    _ => bail!("Invalid resolver response"),
                }
            } else {
                let mut bytes = [0; 8192];
                match self.output.read(&mut bytes) {
                    Ok(0) => bail!("Resolver closed its output"),
                    Ok(count) => self.bytes.extend_from_slice(&bytes[..count]),
                    Err(error) if error.kind() == ErrorKind::WouldBlock => return Ok(None),
                    Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                    Err(error) => return Err(error.into()),
                }
                // Caption tracks for every language a video offers can reach tens of kilobytes.
                ensure!(
                    self.bytes.len() <= 1024 * 1024,
                    "Oversized resolver response"
                );
            }
        }
    }
}

impl Drop for WarmResolver {
    fn drop(&mut self) {
        // The group can still contain a JS runtime after Python has exited.
        self.child.kill();
        let _ = self.child.wait();
    }
}

pub(super) struct StreamPreparation {
    pub(super) target: PreparedTarget,
    pub(super) child: Group,
    pub(super) output: File,
    pub(super) exited: ExitNotice,
}

impl StreamPreparation {
    pub(super) fn start(
        target: PreparedTarget,
        ytdlp: &Path,
        credentials: Option<&ExtractorCredentials>,
    ) -> Result<Self> {
        // Keep signed URLs and any extractor output in anonymous RAM, never disk.
        let output = private_file()?;
        let mut command = ytdlp_command(ytdlp);
        extractor_arguments(&mut command, credentials);
        command
            .arg("--format")
            .arg(target.key.selector())
            .stdin(if credentials.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(output.try_clone()?)
            .stderr(Stdio::null());
        command.arg(&target.key.url);
        let mut child = Group::spawn(&mut command)?;
        // stdout is a memfd, so watch process exit instead of polling try_wait.
        let exited = match exit_notice(&child) {
            Ok(exited) => exited,
            Err(error) => {
                child.kill();
                let _ = child.wait();
                return Err(error).context("Could not watch the stream extractor");
            }
        };
        let input = child.stdin.take();
        let job = Self {
            target,
            child,
            output,
            exited,
        };
        if let (Some(input), Some(credentials)) = (input, credentials) {
            // The writer closes the pipe afterwards: the extractor gets nothing more.
            send_credentials(Arc::new(input), credentials)?;
        }
        Ok(job)
    }

    pub(super) fn finish(&mut self) -> Result<Option<ResolvedStream>> {
        let Some(status) = self.child.try_wait()? else {
            return Ok(None);
        };
        ensure!(status.success(), "Could not prepare the next stream");
        self.output.rewind()?;
        let mut bytes = Vec::new();
        Read::by_ref(&mut self.output)
            .take(16 * 1024 + 1)
            .read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= 16 * 1024, "Oversized prepared stream");
        // The CLI fallback prints only URLs, so these streams play unnormalized.
        ResolvedStream::parse(self.target.key.clone(), &bytes, 0., SystemTime::now()).map(Some)
    }
}

impl Drop for StreamPreparation {
    fn drop(&mut self) {
        // yt-dlp can start a JS runtime. Cancel its process group as well.
        self.child.kill();
        let _ = self.child.wait();
    }
}

// Account cookies may reach these tools. Refuse a redirected or writable path,
// including every directory through `top`, before preferring a private copy.
#[cfg(unix)]
fn trusted_tool(path: &Path, top: &Path, owner: u32) -> bool {
    if !top.is_absolute() || !path.starts_with(top) {
        return false;
    }
    for entry in path.ancestors() {
        let Ok(metadata) = fs::symlink_metadata(entry) else {
            return false;
        };
        if metadata.uid() != owner || metadata.mode() & 0o022 != 0 {
            return false;
        }
        if entry == path {
            if !metadata.is_file() || metadata.mode() & 0o100 == 0 {
                return false;
            }
        } else if !metadata.is_dir() {
            return false;
        }
        if entry == top {
            return true;
        }
    }
    false
}

// .deb and .rpm packages ship their root-owned helpers here.
#[cfg(unix)]
const PACKAGE_HELPERS: &str = "/usr/lib/yt-music-gpui";

// Each install prefers the helpers it shipped, whose versions it pins: the
// release installer's copies in HOME, or the copies bundled with the running
// executable. Builds without bundled helpers fall back to the package's.
#[cfg(unix)]
pub(crate) fn private_tool(name: &str) -> Option<PathBuf> {
    let installer = std::env::var_os("HOME").map(|home| {
        let home = PathBuf::from(home);
        let uid = unsafe { libc::geteuid() };
        (home.join(".local/share/yt-music-gpui"), home, uid)
    });
    // Package helpers must be root-owned all the way up from /. A macOS app's
    // need only match its executable: whoever can change them can change the app.
    let bundled = std::env::current_exe().ok().and_then(|exe| {
        let (directory, top) = bundled_helpers(&exe)?;
        let owner = if top == Path::new("/") {
            0
        } else {
            fs::metadata(&exe).ok()?.uid()
        };
        Some((directory, top, owner))
    });
    let candidates = match bundled {
        Some(bundled) => [Some(bundled), installer],
        None => [installer, Some((PACKAGE_HELPERS.into(), "/".into(), 0))],
    };
    candidates
        .into_iter()
        .flatten()
        .map(|(directory, top, owner)| (directory.join(name), top, owner))
        .find(|(path, top, owner)| trusted_tool(path, top, *owner))
        .map(|(path, ..)| path)
}

/// The Python inside the running macOS app, never a copy in the home folder.
#[cfg(target_os = "macos")]
pub(crate) fn app_python() -> Option<PathBuf> {
    let (resources, _) = bundled_helpers(&std::env::current_exe().ok()?)?;
    private_tool("python/bin/python3").filter(|python| python.starts_with(&resources))
}

// Where the running executable's own helpers live, by platform layout, and the
// directory they are checked up to. On Linux only the package binary has
// bundled helpers: /usr/bin/yt-music-gpui uses /usr/lib/yt-music-gpui. After a
// package upgrade replaces the running binary, Linux reports its old path with
// a " (deleted)" suffix. A macOS app keeps them in its Contents/Resources.
#[cfg(target_os = "macos")]
fn bundled_helpers(exe: &Path) -> Option<(PathBuf, PathBuf)> {
    let contents = exe.parent()?.parent()?;
    let app = contents.parent()?;
    (exe.parent()?.file_name()? == "MacOS"
        && contents.file_name()? == "Contents"
        && app.extension()? == "app")
        .then(|| (contents.join("Resources"), app.to_path_buf()))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn bundled_helpers(exe: &Path) -> Option<(PathBuf, PathBuf)> {
    let exe = exe.as_os_str().as_encoded_bytes();
    let exe = exe.strip_suffix(b" (deleted)").unwrap_or(exe);
    (exe == b"/usr/bin/yt-music-gpui").then(|| (PACKAGE_HELPERS.into(), "/".into()))
}

// macOS has no Python new enough for yt-dlp, so the app bundles one beside its
// zipapp. Any other yt-dlp runs with the interpreter its shebang names.
fn bundled_python(ytdlp: &Path) -> Option<PathBuf> {
    let python = private_tool("python/bin/python3")?;
    (python.ancestors().nth(3) == ytdlp.parent()).then_some(python)
}

fn ytdlp_command(ytdlp: &Path) -> Command {
    let Some(python) = bundled_python(ytdlp) else {
        return Command::new(ytdlp);
    };
    let mut command = Command::new(python);
    command.arg(ytdlp);
    command
}

/// Windows builds bundle no helpers yet, so yt-dlp and Deno come from PATH.
#[cfg(windows)]
pub(crate) fn private_tool(_: &str) -> Option<PathBuf> {
    None
}

/// Lets a native test stand in for yt-dlp. The installer's private copy comes
/// before PATH, so a test cannot rely on PATH to replace it.
#[cfg(all(test, unix))]
pub(crate) static TEST_YTDLP: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

pub(crate) fn find_ytdlp() -> Result<PathBuf> {
    #[cfg(all(test, unix))]
    if let Some(path) = TEST_YTDLP.get() {
        return Ok(path.clone());
    }
    private_tool("yt-dlp")
        .into_iter()
        .chain(
            std::env::var_os("PATH")
                .into_iter()
                .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
                .map(|path| path.join(format!("yt-dlp{}", std::env::consts::EXE_SUFFIX)))
                .chain(
                    std::env::var_os("HOME")
                        .map(|home| PathBuf::from(home).join(".local/bin/yt-dlp")),
                ),
        )
        .find(|path| path.is_file())
        .context("Could not find yt-dlp. Install it, then restart the app.")
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    #[cfg(unix)]
    use crate::player::stream::tests::resolver_target;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    #[cfg(unix)]
    #[test]
    fn private_tools_require_owned_non_writable_parents_and_regular_executables() {
        let home = tempfile::tempdir().unwrap();
        let directory = home.path().join(".local/share/yt-music-gpui");
        fs::create_dir_all(&directory).unwrap();
        let tool = directory.join("yt-dlp");
        fs::write(&tool, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
        let uid = unsafe { libc::geteuid() };
        assert!(trusted_tool(&tool, home.path(), uid));
        for path in [&tool, &directory, home.path()] {
            let mode = fs::metadata(path).unwrap().permissions().mode();
            fs::set_permissions(path, fs::Permissions::from_mode(mode | 0o020)).unwrap();
            assert!(!trusted_tool(&tool, home.path(), uid));
            fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
        }
        let link = directory.join("deno");
        std::os::unix::fs::symlink(&tool, &link).unwrap();
        assert!(!trusted_tool(&link, home.path(), uid));
        assert!(!trusted_tool(&tool, &directory.join("other-home"), uid));
        // Packaged copies must be root-owned all the way up from /.
        assert!(!trusted_tool(&tool, Path::new("/"), 0));
        // NixOS, Guix and rootless builds have no root-owned /usr/bin/env.
        let env = fs::symlink_metadata("/usr/bin/env");
        if env.is_ok_and(|env| env.is_file() && env.uid() == 0) {
            assert!(trusted_tool(Path::new("/usr/bin/env"), Path::new("/"), 0));
        }
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn only_the_package_binary_uses_bundled_helpers_even_after_an_upgrade_replaces_it() {
        let package = Some(("/usr/lib/yt-music-gpui".into(), "/".into()));
        assert_eq!(
            bundled_helpers(Path::new("/usr/bin/yt-music-gpui")),
            package
        );
        assert_eq!(
            bundled_helpers(Path::new("/usr/bin/yt-music-gpui (deleted)")),
            package
        );
        for exe in [
            "/usr/local/bin/yt-music-gpui",
            "/home/me/.local/bin/yt-music-gpui",
            "/home/me/yt-music-gpui/target/release/yt-music-gpui",
            "/usr/bin/yt-music-gpui-old",
        ] {
            assert_eq!(bundled_helpers(Path::new(exe)), None, "{exe}");
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn only_an_app_bundle_uses_its_resources_as_helpers() {
        assert_eq!(
            bundled_helpers(Path::new(
                "/Applications/YouTube Music.app/Contents/MacOS/yt-music-gpui"
            )),
            Some((
                "/Applications/YouTube Music.app/Contents/Resources".into(),
                "/Applications/YouTube Music.app".into()
            ))
        );
        for exe in [
            "/usr/bin/yt-music-gpui",
            "/Users/me/yt-music-gpui/target/release/yt-music-gpui",
            "/Users/me/Contents/MacOS/yt-music-gpui",
            "/Applications/YouTube Music.app/Contents/Resources/yt-music-gpui",
        ] {
            assert_eq!(bundled_helpers(Path::new(exe)), None, "{exe}");
        }
    }

    #[test]
    fn youtube_player_code_only_runs_in_sandboxed_deno() {
        let mut command = Command::new("yt-dlp");
        extractor_arguments(&mut command, None);
        let args: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        let runtimes: Vec<_> = args
            .windows(2)
            .filter(|pair| pair[0] == "--js-runtimes")
            .map(|pair| pair[1].clone())
            .collect();
        assert!(!runtimes.is_empty());
        assert!(
            runtimes
                .iter()
                .all(|runtime| runtime == "deno" || runtime.starts_with("deno:")),
            "{runtimes:?}"
        );
    }

    #[test]
    fn mismatched_extractor_uses_guest_streams_and_notifies_only_once() {
        let mut notice = None;
        for _ in 0..2 {
            assert!(
                account_credentials(
                    Some(synthetic_credentials("INVENTED")),
                    || false,
                    false,
                    &mut notice
                )
                .is_none()
            );
        }
        let message = notice
            .take()
            .expect("fallback notice must wait for playback");
        assert!(message.contains("2026.08.19"));
        assert!(message.contains("guest"));
        assert!(!message.contains("update yt-dlp"));
        assert!(
            account_credentials(
                Some(synthetic_credentials("INVENTED")),
                || false,
                true,
                &mut notice
            )
            .is_none()
        );
        assert!(
            notice.is_none(),
            "a displayed fallback notice must not repeat"
        );
        assert!(
            account_credentials(
                Some(synthetic_credentials("INVENTED")),
                || true,
                false,
                &mut notice
            )
            .is_some()
        );
        account_credentials(
            Some(synthetic_credentials("INVENTED")),
            || false,
            false,
            &mut notice,
        );
        account_credentials(
            None,
            || panic!("sign-out must not probe account support"),
            false,
            &mut notice,
        );
        assert!(
            notice.is_none(),
            "sign-out must cancel an undisplayed account notice"
        );
    }

    #[cfg(unix)]
    #[test]
    fn cancelling_preparation_reaps_the_signed_in_extractor() {
        let directory = tempfile::tempdir().unwrap();
        let extractor = directory.path().join("extractor");
        fs::write(&extractor, "#!/bin/sh\nexec sleep 30\n").unwrap();
        fs::set_permissions(&extractor, fs::Permissions::from_mode(0o700)).unwrap();
        let credentials = signed_in(synthetic_credentials("INVENTED"), directory.path());
        let target = resolver_target("abcdefghijk", 1);
        let job = StreamPreparation::start(target, &extractor, Some(&credentials)).unwrap();
        let pid = job.child.id();
        drop(job);
        assert!(!alive(pid));
    }

    #[cfg(unix)]
    /// Whether `pid` exists, including as an unreaped zombie.
    pub(crate) fn alive(pid: impl Into<u64>) -> bool {
        unsafe { libc::kill(pid.into() as libc::pid_t, 0) == 0 }
    }

    pub(crate) fn synthetic_account(
        value: &str,
        authuser: &str,
        brand: &str,
        user: &str,
    ) -> PlaybackCredentials {
        let session: crate::auth::Session = serde_json::from_value(json!({
            "cookie": format!("SAPISID={value}"),
            "playback_cookie": format!(
                "# Netscape HTTP Cookie File\n.youtube.com\tTRUE\t/\tTRUE\t0\tSAPISID\t{value}\n"
            ),
            "authuser": authuser,
            "delegated_session_id": brand,
            "data_sync_id": format!("{brand}||{user}"),
            "client_version": "synthetic",
        }))
        .unwrap();
        session.playback_credentials().unwrap()
    }

    pub(crate) fn synthetic_credentials(value: &str) -> PlaybackCredentials {
        synthetic_account(value, "2", "brand", "user")
    }

    pub(crate) fn signed_in(
        source: PlaybackCredentials,
        plugin_dir: &Path,
    ) -> ExtractorCredentials {
        ExtractorCredentials {
            plugin_dir: plugin_dir.to_owned(),
            source,
        }
    }
}
