//! Native desktop host for OpenHarness.
//!
//! Spawns the bundled `dsh web` server as a supervised child process (auto
//! restart on crash), hosts the browser UI in one native window, exposes live
//! task state in the system tray, and enforces a single
//! app instance. The bundled Node runtime and `@deepseek-ai/dsh` node_modules
//! live under the app's resource directory (`runtime/**` in tauri.conf.json).

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::cmp::Ordering as CompareOrdering;
use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

#[cfg(target_os = "macos")]
use std::ffi::{c_char, CStr};
#[cfg(unix)]
use std::os::unix::process::CommandExt;

use semver::Version;
use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem, Submenu},
    tray::{TrayIcon, TrayIconBuilder},
    AppHandle, Manager, RunEvent, Theme, WebviewUrl, WebviewWindowBuilder, Wry,
};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
use tauri_plugin_updater::UpdaterExt;
use url::{Host, Url};

mod runtime_paths;
mod status_bar;
use runtime_paths::{resolve_runtime, RuntimePaths};

use status_bar::{select_sessions, validate_snapshot, SessionMenuEntry, SessionMenuSnapshot};
#[cfg(test)]
use status_bar::{HISTORY_SESSION_LIMIT, TOP_SESSION_LIMIT};

/// How long to wait for the DSH server to print its URL before giving up.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
/// Consecutive backend failures before surfacing an error dialog.
const MAX_RESTART_ATTEMPTS: u32 = 3;
/// Runtime long enough to treat an earlier failure streak as recovered.
const STABLE_BACKEND_UPTIME: Duration = Duration::from_secs(30);
/// Keep a slow or broken shell profile from blocking desktop startup.
const SHELL_ENV_TIMEOUT: Duration = Duration::from_secs(5);
/// A normal login environment is small; larger output is treated as invalid.
const SHELL_ENV_OUTPUT_LIMIT: usize = 1024 * 1024;
const MANAGED_RUNTIME_PROTOCOL_VERSION: &str = "1";
const MANAGED_RESTART_EXIT_CODE: i32 = 75;
const DSH_PROFILE_NAME: &str = "web";
const FIND_PLUGIN_PACKAGE: &str = "@microspotlight/openharness-find-plugin";
const UPDATE_NOTES_LIMIT: usize = 1_200;
const APP_UPDATE_MENU_ID: &str = "app-update";
const SESSION_MENU_LABEL_LIMIT: usize = 96;
const SESSION_MENU_ID_PREFIX: &str = "session:";
const WEBVIEW_INIT_SCRIPT: &str = include_str!("../webview-init.js");
/// Rotate the backend log rather than growing it without bound.
const BACKEND_LOG_LIMIT: u64 = 1024 * 1024;
/// Startup diagnostics carried into the error dialog; the full log has more.
const ERROR_DIALOG_OUTPUT_LINES: usize = 6;
const ERROR_DIALOG_LINE_LIMIT: usize = 240;
/// Backend output kept in memory for failure reports, in lines.
const RECENT_OUTPUT_LIMIT: usize = 12;

#[cfg(target_os = "macos")]
extern "C" {
    fn openharness_current_build_number() -> *const c_char;
    fn openharness_install_webview_context_menu_filter();
    fn openharness_preferred_native_locale() -> i32;
}

#[cfg(target_os = "macos")]
fn install_native_context_menu_filter() {
    // The native function is process-global and idempotent via dispatch_once.
    unsafe { openharness_install_webview_context_menu_filter() };
}

#[cfg(not(target_os = "macos"))]
fn install_native_context_menu_filter() {}

#[derive(Clone, Copy, PartialEq, Eq)]
enum UpdateCheckSource {
    Automatic,
    Manual,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum NativeLocale {
    Zh,
    En,
}

impl NativeLocale {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "zh" => Some(Self::Zh),
            "en" => Some(Self::En),
            _ => None,
        }
    }

    fn text(self, zh: &'static str, en: &'static str) -> &'static str {
        match self {
            Self::Zh => zh,
            Self::En => en,
        }
    }
}

fn preferred_native_locale() -> NativeLocale {
    #[cfg(target_os = "macos")]
    {
        if unsafe { openharness_preferred_native_locale() } == 1 {
            NativeLocale::En
        } else {
            NativeLocale::Zh
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        NativeLocale::Zh
    }
}

#[derive(Clone, Copy)]
enum UpdateMenuStatus {
    Idle,
    Checking,
    Downloading(Option<u8>),
    Installing,
    Restarting,
}

#[derive(Clone)]
struct AppMenuItems {
    about: PredefinedMenuItem<Wry>,
    update: MenuItem<Wry>,
    services: PredefinedMenuItem<Wry>,
    hide: PredefinedMenuItem<Wry>,
    hide_others: PredefinedMenuItem<Wry>,
    quit: PredefinedMenuItem<Wry>,
}

#[derive(Clone)]
struct TrayMenuItems {
    tray: TrayIcon<Wry>,
    snapshot: Arc<Mutex<SessionMenuSnapshot>>,
}

#[derive(Clone)]
struct NativeUi {
    locale: Arc<Mutex<NativeLocale>>,
    update_status: Arc<Mutex<UpdateMenuStatus>>,
    app_menu: AppMenuItems,
    tray_menu: TrayMenuItems,
}

struct AppUpdateState {
    busy: AtomicBool,
    automatic_check_started: AtomicBool,
    ui: NativeUi,
}

/// Shared backend state: the running child PID (for quit-time kill), the
/// current canonical URL (for the main window), and coordination flags.
struct BackendState {
    child_pid: Mutex<Option<u32>>,
    url: Mutex<Option<Url>>,
    quitting: AtomicBool,
}

impl BackendState {
    fn new() -> Self {
        Self {
            child_pid: Mutex::new(None),
            url: Mutex::new(None),
            quitting: AtomicBool::new(false),
        }
    }

    fn set_url(&self, url: Url) {
        *self.url.lock().unwrap() = Some(url);
    }

    fn mark_stopped(&self) {
        *self.child_pid.lock().unwrap() = None;
        *self.url.lock().unwrap() = None;
    }

    fn current_url(&self) -> Option<Url> {
        self.url.lock().unwrap().clone()
    }
}

/// Extract the URL from the `dsh web: http://127.0.0.1:PORT` boot line,
/// accepting only loopback HTTP URLs with an explicit port and no credentials.
fn parse_url(line: &str) -> Option<Url> {
    const MARKER: &str = "dsh web: ";
    let rest = line.find(MARKER).map(|i| &line[i + MARKER.len()..])?;
    let candidate = rest.split_whitespace().next()?;
    let url = Url::parse(candidate).ok()?;

    if url.scheme() != "http"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_none()
    {
        return None;
    }

    match url.host()? {
        Host::Ipv4(address) if address.is_loopback() => Some(url),
        Host::Ipv6(address) if address.is_loopback() => Some(url),
        Host::Domain(domain) if domain.eq_ignore_ascii_case("localhost") => Some(url),
        _ => None,
    }
}

fn runtime_log_line(line: &str) -> &str {
    // Newer DSH versions put a browser authentication token in the boot URL.
    if line.contains("dsh web: ") {
        "dsh web: [startup URL omitted]"
    } else {
        line
    }
}

fn wait_for_startup_url(
    output: &mpsc::Receiver<Result<String, std::io::Error>>,
    timeout: Duration,
) -> Result<Url, std::io::Error> {
    let deadline = Instant::now() + timeout;

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(startup_timeout_error());
        }

        match output.recv_timeout(remaining) {
            Ok(Ok(line)) => {
                if let Some(url) = parse_url(&line) {
                    return Ok(url);
                }
            }
            Ok(Err(error)) => return Err(error),
            Err(mpsc::RecvTimeoutError::Timeout) => return Err(startup_timeout_error()),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "dsh web exited before reporting its URL",
                ));
            }
        }
    }
}

fn startup_timeout_error() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        "timed out waiting for the dsh web server to report its URL",
    )
}

fn next_failure_count(current: u32, uptime: Option<Duration>) -> u32 {
    let previous = if uptime.is_some_and(|duration| duration >= STABLE_BACKEND_UPTIME) {
        0
    } else {
        current
    };
    previous.saturating_add(1)
}

fn restart_delay(failures: u32) -> Duration {
    Duration::from_secs(2u64.saturating_pow(failures.min(6)).min(30))
}

fn is_managed_restart_code(code: Option<i32>) -> bool {
    code == Some(MANAGED_RESTART_EXIT_CODE)
}

fn read_bounded<R: Read>(mut reader: R, limit: usize) -> Result<Vec<u8>, std::io::Error> {
    let mut output = Vec::new();
    let mut buffer = [0u8; 8192];
    let mut overflowed = false;

    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }

        let remaining = limit.saturating_sub(output.len());
        output.extend_from_slice(&buffer[..count.min(remaining)]);
        overflowed |= count > remaining;
    }

    if overflowed {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("login shell environment exceeded {limit} bytes"),
        ))
    } else {
        Ok(output)
    }
}

fn command_stdout_with_timeout(
    command: &mut Command,
    timeout: Duration,
    output_limit: usize,
) -> Result<Vec<u8>, std::io::Error> {
    // Shell profiles can start child processes. A dedicated process group lets
    // timeout and read-failure paths clean up the complete temporary tree.
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let stdout = child.stdout.take().expect("stdout was piped");
    let (output_tx, output_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = output_tx.send(read_bounded(stdout, output_limit));
    });

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(error) => {
                terminate_command(&mut child);
                return Err(error);
            }
        }
        if Instant::now() >= deadline {
            terminate_command(&mut child);
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!(
                    "login shell environment timed out after {} seconds",
                    timeout.as_secs()
                ),
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    };

    let output = match output_rx.recv_timeout(Duration::from_secs(1)) {
        Ok(output) => output?,
        Err(_) => {
            terminate_command(&mut child);
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "timed out reading login shell environment",
            ));
        }
    };
    if !status.success() {
        return Err(std::io::Error::other(format!(
            "login shell environment command exited with {status}"
        )));
    }
    Ok(output)
}

/// Backend diagnostics must outlive the console: Windows GUI builds have no
/// stderr, so `eprintln!` alone cannot explain a startup failure to the user.
/// The path is settable rather than write-once so app setup can refine the early
/// launch-time guess into the platform's application-data directory.
static BACKEND_LOG: Mutex<Option<PathBuf>> = Mutex::new(None);
static LAST_LOG_ERROR: AtomicBool = AtomicBool::new(false);

fn last_log_error() -> &'static AtomicBool {
    &LAST_LOG_ERROR
}

fn resolve_log_directory(
    local_app_data: Option<OsString>,
    xdg_state_home: Option<OsString>,
    home: Option<OsString>,
) -> PathBuf {
    if let Some(directory) = local_app_data.filter(|value| !value.is_empty()) {
        return PathBuf::from(directory).join("OpenHarness/logs");
    }
    if let Some(directory) = xdg_state_home.filter(|value| !value.is_empty()) {
        return PathBuf::from(directory).join("OpenHarness/logs");
    }
    if let Some(directory) = home.filter(|value| !value.is_empty()) {
        return PathBuf::from(directory).join(".local/state/OpenHarness/logs");
    }
    std::env::temp_dir().join("OpenHarness/logs")
}

fn fallback_log_directory() -> PathBuf {
    resolve_log_directory(
        std::env::var_os("LOCALAPPDATA"),
        std::env::var_os("XDG_STATE_HOME"),
        std::env::var_os("HOME"),
    )
}

fn install_backend_log(directory: PathBuf) {
    let _ = fs::create_dir_all(&directory);
    if let Ok(mut path) = BACKEND_LOG.lock() {
        *path = Some(directory.join("backend.log"));
    }
}

/// Reserve the log path before any runtime work starts, so the earliest startup
/// failure still has somewhere to land.
fn initialize_backend_log() {
    install_backend_log(fallback_log_directory());
}

/// The app framework knows the platform's application-data directory, which is a
/// better home for logs than the environment-variable fallback.
fn finalize_backend_log_directory(app_local_data: Option<PathBuf>) {
    let directory = match app_local_data.filter(|path| !path.as_os_str().is_empty()) {
        Some(base) => base.join("OpenHarness/logs"),
        None => fallback_log_directory(),
    };
    install_backend_log(directory);
}

fn backend_log_path() -> Option<PathBuf> {
    BACKEND_LOG.lock().ok().and_then(|path| path.clone())
}

fn rotate_backend_log(path: &Path) {
    let oversized = fs::metadata(path).is_ok_and(|metadata| metadata.len() > BACKEND_LOG_LIMIT);
    if !oversized {
        return;
    }
    let _ = fs::remove_file(path.with_extension("log.1"));
    let _ = fs::rename(path, path.with_extension("log.1"));
}

/// Append one line to the backend log. Drop logs to a single failed attempt
/// instead of retrying on every line.
fn write_backend_log(line: &str) {
    let Some(path) = backend_log_path() else {
        return;
    };
    if last_log_error().load(Ordering::SeqCst) {
        return;
    }
    rotate_backend_log(&path);
    match fs::OpenOptions::new().create(true).append(true).open(&path) {
        Ok(mut file) => {
            if std::io::Write::write_all(&mut file, format!("{line}\n").as_bytes()).is_err() {
                last_log_error().store(true, Ordering::SeqCst);
            }
        }
        Err(_) => last_log_error().store(true, Ordering::SeqCst),
    }
}

/// Read a child stream line by line without demanding UTF-8. A strict
/// `BufRead::read_line` fails on the first non-UTF-8 byte, which would end the
/// reader thread and make the supervisor report a startup failure even while
/// the child is still running.
fn read_stream_lines<R: Read>(mut stream: R, mut on_line: impl FnMut(String)) {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match stream.read(&mut byte) {
            Ok(0) => break,
            Ok(_) if byte[0] == b'\n' => {
                let owned = std::mem::take(&mut line);
                on_line(String::from_utf8_lossy(&owned).into_owned());
            }
            Ok(_) => line.push(byte[0]),
            Err(_) => break,
        }
    }
    if !line.is_empty() {
        on_line(String::from_utf8_lossy(&line).into_owned());
    }
}

/// Forward one backend stream to the log, the recent-output buffer, and the
/// startup channel, with the boot URL redacted.
fn forward_backend_stream<R: Read>(
    stream: R,
    prefix: &'static str,
    recent: Arc<Mutex<Vec<String>>>,
    output_tx: Option<mpsc::Sender<Result<String, std::io::Error>>>,
) {
    read_stream_lines(stream, |line| {
        let trimmed = line.trim_end().to_owned();
        let logged = runtime_log_line(&trimmed);
        write_backend_log(&format!("{prefix}{logged}"));
        if !trimmed.is_empty() {
            if let Ok(mut lines) = recent.lock() {
                lines.push(logged.to_owned());
                if lines.len() > RECENT_OUTPUT_LIMIT {
                    lines.remove(0);
                }
            }
        }
        if let Some(sender) = &output_tx {
            let _ = sender.send(Ok(trimmed));
        }
    });
}

fn truncate_for_dialog(line: &str) -> String {
    if line.chars().count() <= ERROR_DIALOG_LINE_LIMIT {
        return line.to_owned();
    }
    let mut truncated: String = line.chars().take(ERROR_DIALOG_LINE_LIMIT).collect();
    truncated.push('…');
    truncated
}

/// Compose a failure message a user can act on: what happened, what the backend
/// said, and where the full log lives.
fn backend_failure_message(
    detail: &str,
    recent: &Mutex<Vec<String>>,
    log_path: Option<&Path>,
) -> String {
    let lines = recent.lock().map(|lines| lines.clone()).unwrap_or_default();
    let tail: Vec<&String> = lines
        .iter()
        .rev()
        .take(ERROR_DIALOG_OUTPUT_LINES)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let mut message = detail.to_owned();
    if !tail.is_empty() {
        message.push_str("\n\n");
        message.push_str(
            &tail
                .iter()
                .map(|line| truncate_for_dialog(line))
                .collect::<Vec<_>>()
                .join("\n"),
        );
    }
    if let Some(path) = log_path {
        message.push_str(&format!("\n\nLog: {}", path.display()));
    }
    message
}

fn terminate_command(child: &mut Child) {
    #[cfg(unix)]
    unsafe {
        // The child is the process-group leader because `process_group(0)` was
        // set before spawn. A negative PID targets every process in that group.
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn valid_env_key(key: &str) -> bool {
    let mut bytes = key.bytes();
    matches!(bytes.next(), Some(b'A'..=b'Z' | b'a'..=b'z' | b'_'))
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn parse_shell_env(output: &[u8]) -> Vec<(String, String)> {
    let Some(marker) = output.iter().position(|byte| *byte == 0) else {
        return Vec::new();
    };

    output[marker + 1..]
        .split(|byte| *byte == 0)
        .filter_map(|entry| {
            let entry = std::str::from_utf8(entry).ok()?;
            let (key, value) = entry.split_once('=')?;
            if !value.is_empty()
                && valid_env_key(key)
                && (key == "PATH" || key.starts_with("DEEPSEEK_"))
            {
                Some((key.to_string(), value.to_string()))
            } else {
                None
            }
        })
        .collect()
}

fn managed_runtime_path(
    runtime: &RuntimePaths,
    shell_env: &[(String, String)],
    inherited_path: Option<&OsStr>,
) -> OsString {
    let mut seen = HashSet::new();
    let mut paths = Vec::new();
    for path in [
        runtime.package_manager_bin.clone(),
        runtime
            .node
            .parent()
            .expect("bundled Node path has no parent")
            .to_path_buf(),
    ] {
        if seen.insert(path.clone()) {
            paths.push(path);
        }
    }
    let shell_path = shell_env
        .iter()
        .find_map(|(key, value)| (key == "PATH").then_some(value.as_str()));
    for source in shell_path.map(OsStr::new).into_iter().chain(inherited_path) {
        for path in std::env::split_paths(source) {
            if !path.as_os_str().is_empty() && seen.insert(path.clone()) {
                paths.push(path);
            }
        }
    }
    std::env::join_paths(paths)
        .unwrap_or_else(|_| inherited_path.map(OsString::from).unwrap_or_default())
}

fn resolve_dsh_home(home: &Path) -> PathBuf {
    std::env::var_os("DSH_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .map(|path| {
            if path.is_absolute() {
                path
            } else {
                home.join(path)
            }
        })
        .unwrap_or_else(|| home.join(".dsh"))
}

fn manifest_declares_bundle(manifest: &serde_json::Value, bundle: &str) -> bool {
    manifest
        .pointer("/dsh/profile/bundles")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|bundles| bundles.iter().any(|value| value.as_str() == Some(bundle)))
}

fn profile_declares_bundle(profile_directory: &Path, bundle: &str) -> bool {
    fs::read(profile_directory.join("package.json"))
        .ok()
        .and_then(|content| serde_json::from_slice(&content).ok())
        .is_some_and(|manifest| manifest_declares_bundle(&manifest, bundle))
}

/// Read PATH and DeepSeek variables from the user's login shell. Finder does
/// not inherit the login environment, so desktop launches otherwise miss user
/// tools and credentials. Values are bounded and never logged.
#[cfg(unix)]
fn resolve_login_shell(configured: Option<&OsStr>) -> PathBuf {
    configured
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.is_absolute() && path.is_file())
        .unwrap_or_else(|| PathBuf::from("/bin/sh"))
}

#[cfg(unix)]
fn load_shell_env() -> Result<Vec<(String, String)>, std::io::Error> {
    let configured_shell = std::env::var_os("SHELL");
    let shell = resolve_login_shell(configured_shell.as_deref());
    let output = command_stdout_with_timeout(
        Command::new(&shell).args(["-lic", "printf '\\0'; exec /usr/bin/env -0"]),
        SHELL_ENV_TIMEOUT,
        SHELL_ENV_OUTPUT_LIMIT,
    )?;
    Ok(parse_shell_env(&output))
}

#[cfg(not(unix))]
fn load_shell_env() -> Result<Vec<(String, String)>, std::io::Error> {
    Ok(Vec::new())
}

fn load_shell_env_or_empty() -> Vec<(String, String)> {
    match load_shell_env() {
        Ok(shell_env) => {
            let has_path = shell_env.iter().any(|(key, _)| key == "PATH");
            let deepseek_count = shell_env
                .iter()
                .filter(|(key, _)| key.starts_with("DEEPSEEK_"))
                .count();
            eprintln!(
                "[harness] login shell environment loaded (PATH: {has_path}, DeepSeek variables: {deepseek_count})"
            );
            shell_env
        }
        Err(error) => {
            eprintln!(
                "[harness] login shell environment unavailable; using inherited environment: {error}"
            );
            Vec::new()
        }
    }
}

fn configure_harness_args(
    command: &mut Command,
    runtime: &RuntimePaths,
    profile_has_find_plugin: bool,
) {
    command
        .arg(&runtime.dsh_entry)
        .args(["--profile", DSH_PROFILE_NAME])
        .arg("--patch")
        .arg(&runtime.patch);
    if !profile_has_find_plugin {
        command.arg("--patch").arg(&runtime.find_plugin_patch);
    }
    command.args(["--host", "127.0.0.1", "--port", "0", "--no-open"]);
}

/// Spawn the DSH web server and block until it reports its canonical URL.
fn spawn_harness(
    resource_dir: &Path,
    home: &Path,
    shell_env: &[(String, String)],
) -> Result<(Child, Url, Arc<Mutex<Vec<String>>>), Box<dyn std::error::Error>> {
    let runtime = resolve_runtime(resource_dir)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::NotFound, e))?;
    let dsh_home = resolve_dsh_home(home);
    let profile_directory = dsh_home.join("profiles").join(DSH_PROFILE_NAME);
    fs::create_dir_all(&dsh_home)?;
    let inherited_path = std::env::var_os("PATH");
    let executable_path = managed_runtime_path(&runtime, shell_env, inherited_path.as_deref());
    let profile_has_find_plugin = profile_declares_bundle(&profile_directory, FIND_PLUGIN_PACKAGE);

    let mut command = Command::new(&runtime.node);
    configure_harness_args(&mut command, &runtime, profile_has_find_plugin);
    command
        .env("PATH", executable_path)
        .env("DSH_HOME", &dsh_home)
        .env("DSH_TELEMETRY_DISABLED", "1")
        .env("OPENHARNESS_MANAGED_RUNTIME", "1")
        .env(
            "OPENHARNESS_MANAGED_RUNTIME_PROTOCOL",
            MANAGED_RUNTIME_PROTOCOL_VERSION,
        )
        .env("OPENHARNESS_PROFILE_NAME", DSH_PROFILE_NAME)
        .env("OPENHARNESS_PROFILE_DIRECTORY", &profile_directory)
        .env("OPENHARNESS_RUNTIME_ROOT", &runtime.root)
        .env("OPENHARNESS_NODE_PATH", &runtime.node)
        .env("OPENHARNESS_DSH_ENTRY", &runtime.dsh_entry)
        .env(
            "OPENHARNESS_PACKAGE_MANAGER_BIN",
            &runtime.package_manager_bin,
        )
        .env(
            "OPENHARNESS_RESTART_EXIT_CODE",
            MANAGED_RESTART_EXIT_CODE.to_string(),
        )
        .current_dir(home)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in shell_env {
        if key != "PATH" && std::env::var_os(key).is_none_or(|value| value.is_empty()) {
            command.env(key, value);
        }
    }
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command
        .spawn()
        .map_err(|e| std::io::Error::other(format!("failed to spawn dsh: {e}")))?;

    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");

    // Startup diagnostics must survive the console-less Windows build.
    write_backend_log(&format!(
        "--- dsh web started: {}",
        runtime.dsh_entry.display()
    ));

    // Forward the server's stderr to the log so startup failures stay visible.
    let recent = Arc::new(Mutex::new(Vec::new()));
    std::thread::spawn({
        let recent = Arc::clone(&recent);
        move || forward_backend_stream(stderr, "[harness:stderr] ", recent, None)
    });

    let (output_tx, output_rx) = mpsc::channel();
    std::thread::spawn({
        let recent = Arc::clone(&recent);
        move || forward_backend_stream(stdout, "[harness] ", recent, Some(output_tx))
    });

    match wait_for_startup_url(&output_rx, STARTUP_TIMEOUT) {
        Ok(url) => Ok((child, url, recent)),
        Err(error) => {
            terminate_command(&mut child);
            Err(
                backend_failure_message(&error.to_string(), &recent, backend_log_path().as_deref())
                    .into(),
            )
        }
    }
}

/// Supervise the harness backend: spawn it, keep its URL published, and restart
/// it (with backoff) if it exits unexpectedly. Runs until the app quits.
fn backend_supervisor(
    state: Arc<BackendState>,
    resource_dir: PathBuf,
    home: PathBuf,
    shell_env: Vec<(String, String)>,
    app: tauri::AppHandle,
) {
    let mut failures: u32 = 0;
    let mut error_reported = false;
    loop {
        if state.quitting.load(Ordering::SeqCst) {
            break;
        }
        match spawn_harness(&resource_dir, &home, &shell_env) {
            Ok((mut child, url, recent)) => {
                let started_at = Instant::now();
                let pid = child.id();
                *state.child_pid.lock().unwrap() = Some(pid);
                state.set_url(url.clone());

                // Point the single business window at the validated backend.
                // If startup wins the race, window creation reads current_url.
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.navigate(url.clone());
                }

                // Block until the backend exits (crash or quit-time kill).
                let exit_status = child.wait();
                state.mark_stopped();

                if state.quitting.load(Ordering::SeqCst) {
                    break;
                }
                if matches!(&exit_status, Ok(status) if is_managed_restart_code(status.code())) {
                    failures = 0;
                    error_reported = false;
                    eprintln!("[harness] managed runtime requested a backend restart");
                    continue;
                }
                if let Some(update_state) = app.try_state::<AppUpdateState>() {
                    let ui = update_state.ui.clone();
                    let update_enabled = !update_state.busy.load(Ordering::SeqCst);
                    if let Err(error) = app.run_on_main_thread(move || {
                        ui.set_session_snapshot(SessionMenuSnapshot::default(), update_enabled);
                    }) {
                        eprintln!("[harness] failed to reset Status Bar sessions: {error}");
                    }
                }

                let uptime = started_at.elapsed();
                if uptime >= STABLE_BACKEND_UPTIME {
                    error_reported = false;
                }
                failures = next_failure_count(failures, Some(uptime));
                let detail = match exit_status {
                    Ok(status) => format!("backend exited unexpectedly with {status}"),
                    Err(error) => format!("failed to wait for backend process: {error}"),
                };
                eprintln!("[harness] {detail} ({failures}); restarting");
                write_backend_log(&format!("[harness] {detail} ({failures}); restarting"));
                if failures >= MAX_RESTART_ATTEMPTS && !error_reported {
                    error_reported = true;
                    show_backend_error(
                        &app,
                        &backend_failure_message(&detail, &recent, backend_log_path().as_deref()),
                    );
                }
                std::thread::sleep(restart_delay(failures));
            }
            Err(e) => {
                failures = next_failure_count(failures, None);
                let detail = e.to_string();
                eprintln!("[harness] backend spawn failed ({failures}): {detail}");
                write_backend_log(&format!(
                    "[harness] backend spawn failed ({failures}): {detail}"
                ));
                if failures >= MAX_RESTART_ATTEMPTS && !error_reported {
                    error_reported = true;
                    show_backend_error(&app, &detail);
                }
                std::thread::sleep(restart_delay(failures));
            }
        }
    }
    eprintln!("[harness] backend supervisor exiting");
}

/// Surface a native error dialog when the backend repeatedly fails.
fn show_backend_error(app: &tauri::AppHandle, err: &dyn std::fmt::Display) {
    let locale = native_locale(app);
    app.dialog()
        .message(match locale {
            NativeLocale::Zh => format!("OpenHarness 后端运行失败：{err}"),
            NativeLocale::En => format!("The OpenHarness backend failed: {err}"),
        })
        .title("OpenHarness")
        .kind(MessageDialogKind::Error)
        .show(|_| {});
}

fn show_update_message(app: &AppHandle, message: impl Into<String>, kind: MessageDialogKind) {
    app.dialog()
        .message(message)
        .title("OpenHarness")
        .kind(kind)
        .show(|_| {});
}

fn update_menu_label(locale: NativeLocale, status: UpdateMenuStatus) -> String {
    match status {
        UpdateMenuStatus::Idle => locale
            .text("检查更新...", "Check for Updates...")
            .to_string(),
        UpdateMenuStatus::Checking => locale
            .text("正在检查更新...", "Checking for Updates...")
            .to_string(),
        UpdateMenuStatus::Downloading(Some(percent)) => match locale {
            NativeLocale::Zh => format!("正在下载更新... {percent}%"),
            NativeLocale::En => format!("Downloading Update... {percent}%"),
        },
        UpdateMenuStatus::Downloading(None) => locale
            .text("正在下载更新...", "Downloading Update...")
            .to_string(),
        UpdateMenuStatus::Installing => locale
            .text("正在安装更新...", "Installing Update...")
            .to_string(),
        UpdateMenuStatus::Restarting => locale.text("正在重启...", "Restarting...").to_string(),
    }
}

fn clean_menu_text(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn truncate_menu_text(value: String) -> String {
    if value.chars().count() <= SESSION_MENU_LABEL_LIMIT {
        return value;
    }
    let mut truncated = value
        .chars()
        .take(SESSION_MENU_LABEL_LIMIT.saturating_sub(3))
        .collect::<String>();
    truncated.push_str("...");
    truncated
}

fn session_menu_label(locale: NativeLocale, session: &SessionMenuEntry) -> String {
    let title = clean_menu_text(&session.title);
    let title = if title.is_empty() {
        locale.text("未命名会话", "Untitled Session").to_string()
    } else {
        title
    };
    let workspace = session
        .workspace
        .as_deref()
        .map(clean_menu_text)
        .filter(|workspace| !workspace.is_empty() && workspace != &title);
    let subject = match workspace {
        Some(workspace) => format!("{title} - {workspace}"),
        None => title,
    };
    let prefix = match session.pending_interaction.as_deref() {
        Some("approval") => locale.text("[需批准] ", "[Approval Needed] "),
        Some("question" | "plan-review") => locale.text("[待回答] ", "[Waiting for You] "),
        _ if session.running => locale.text("[进行中] ", "[Running] "),
        _ if session.completed => locale.text("[完成待查看] ", "[Ready to Review] "),
        _ => "",
    };
    truncate_menu_text(format!("{prefix}{subject}"))
}

fn build_tray_menu(
    app: &AppHandle,
    locale: NativeLocale,
    snapshot: &SessionMenuSnapshot,
    update_status: UpdateMenuStatus,
    update_enabled: bool,
) -> tauri::Result<Menu<Wry>> {
    let menu = Menu::new(app)?;
    let (top, history) = select_sessions(&snapshot.sessions);

    if !snapshot.ready {
        menu.append(&MenuItem::with_id(
            app,
            "sessions-loading",
            locale.text("正在连接 Harness...", "Connecting to Harness..."),
            false,
            None::<&str>,
        )?)?;
    } else if top.is_empty() {
        menu.append(&MenuItem::with_id(
            app,
            "sessions-empty",
            locale.text("暂无会话", "No Sessions"),
            false,
            None::<&str>,
        )?)?;
    } else {
        for session in &top {
            menu.append(&MenuItem::with_id(
                app,
                format!("{SESSION_MENU_ID_PREFIX}{}", session.id),
                session_menu_label(locale, session),
                true,
                None::<&str>,
            )?)?;
        }
    }

    let more = Submenu::new(app, locale.text("更多会话", "More Sessions"), true)?;
    if history.is_empty() {
        more.append(&MenuItem::with_id(
            app,
            "history-empty",
            locale.text("暂无更多会话", "No More Sessions"),
            false,
            None::<&str>,
        )?)?;
    } else {
        for session in history {
            more.append(&MenuItem::with_id(
                app,
                format!("{SESSION_MENU_ID_PREFIX}{}", session.id),
                session_menu_label(locale, session),
                true,
                None::<&str>,
            )?)?;
        }
    }
    more.append(&PredefinedMenuItem::separator(app)?)?;
    more.append(&MenuItem::with_id(
        app,
        "show-all",
        locale.text("在 Harness 中查看全部...", "View All in Harness..."),
        true,
        None::<&str>,
    )?)?;
    menu.append(&more)?;

    menu.append(&PredefinedMenuItem::separator(app)?)?;
    menu.append(&MenuItem::with_id(
        app,
        "show",
        locale.text("打开 Harness", "Open Harness"),
        true,
        None::<&str>,
    )?)?;
    menu.append(&MenuItem::with_id(
        app,
        "new-session",
        locale.text("新建会话", "New Session"),
        snapshot.ready,
        None::<&str>,
    )?)?;
    menu.append(&PredefinedMenuItem::separator(app)?)?;
    menu.append(&MenuItem::with_id(
        app,
        "update",
        update_menu_label(locale, update_status),
        update_enabled,
        None::<&str>,
    )?)?;
    menu.append(&MenuItem::with_id(
        app,
        "quit",
        locale.text("退出 OpenHarness", "Quit OpenHarness"),
        true,
        None::<&str>,
    )?)?;
    Ok(menu)
}

impl TrayMenuItems {
    fn rebuild(
        &self,
        locale: NativeLocale,
        update_status: UpdateMenuStatus,
        update_enabled: bool,
    ) -> tauri::Result<()> {
        let snapshot = self.snapshot.lock().unwrap().clone();
        let menu = build_tray_menu(
            self.tray.app_handle(),
            locale,
            &snapshot,
            update_status,
            update_enabled,
        )?;
        self.tray.set_menu(Some(menu))
    }

    fn set_snapshot(
        &self,
        snapshot: SessionMenuSnapshot,
        locale: NativeLocale,
        update_status: UpdateMenuStatus,
        update_enabled: bool,
    ) -> tauri::Result<()> {
        *self.snapshot.lock().unwrap() = snapshot;
        self.rebuild(locale, update_status, update_enabled)
    }
}

impl NativeUi {
    fn locale(&self) -> NativeLocale {
        *self.locale.lock().unwrap()
    }

    fn render_update_status(&self, enabled: bool) {
        let locale = self.locale();
        let status = *self.update_status.lock().unwrap();
        let label = update_menu_label(locale, status);
        let _ = self.app_menu.update.set_text(&label);
        let _ = self.app_menu.update.set_enabled(enabled);
        if let Err(error) = self.tray_menu.rebuild(locale, status, enabled) {
            eprintln!("[harness] failed to rebuild Status Bar menu: {error}");
        }
    }

    fn set_update_status(&self, status: UpdateMenuStatus, enabled: bool) {
        *self.update_status.lock().unwrap() = status;
        self.render_update_status(enabled);
    }

    fn set_locale(&self, locale: NativeLocale, update_enabled: bool) {
        *self.locale.lock().unwrap() = locale;
        let _ = self
            .app_menu
            .about
            .set_text(locale.text("关于 OpenHarness", "About OpenHarness"));
        let _ = self
            .app_menu
            .services
            .set_text(locale.text("服务", "Services"));
        let _ = self
            .app_menu
            .hide
            .set_text(locale.text("隐藏 OpenHarness", "Hide OpenHarness"));
        let _ = self
            .app_menu
            .hide_others
            .set_text(locale.text("隐藏其他", "Hide Others"));
        let _ = self
            .app_menu
            .quit
            .set_text(locale.text("退出 OpenHarness", "Quit OpenHarness"));
        self.render_update_status(update_enabled);
    }

    fn set_session_snapshot(&self, snapshot: SessionMenuSnapshot, update_enabled: bool) {
        let locale = self.locale();
        let status = *self.update_status.lock().unwrap();
        if let Err(error) = self
            .tray_menu
            .set_snapshot(snapshot, locale, status, update_enabled)
        {
            eprintln!("[harness] failed to update Status Bar sessions: {error}");
        }
    }
}

fn native_locale(app: &AppHandle) -> NativeLocale {
    app.try_state::<AppUpdateState>()
        .map(|state| state.ui.locale())
        .unwrap_or(NativeLocale::Zh)
}

#[tauri::command]
fn sync_dsh_preferences(app: AppHandle, theme: String, locale: String) -> Result<(), String> {
    let theme = match theme.as_str() {
        "light" => Some(Theme::Light),
        "dark" => Some(Theme::Dark),
        "system" => None,
        _ => return Err("unsupported DSH theme preference".to_string()),
    };
    let locale = NativeLocale::parse(&locale)
        .ok_or_else(|| "unsupported DSH locale preference".to_string())?;
    let should_check_for_updates = {
        let state = app
            .try_state::<AppUpdateState>()
            .ok_or_else(|| "native UI state is not ready".to_string())?;

        app.set_theme(theme);
        state
            .ui
            .set_locale(locale, !state.busy.load(Ordering::SeqCst));
        !state.automatic_check_started.swap(true, Ordering::SeqCst)
    };
    if should_check_for_updates {
        request_update_check(app, UpdateCheckSource::Automatic);
    }
    Ok(())
}

#[tauri::command]
fn sync_dsh_sessions(
    window: tauri::WebviewWindow,
    app: AppHandle,
    snapshot: SessionMenuSnapshot,
) -> Result<(), String> {
    if window.label() != "main" {
        return Err("session snapshots are accepted only from the main window".to_string());
    }
    if snapshot.revision == 0 {
        return Err("invalid DSH session snapshot revision".to_string());
    }
    validate_snapshot(&snapshot)?;
    let state = app
        .try_state::<AppUpdateState>()
        .ok_or_else(|| "native UI state is not ready".to_string())?;
    state
        .ui
        .set_session_snapshot(snapshot, !state.busy.load(Ordering::SeqCst));
    Ok(())
}

fn update_prompt(
    locale: NativeLocale,
    current_version: &str,
    new_version: &str,
    notes: Option<&str>,
) -> String {
    let notes = notes.map(str::trim).filter(|notes| !notes.is_empty());
    let notes = notes.map(|notes| {
        let mut chars = notes.chars();
        let truncated: String = chars.by_ref().take(UPDATE_NOTES_LIMIT).collect();
        if chars.next().is_some() {
            format!("{truncated}\n...")
        } else {
            truncated
        }
    });

    match (locale, notes) {
        (NativeLocale::Zh, Some(notes)) => format!(
            "发现 OpenHarness {new_version}（当前版本 {current_version}）。\n\n{notes}\n\n立即下载并安装吗？安装完成后应用会自动重启。"
        ),
        (NativeLocale::Zh, None) => format!(
            "发现 OpenHarness {new_version}（当前版本 {current_version}）。\n\n立即下载并安装吗？安装完成后应用会自动重启。"
        ),
        (NativeLocale::En, Some(notes)) => format!(
            "OpenHarness {new_version} is available (current version: {current_version}).\n\n{notes}\n\nDownload and install it now? The app will restart automatically after installation."
        ),
        (NativeLocale::En, None) => format!(
            "OpenHarness {new_version} is available (current version: {current_version}).\n\nDownload and install it now? The app will restart automatically after installation."
        ),
    }
}

fn download_percent(downloaded: u64, total: Option<u64>) -> Option<u8> {
    let total = total.filter(|total| *total > 0)?;
    Some(((downloaded.saturating_mul(100) / total).min(100)) as u8)
}

fn parse_build_number(value: &str) -> Result<Vec<u64>, String> {
    let parts: Vec<_> = value.split('.').collect();
    if parts.is_empty() || parts.len() > 3 {
        return Err(format!("invalid build number {value}"));
    }
    parts
        .into_iter()
        .map(|part| {
            if part.is_empty()
                || !part.bytes().all(|byte| byte.is_ascii_digit())
                || (part.len() > 1 && part.starts_with('0'))
            {
                return Err(format!("invalid build number {value}"));
            }
            part.parse::<u64>()
                .map_err(|error| format!("invalid build number {value}: {error}"))
        })
        .collect()
}

fn compare_build_numbers(left: &str, right: &str) -> Result<CompareOrdering, String> {
    let left = parse_build_number(left)?;
    let right = parse_build_number(right)?;
    let count = left.len().max(right.len());
    for index in 0..count {
        let ordering = left
            .get(index)
            .copied()
            .unwrap_or(0)
            .cmp(&right.get(index).copied().unwrap_or(0));
        if ordering != CompareOrdering::Equal {
            return Ok(ordering);
        }
    }
    Ok(CompareOrdering::Equal)
}

fn validated_build_number(value: Option<&str>) -> Result<String, String> {
    let value = value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "application bundle is missing a build number".to_string())?;
    let parts = parse_build_number(value)?;
    if parts.iter().all(|part| *part == 0) {
        return Err("application build number must be positive".to_string());
    }
    Ok(value.to_string())
}

#[cfg(target_os = "macos")]
fn installed_build_number(_app: &AppHandle) -> Result<String, String> {
    // CLI config overrides are not retained reliably in Tauri's runtime config;
    // the signed app bundle is the authoritative source for CFBundleVersion.
    let value = unsafe { openharness_current_build_number() };
    if value.is_null() {
        return Err("application bundle is missing CFBundleVersion".to_string());
    }
    let value = unsafe { CStr::from_ptr(value) }
        .to_str()
        .map_err(|error| format!("application CFBundleVersion is not valid UTF-8: {error}"))?;
    validated_build_number(Some(value))
}

#[cfg(not(target_os = "macos"))]
fn installed_build_number(app: &AppHandle) -> Result<String, String> {
    validated_build_number(
        option_env!("OPENHARNESS_BUILD_NUMBER").or(app
            .config()
            .bundle
            .macos
            .bundle_version
            .as_deref()),
    )
}

#[cfg(unix)]
fn terminate_backend_process(pid: u32) {
    unsafe {
        libc::kill(-(pid as i32), libc::SIGTERM);
    }
    std::thread::sleep(Duration::from_millis(250));
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
}

#[cfg(target_os = "windows")]
fn terminate_backend_process(pid: u32) {
    let _ = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn compare_core_versions(left: &Version, right: &Version) -> CompareOrdering {
    (left.major, left.minor, left.patch).cmp(&(right.major, right.minor, right.patch))
}

fn is_remote_update_newer(
    current_version: &Version,
    current_build: &str,
    remote_version: &str,
    remote_build: &str,
) -> Result<bool, String> {
    let remote_version = Version::parse(remote_version)
        .map_err(|error| format!("invalid update version {remote_version}: {error}"))?;
    let build_ordering = compare_build_numbers(remote_build, current_build)?;
    match compare_core_versions(&remote_version, current_version) {
        CompareOrdering::Greater => Ok(true),
        CompareOrdering::Less => Ok(false),
        CompareOrdering::Equal => Ok(build_ordering == CompareOrdering::Greater),
    }
}

fn manifest_build_number(manifest: &serde_json::Value) -> Result<&str, String> {
    manifest
        .get("build_number")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "update manifest is missing build_number".to_string())
}

fn version_label(locale: NativeLocale, version: &str, build_number: &str) -> String {
    match locale {
        NativeLocale::Zh => format!("{version}，构建 {build_number}"),
        NativeLocale::En => format!("{version}, build {build_number}"),
    }
}

fn up_to_date_message(locale: NativeLocale, version: &str, build_number: &str) -> String {
    let version = version_label(locale, version, build_number);
    match locale {
        NativeLocale::Zh => format!("当前已是最新版本（{version}）。"),
        NativeLocale::En => format!("OpenHarness {version} is up to date."),
    }
}

fn finish_update_operation(app: &AppHandle) {
    let state = app.state::<AppUpdateState>();
    state.busy.store(false, Ordering::SeqCst);
    state.ui.set_update_status(UpdateMenuStatus::Idle, true);
}

fn request_update_check(app: AppHandle, source: UpdateCheckSource) {
    let locale = native_locale(&app);
    if cfg!(debug_assertions) {
        if source == UpdateCheckSource::Manual {
            show_update_message(
                &app,
                locale.text(
                    "开发构建不会检查或安装正式更新。",
                    "Development builds do not check for or install release updates.",
                ),
                MessageDialogKind::Info,
            );
        }
        return;
    }

    let state = app.state::<AppUpdateState>();
    if state.busy.swap(true, Ordering::SeqCst) {
        if source == UpdateCheckSource::Manual {
            show_update_message(
                &app,
                locale.text(
                    "更新检查或安装正在进行中。",
                    "An update check or installation is already in progress.",
                ),
                MessageDialogKind::Info,
            );
        }
        return;
    }
    state
        .ui
        .set_update_status(UpdateMenuStatus::Checking, false);

    tauri::async_runtime::spawn(async move {
        let current_version = app.package_info().version.clone();
        let current_build_number = match installed_build_number(&app) {
            Ok(build_number) => build_number,
            Err(error) => {
                eprintln!("[updater] {error}");
                if source == UpdateCheckSource::Manual {
                    show_update_message(
                            &app,
                            locale.text(
                                "应用构建号配置无效，无法检查更新。",
                                "The application build number is invalid, so updates cannot be checked.",
                            ),
                            MessageDialogKind::Error,
                        );
                }
                finish_update_operation(&app);
                return;
            }
        };
        let comparison_version = current_version.clone();
        let check_result = match app
            .updater_builder()
            .version_comparator(move |_package_version, release| {
                compare_core_versions(&release.version, &comparison_version)
                    != CompareOrdering::Less
            })
            .build()
        {
            Ok(updater) => updater.check().await,
            Err(error) => Err(error),
        };

        let update = match check_result {
            Ok(Some(update)) => update,
            Ok(None) => {
                if source == UpdateCheckSource::Manual {
                    show_update_message(
                        &app,
                        up_to_date_message(
                            locale,
                            &current_version.to_string(),
                            &current_build_number,
                        ),
                        MessageDialogKind::Info,
                    );
                }
                finish_update_operation(&app);
                return;
            }
            Err(error) => {
                eprintln!("[updater] update check failed: {error}");
                if source == UpdateCheckSource::Manual {
                    show_update_message(
                        &app,
                        match locale {
                            NativeLocale::Zh => format!("检查更新失败：{error}"),
                            NativeLocale::En => format!("Failed to check for updates: {error}"),
                        },
                        MessageDialogKind::Error,
                    );
                }
                finish_update_operation(&app);
                return;
            }
        };

        let remote_build_number = match manifest_build_number(&update.raw_json) {
            Ok(build_number) => build_number,
            Err(error) => {
                eprintln!("[updater] {error}");
                if source == UpdateCheckSource::Manual {
                    show_update_message(
                        &app,
                        locale.text(
                            "更新清单缺少有效的构建号。",
                            "The update manifest does not contain a valid build number.",
                        ),
                        MessageDialogKind::Error,
                    );
                }
                finish_update_operation(&app);
                return;
            }
        };
        let is_newer = match is_remote_update_newer(
            &current_version,
            &current_build_number,
            &update.version,
            remote_build_number,
        ) {
            Ok(is_newer) => is_newer,
            Err(error) => {
                eprintln!("[updater] {error}");
                if source == UpdateCheckSource::Manual {
                    show_update_message(
                        &app,
                        locale.text(
                            "更新版本信息无效。",
                            "The update version information is invalid.",
                        ),
                        MessageDialogKind::Error,
                    );
                }
                finish_update_operation(&app);
                return;
            }
        };
        if !is_newer {
            if source == UpdateCheckSource::Manual {
                show_update_message(
                    &app,
                    up_to_date_message(locale, &current_version.to_string(), &current_build_number),
                    MessageDialogKind::Info,
                );
            }
            finish_update_operation(&app);
            return;
        }

        let prompt = update_prompt(
            locale,
            &version_label(locale, &current_version.to_string(), &current_build_number),
            &version_label(locale, &update.version, remote_build_number),
            update.body.as_deref(),
        );
        let dialog_app = app.clone();
        let accepted = tauri::async_runtime::spawn_blocking(move || {
            dialog_app
                .dialog()
                .message(prompt)
                .title(locale.text("OpenHarness 更新", "OpenHarness Update"))
                .kind(MessageDialogKind::Info)
                .buttons(MessageDialogButtons::OkCancelCustom(
                    locale.text("立即更新", "Update Now").to_string(),
                    locale.text("稍后", "Later").to_string(),
                ))
                .blocking_show()
        })
        .await
        .unwrap_or(false);

        if !accepted {
            finish_update_operation(&app);
            return;
        }

        let native_ui = app.state::<AppUpdateState>().ui.clone();
        native_ui.set_update_status(UpdateMenuStatus::Downloading(None), false);
        let mut downloaded = 0u64;
        let mut last_bucket = None;
        let install_result = update
            .download_and_install(
                |chunk_length, content_length| {
                    downloaded = downloaded.saturating_add(chunk_length as u64);
                    if let Some(percent) = download_percent(downloaded, content_length) {
                        let bucket = percent / 5;
                        if last_bucket != Some(bucket) {
                            last_bucket = Some(bucket);
                            native_ui.set_update_status(
                                UpdateMenuStatus::Downloading(Some(percent)),
                                false,
                            );
                        }
                    }
                },
                || {
                    native_ui.set_update_status(UpdateMenuStatus::Installing, false);
                },
            )
            .await;

        match install_result {
            Ok(()) => {
                native_ui.set_update_status(UpdateMenuStatus::Restarting, false);
                app.restart();
            }
            Err(error) => {
                eprintln!("[updater] update installation failed: {error}");
                show_update_message(
                    &app,
                    match locale {
                        NativeLocale::Zh => format!("更新安装失败：{error}"),
                        NativeLocale::En => format!("Failed to install the update: {error}"),
                    },
                    MessageDialogKind::Error,
                );
                finish_update_operation(&app);
            }
        }
    });
}

/// Resolve the URL the main window should load (the live backend, or the bundled
/// "starting" stub while the backend is not ready).
fn window_url(state: &Arc<BackendState>) -> WebviewUrl {
    match state.current_url() {
        Some(url) => WebviewUrl::External(url),
        None => WebviewUrl::App("index.html".into()),
    }
}

/// Create the single native window hosting the harness browser UI.
///
/// Theme is left at its default (`None`) so the window follows the system
/// dark/light appearance.
fn create_main_window(app: &tauri::AppHandle, state: &Arc<BackendState>) -> tauri::Result<()> {
    if app.get_webview_window("main").is_some() {
        show_main_window(app);
        return Ok(());
    }
    let url = window_url(state);
    WebviewWindowBuilder::new(app, "main", url)
        .title("OpenHarness")
        .inner_size(1280.0, 800.0)
        .min_inner_size(900.0, 600.0)
        // Wry's macOS acceptsFirstMouse override preserves the activation click.
        .accept_first_mouse(true)
        .initialization_script(WEBVIEW_INIT_SCRIPT)
        .build()?;
    Ok(())
}

/// Show (and focus) the primary window, used by tray / dock re-activation.
fn show_main_window(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

fn dispatch_harness_action(app: &AppHandle, action: &str, session_id: Option<&str>) {
    show_main_window(app);
    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    let detail = match session_id {
        Some(session_id) => serde_json::json!({ "type": action, "sessionId": session_id }),
        None => serde_json::json!({ "type": action }),
    };
    let script = format!(
        "window.dispatchEvent(new CustomEvent(\"openharness:native-action\", {{ detail: {} }}));",
        detail
    );
    if let Err(error) = window.eval(script) {
        eprintln!("[harness] failed to dispatch native action: {error}");
    }
}

/// Build the macOS application menu with the native About panel and update command.
fn build_app_menu(
    app: &AppHandle,
    locale: NativeLocale,
) -> tauri::Result<(Menu<Wry>, AppMenuItems)> {
    let package = app.package_info();
    let about = PredefinedMenuItem::about(
        app,
        Some(locale.text("关于 OpenHarness", "About OpenHarness")),
        None,
    )?;
    let update = MenuItem::with_id(
        app,
        APP_UPDATE_MENU_ID,
        update_menu_label(locale, UpdateMenuStatus::Idle),
        true,
        None::<&str>,
    )?;
    let services = PredefinedMenuItem::services(app, Some(locale.text("服务", "Services")))?;
    let hide = PredefinedMenuItem::hide(
        app,
        Some(locale.text("隐藏 OpenHarness", "Hide OpenHarness")),
    )?;
    let hide_others =
        PredefinedMenuItem::hide_others(app, Some(locale.text("隐藏其他", "Hide Others")))?;
    let quit = PredefinedMenuItem::quit(
        app,
        Some(locale.text("退出 OpenHarness", "Quit OpenHarness")),
    )?;
    let app_menu = Submenu::with_items(
        app,
        package.name.clone(),
        true,
        &[
            &about,
            &PredefinedMenuItem::separator(app)?,
            &update,
            &PredefinedMenuItem::separator(app)?,
            &services,
            &PredefinedMenuItem::separator(app)?,
            &hide,
            &hide_others,
            &PredefinedMenuItem::separator(app)?,
            &quit,
        ],
    )?;
    let menu = Menu::default(app)?;
    menu.remove_at(0)?
        .ok_or_else(|| std::io::Error::other("default application menu is missing"))?;
    menu.prepend(&app_menu)?;
    Ok((
        menu,
        AppMenuItems {
            about,
            update,
            services,
            hide,
            hide_others,
            quit,
        },
    ))
}

/// Build the menu-bar (tray) icon and its menu.
fn build_tray(app: &tauri::AppHandle, locale: NativeLocale) -> tauri::Result<TrayMenuItems> {
    let snapshot = Arc::new(Mutex::new(SessionMenuSnapshot::default()));
    let menu = build_tray_menu(
        app,
        locale,
        &snapshot.lock().unwrap(),
        UpdateMenuStatus::Idle,
        true,
    )?;
    let tray = TrayIconBuilder::new()
        .icon(app.default_window_icon().cloned().expect("app icon"))
        .tooltip("OpenHarness")
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(|app, event| {
            let id = event.id.as_ref();
            if let Some(session_id) = id.strip_prefix(SESSION_MENU_ID_PREFIX) {
                dispatch_harness_action(app, "open-session", Some(session_id));
                return;
            }
            match id {
                "show" | "show-all" => show_main_window(app),
                "new-session" => dispatch_harness_action(app, "new-session", None),
                "update" => request_update_check(app.clone(), UpdateCheckSource::Manual),
                "quit" => app.exit(0),
                _ => {}
            }
        })
        .build(app)?;
    Ok(TrayMenuItems { tray, snapshot })
}

fn main() {
    let initial_locale = preferred_native_locale();
    let app_menu_slot = Arc::new(Mutex::new(None::<AppMenuItems>));
    let menu_slot = app_menu_slot.clone();
    let setup_slot = app_menu_slot;

    // Record the log location before any runtime work so startup failures are
    // diagnosable from the installed app, which has no console on Windows.
    initialize_backend_log();

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            // A second launch was requested: re-focus the existing instance.
            show_main_window(app);
        }))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .invoke_handler(tauri::generate_handler![
            sync_dsh_preferences,
            sync_dsh_sessions
        ])
        .menu(move |app| {
            let (menu, items) = build_app_menu(app, initial_locale)?;
            *menu_slot.lock().unwrap() = Some(items);
            Ok(menu)
        })
        .on_menu_event(|app, event| {
            if event.id.as_ref() == APP_UPDATE_MENU_ID {
                request_update_check(app.clone(), UpdateCheckSource::Manual);
            }
        })
        .setup(move |app| {
            install_native_context_menu_filter();

            let resource_dir = app.path().resource_dir()?;
            let home = app.path().home_dir()?;
            finalize_backend_log_directory(app.path().app_local_data_dir().ok());
            let state = Arc::new(BackendState::new());
            app.manage(state.clone());

            let app_menu = setup_slot
                .lock()
                .unwrap()
                .take()
                .ok_or_else(|| std::io::Error::other("application menu items are missing"))?;
            let tray_menu = build_tray(app.handle(), initial_locale)?;
            let ui = NativeUi {
                locale: Arc::new(Mutex::new(initial_locale)),
                update_status: Arc::new(Mutex::new(UpdateMenuStatus::Idle)),
                app_menu,
                tray_menu,
            };
            ui.set_locale(initial_locale, true);
            app.manage(AppUpdateState {
                busy: AtomicBool::new(false),
                automatic_check_started: AtomicBool::new(false),
                ui,
            });

            // Supervise the backend in a detached background thread. The
            // shared quit flag and child PID own its process lifecycle.
            std::thread::spawn({
                let state = state.clone();
                let app = app.handle().clone();
                let resource_dir = resource_dir.clone();
                let home = home.clone();
                move || {
                    // Finder launches lack the login PATH and shell-configured
                    // API credentials. Keep this bounded work off the UI thread.
                    let shell_env = load_shell_env_or_empty();
                    backend_supervisor(state, resource_dir, home, shell_env, app);
                }
            });

            // Keep the native UI responsive while DSH starts. The supervisor
            // navigates this window after it validates the loopback URL.
            create_main_window(app.handle(), &state)?;

            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building OpenHarness app")
        .run(|app_handle, event| match event {
            RunEvent::WindowEvent {
                label,
                event: tauri::WindowEvent::CloseRequested { api, .. },
                ..
            } => {
                // Close-to-tray: hide instead of destroying the window, so the
                // backend and its session stay alive in the system tray.
                api.prevent_close();
                if let Some(window) = app_handle.get_webview_window(&label) {
                    let _ = window.hide();
                }
            }
            RunEvent::Exit => {
                let state = app_handle.state::<Arc<BackendState>>();
                state.quitting.store(true, Ordering::SeqCst);
                let pid = state.child_pid.lock().unwrap().take();
                if let Some(pid) = pid {
                    terminate_backend_process(pid);
                }
            }
            // macOS: clicking the Dock icon with no visible windows reopens.
            #[cfg(target_os = "macos")]
            RunEvent::Reopen { .. } => show_main_window(app_handle),
            _ => {}
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_browser_authentication_without_logging_launch_tokens() {
        let line = "dsh web: http://127.0.0.1:3080/?token=test-secret";
        assert_eq!(parse_url(line).unwrap().query(), Some("token=test-secret"));
        assert!(!runtime_log_line(line).contains("test-secret"));
        assert_eq!(runtime_log_line("plugin activated"), "plugin activated");
    }

    fn menu_session(
        id: &str,
        updated_at: u64,
        pending_interaction: Option<&str>,
        running: bool,
        completed: bool,
    ) -> SessionMenuEntry {
        SessionMenuEntry {
            id: id.to_string(),
            title: id.to_string(),
            workspace: None,
            updated_at,
            running,
            completed,
            pending_interaction: pending_interaction.map(str::to_string),
        }
    }

    #[test]
    fn status_bar_orders_the_five_task_categories() {
        let sessions = vec![
            menu_session("idle", 500, None, false, false),
            menu_session("completed", 400, None, false, true),
            menu_session("running", 300, None, true, false),
            menu_session("question", 200, Some("question"), false, false),
            menu_session("plan-review", 250, Some("plan-review"), false, false),
            menu_session("approval", 100, Some("approval"), false, false),
        ];

        let (top, _) = select_sessions(&sessions);
        let ids = top
            .iter()
            .map(|session| session.id.as_str())
            .collect::<Vec<_>>();

        assert_eq!(
            ids,
            [
                "approval",
                "plan-review",
                "question",
                "running",
                "completed"
            ]
        );
    }

    #[test]
    fn status_bar_limits_top_and_history_lists() {
        let sessions = (0..30)
            .map(|index| menu_session(&format!("session-{index}"), index, None, false, false))
            .collect::<Vec<_>>();

        let (top, history) = select_sessions(&sessions);

        assert_eq!(top.len(), TOP_SESSION_LIMIT);
        assert_eq!(history.len(), HISTORY_SESSION_LIMIT);
        assert_eq!(top[0].id, "session-29");
        assert_eq!(top[4].id, "session-25");
        assert_eq!(history[0].id, "session-24");
        assert_eq!(history[19].id, "session-5");
    }

    #[test]
    fn idle_session_label_has_no_false_completion_state() {
        let idle = menu_session("Resumable", 1, None, false, false);
        let completed = menu_session("Finished", 2, None, false, true);

        assert_eq!(session_menu_label(NativeLocale::Zh, &idle), "Resumable");
        assert!(session_menu_label(NativeLocale::Zh, &completed).starts_with("[完成待查看] "));
    }

    #[test]
    fn session_menu_label_normalizes_controls_and_caps_length() {
        let mut session = menu_session("session", 1, None, false, false);
        session.title = format!("Long\0\n  title {}", "x".repeat(120));

        let label = session_menu_label(NativeLocale::En, &session);

        assert!(label.starts_with("Long title "));
        assert!(label.ends_with("..."));
        assert!(label.chars().count() <= SESSION_MENU_LABEL_LIMIT);
        assert!(!label.chars().any(char::is_control));
    }

    #[test]
    fn session_snapshot_rejects_duplicate_or_unknown_state() {
        let duplicate = menu_session("same", 1, None, false, false);
        let snapshot = SessionMenuSnapshot {
            revision: 1,
            ready: true,
            sessions: vec![duplicate.clone(), duplicate],
        };
        assert!(validate_snapshot(&snapshot).is_err());

        let snapshot = SessionMenuSnapshot {
            revision: 1,
            ready: true,
            sessions: vec![menu_session("bad", 1, Some("unknown"), false, false)],
        };
        assert!(validate_snapshot(&snapshot).is_err());
    }

    #[test]
    fn shell_env_parser_imports_only_path_and_deepseek_variables() {
        let output = b"shell banner without newline\0PATH=/opt/homebrew/bin:/usr/bin\0\
DEEPSEEK_API_KEY=secret\0\
DEEPSEEK_BASE_URL=https://example.invalid\0\
DEEPSEEK_EMPTY=\0\
HOME=/Users/example\0\
DYLD_INSERT_LIBRARIES=/tmp/injected.dylib\0\
INVALID-KEY=value\0";

        assert_eq!(
            parse_shell_env(output),
            vec![
                ("PATH".to_string(), "/opt/homebrew/bin:/usr/bin".to_string()),
                ("DEEPSEEK_API_KEY".to_string(), "secret".to_string()),
                (
                    "DEEPSEEK_BASE_URL".to_string(),
                    "https://example.invalid".to_string()
                ),
            ]
        );
        assert!(parse_shell_env(b"PATH=/unmarked/bin\0").is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn login_shell_uses_configured_absolute_file_or_portable_fallback() {
        assert_eq!(
            resolve_login_shell(Some(OsStr::new("/bin/sh"))),
            PathBuf::from("/bin/sh")
        );
        assert_eq!(resolve_login_shell(None), PathBuf::from("/bin/sh"));
        assert_eq!(
            resolve_login_shell(Some(OsStr::new("relative-shell"))),
            PathBuf::from("/bin/sh")
        );
        assert_eq!(
            resolve_login_shell(Some(OsStr::new("/definitely/missing/shell"))),
            PathBuf::from("/bin/sh")
        );
    }

    #[test]
    fn managed_runtime_path_precedes_user_paths_and_deduplicates() {
        let runtime = RuntimePaths {
            root: PathBuf::from("/app/runtime"),
            node: PathBuf::from("/app/runtime/node"),
            dsh_entry: PathBuf::from("/app/runtime/dsh/bin.js"),
            patch: PathBuf::from("/app/runtime/dsh/openharness.patch.yml"),
            find_plugin_patch: PathBuf::from("/app/runtime/dsh/openharness-find.patch.yml"),
            package_manager_bin: PathBuf::from("/app/runtime/dsh/openharness-bin"),
        };
        let shell_env = vec![(
            "PATH".to_string(),
            std::env::join_paths(["/usr/bin", "/app/runtime"])
                .unwrap()
                .to_string_lossy()
                .into_owned(),
        )];
        let inherited = std::env::join_paths(["/bin", "/usr/bin"]).unwrap();

        let paths = std::env::split_paths(&managed_runtime_path(
            &runtime,
            &shell_env,
            Some(&inherited),
        ))
        .collect::<Vec<_>>();

        assert_eq!(paths[0], runtime.package_manager_bin);
        assert_eq!(paths[1], runtime.root);
        assert_eq!(
            paths
                .iter()
                .filter(|path| *path == Path::new("/usr/bin"))
                .count(),
            1
        );
    }

    #[test]
    fn harness_command_keeps_web_ui_inside_desktop_app() {
        let runtime = RuntimePaths {
            root: PathBuf::from("/app/runtime"),
            node: PathBuf::from("/app/runtime/node"),
            dsh_entry: PathBuf::from("/app/runtime/dsh/bin.js"),
            patch: PathBuf::from("/app/runtime/dsh/openharness.patch.yml"),
            find_plugin_patch: PathBuf::from("/app/runtime/dsh/openharness-find.patch.yml"),
            package_manager_bin: PathBuf::from("/app/runtime/dsh/openharness-bin"),
        };
        let mut command = Command::new(&runtime.node);

        configure_harness_args(&mut command, &runtime, false);

        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            args,
            vec![
                "/app/runtime/dsh/bin.js",
                "--profile",
                "web",
                "--patch",
                "/app/runtime/dsh/openharness.patch.yml",
                "--patch",
                "/app/runtime/dsh/openharness-find.patch.yml",
                "--host",
                "127.0.0.1",
                "--port",
                "0",
                "--no-open",
            ]
        );
    }

    #[test]
    fn recognizes_only_the_managed_restart_exit_code() {
        assert!(is_managed_restart_code(Some(MANAGED_RESTART_EXIT_CODE)));
        assert!(!is_managed_restart_code(Some(0)));
        assert!(!is_managed_restart_code(None));
    }

    #[test]
    fn detects_find_plugin_in_an_existing_profile_bundle_list() {
        let manifest = serde_json::json!({
            "dependencies": {
                FIND_PLUGIN_PACKAGE: "github:MicroSpotlight/openharness-find-plugin"
            },
            "dsh": {
                "profile": {
                    "bundles": ["@deepseek-ai/dsh-base", FIND_PLUGIN_PACKAGE]
                }
            }
        });

        assert!(manifest_declares_bundle(&manifest, FIND_PLUGIN_PACKAGE));
        assert!(!manifest_declares_bundle(
            &manifest,
            "@openharness/native-bridge"
        ));
        assert!(!manifest_declares_bundle(
            &serde_json::json!({ "dependencies": { FIND_PLUGIN_PACKAGE: "0.1.0" } }),
            FIND_PLUGIN_PACKAGE
        ));
    }

    #[test]
    fn bounded_reader_rejects_oversized_shell_output() {
        let error = read_bounded(std::io::Cursor::new(b"12345"), 4).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    #[cfg(unix)]
    fn shell_environment_command_honors_timeout() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "sleep 30 & wait"]);
        let started_at = Instant::now();
        let error = command_stdout_with_timeout(
            &mut command,
            Duration::from_millis(30),
            SHELL_ENV_OUTPUT_LIMIT,
        )
        .unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert!(started_at.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn stream_reader_survives_non_utf8_output_before_the_url() {
        let mut bytes = Vec::new();
        // A GBK-encoded diagnostic from a Windows console code page, followed by
        // the ASCII boot line the supervisor needs.
        bytes.extend_from_slice(&[0xd6, 0xd0, 0xce, 0xc4, 0x0a]);
        bytes.extend_from_slice(b"dsh web: http://127.0.0.1:3080\n");
        let mut lines = Vec::new();

        read_stream_lines(std::io::Cursor::new(bytes), |line| lines.push(line));

        assert_eq!(lines.len(), 2);
        assert_eq!(
            parse_url(&lines[1]).unwrap().as_str(),
            "http://127.0.0.1:3080/"
        );
    }

    #[test]
    fn stream_reader_keeps_a_final_unterminated_line() {
        let mut lines = Vec::new();

        read_stream_lines(
            std::io::Cursor::new(b"dsh web: http://127.0.0.1:9"),
            |line| lines.push(line),
        );

        assert_eq!(lines, vec!["dsh web: http://127.0.0.1:9".to_owned()]);
    }

    #[test]
    #[cfg(unix)]
    fn stream_reader_forwards_output_while_the_child_stays_alive() {
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "printf '\\326\\320\\316\\304\\n'; printf 'dsh web: http://127.0.0.1:3080\\n'; sleep 5"])
            .stdout(Stdio::piped());
        let mut child = command.spawn().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (output_tx, output_rx) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            forward_backend_stream(
                stdout,
                "[harness] ",
                Arc::new(Mutex::new(Vec::new())),
                Some(output_tx),
            )
        });

        let url = wait_for_startup_url(&output_rx, Duration::from_secs(10)).unwrap();

        assert_eq!(url.as_str(), "http://127.0.0.1:3080/");
        terminate_command(&mut child);
        reader.join().unwrap();
    }

    #[test]
    fn failure_message_carries_output_tail_and_log_location() {
        let recent = Mutex::new(vec![
            "first".to_owned(),
            "second".to_owned(),
            "third".to_owned(),
        ]);

        let message = backend_failure_message(
            "dsh web exited before reporting its URL",
            &recent,
            Some(Path::new("/tmp/backend.log")),
        );

        assert!(message.starts_with("dsh web exited before reporting its URL"));
        assert!(message.contains("second"));
        assert!(message.contains("third"));
        assert!(message.contains("/tmp/backend.log"));
    }

    #[test]
    fn failure_message_truncates_very_long_output_lines() {
        let recent = Mutex::new(vec!["x".repeat(ERROR_DIALOG_LINE_LIMIT + 50)]);

        let message = backend_failure_message("failed", &recent, None);

        assert!(message.ends_with('…'));
        assert!(message.chars().count() < ERROR_DIALOG_LINE_LIMIT + 64);
    }

    #[test]
    fn oversized_backend_logs_rotate_before_growing() {
        let directory =
            std::env::temp_dir().join(format!("openharness-log-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("backend.log");
        fs::write(&path, vec![b'x'; (BACKEND_LOG_LIMIT + 1) as usize]).unwrap();

        rotate_backend_log(&path);

        assert!(!path.exists());
        assert!(directory.join("backend.log.1").exists());
        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn startup_failures_reach_the_backend_log_and_the_dialog() {
        let directory =
            std::env::temp_dir().join(format!("openharness-startup-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        install_backend_log(directory.clone());
        let log_path = backend_log_path().expect("the log path was just installed");
        assert!(log_path.starts_with(&directory));

        // A backend that dies on its first read is the startup failure users hit:
        // nothing on stdout, an error on stderr, and no URL.
        let mut command = Command::new("node");
        command
            .args(["-e", "process.stderr.write('boom\\n'); process.exit(3)"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().unwrap();
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let recent = Arc::new(Mutex::new(Vec::new()));
        let (output_tx, output_rx) = mpsc::channel();
        let readers = [
            std::thread::spawn({
                let recent = Arc::clone(&recent);
                move || forward_backend_stream(stderr, "[harness:stderr] ", recent, None)
            }),
            std::thread::spawn({
                let recent = Arc::clone(&recent);
                move || {
                    forward_backend_stream(stdout, "[harness] ", recent, Some(output_tx));
                }
            }),
        ];

        let error = wait_for_startup_url(&output_rx, Duration::from_secs(20)).unwrap_err();

        for reader in readers {
            reader.join().unwrap();
        }
        child.wait().unwrap();
        let logged = fs::read_to_string(&log_path).unwrap();
        assert!(logged.contains("[harness:stderr] boom"), "{logged}");
        let message = backend_failure_message(&error.to_string(), &recent, Some(&log_path));
        assert!(message.contains("boom"), "{message}");
        assert!(
            message.contains(&log_path.display().to_string()),
            "{message}"
        );
        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn log_directory_prefers_the_platform_state_directory() {
        assert_eq!(
            resolve_log_directory(
                Some(OsString::from("/Local")),
                Some(OsString::from("/state")),
                Some(OsString::from("/home/user"))
            ),
            PathBuf::from("/Local/OpenHarness/logs")
        );
        assert_eq!(
            resolve_log_directory(None, Some(OsString::from("/state")), None),
            PathBuf::from("/state/OpenHarness/logs")
        );
        assert_eq!(
            resolve_log_directory(None, None, Some(OsString::from("/home/user"))),
            PathBuf::from("/home/user/.local/state/OpenHarness/logs")
        );
        assert!(resolve_log_directory(None, None, None).ends_with("OpenHarness/logs"));
    }

    #[test]
    fn parses_supported_loopback_urls() {
        assert_eq!(
            parse_url("dsh web: http://127.0.0.1:3080")
                .unwrap()
                .as_str(),
            "http://127.0.0.1:3080/"
        );
        assert_eq!(
            parse_url("dsh web: http://[::1]:3080").unwrap().as_str(),
            "http://[::1]:3080/"
        );
        assert_eq!(
            parse_url("[harness] dsh web: http://localhost:3080")
                .unwrap()
                .as_str(),
            "http://localhost:3080/"
        );
    }

    #[test]
    fn rejects_non_loopback_or_unsafe_urls() {
        assert!(parse_url("dsh web: https://127.0.0.1:3080").is_none());
        assert!(parse_url("dsh web: http://192.168.1.2:3080").is_none());
        assert!(parse_url("dsh web: http://example.com:3080").is_none());
        assert!(parse_url("dsh web: http://user@127.0.0.1:3080").is_none());
        assert!(parse_url("dsh web: http://127.0.0.1").is_none());
        assert!(parse_url("server ready").is_none());
    }

    #[test]
    fn startup_wait_honors_timeout_without_output() {
        let (_sender, receiver) = mpsc::channel();
        let error = wait_for_startup_url(&receiver, Duration::from_millis(20)).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    }

    #[test]
    fn failure_streak_resets_only_after_stable_uptime() {
        assert_eq!(next_failure_count(2, None), 3);
        assert_eq!(next_failure_count(2, Some(Duration::from_secs(29))), 3);
        assert_eq!(next_failure_count(2, Some(STABLE_BACKEND_UPTIME)), 1);
        assert_eq!(next_failure_count(u32::MAX, None), u32::MAX);
    }

    #[test]
    fn restart_delay_is_exponential_and_capped() {
        assert_eq!(restart_delay(1), Duration::from_secs(2));
        assert_eq!(restart_delay(3), Duration::from_secs(8));
        assert_eq!(restart_delay(6), Duration::from_secs(30));
        assert_eq!(restart_delay(u32::MAX), Duration::from_secs(30));
    }

    #[test]
    fn backend_state_is_cleared_while_restarting() {
        let state = BackendState::new();
        *state.child_pid.lock().unwrap() = Some(42);
        state.set_url(Url::parse("http://127.0.0.1:3080").unwrap());
        assert_eq!(*state.child_pid.lock().unwrap(), Some(42));
        assert!(state.current_url().is_some());

        state.mark_stopped();

        assert_eq!(*state.child_pid.lock().unwrap(), None);
        assert!(state.current_url().is_none());
    }

    #[test]
    fn parses_only_supported_dsh_locales() {
        assert!(matches!(NativeLocale::parse("zh"), Some(NativeLocale::Zh)));
        assert!(matches!(NativeLocale::parse("en"), Some(NativeLocale::En)));
        assert!(NativeLocale::parse("zh-CN").is_none());
        assert!(NativeLocale::parse("fr").is_none());
    }

    #[test]
    fn localizes_native_update_menu_status() {
        assert_eq!(
            update_menu_label(NativeLocale::Zh, UpdateMenuStatus::Idle),
            "检查更新..."
        );
        assert_eq!(
            update_menu_label(NativeLocale::En, UpdateMenuStatus::Downloading(Some(42))),
            "Downloading Update... 42%"
        );
    }

    #[test]
    fn update_prompt_includes_versions_and_truncates_long_notes() {
        let notes = "a".repeat(UPDATE_NOTES_LIMIT + 1);
        let prompt = update_prompt(
            NativeLocale::Zh,
            "0.1.0，构建 41",
            "0.1.0，构建 42",
            Some(&notes),
        );

        assert!(prompt.contains("构建 41"));
        assert!(prompt.contains("构建 42"));
        assert!(prompt.contains("\n...\n"));
        assert!(!prompt.contains(&notes));
    }

    #[test]
    fn download_percent_handles_unknown_empty_and_overrun_sizes() {
        assert_eq!(download_percent(50, Some(100)), Some(50));
        assert_eq!(download_percent(120, Some(100)), Some(100));
        assert_eq!(download_percent(50, Some(0)), None);
        assert_eq!(download_percent(50, None), None);
    }

    #[test]
    fn update_versions_compare_version_before_build_number() {
        let current_version = Version::parse("0.1.0").unwrap();

        assert!(is_remote_update_newer(&current_version, "41", "0.1.0-beta.2", "42").unwrap());
        assert!(is_remote_update_newer(&current_version, "42", "0.1.0-alpha.9", "43").unwrap());
        assert!(!is_remote_update_newer(&current_version, "42", "0.1.0-beta.3", "42").unwrap());
        assert!(is_remote_update_newer(&current_version, "99", "0.1.1-beta.0", "1").unwrap());
        assert!(!is_remote_update_newer(&current_version, "1", "0.0.9-beta.99", "999").unwrap());
        assert_eq!(
            compare_core_versions(
                &Version::parse("0.1.0-alpha.1").unwrap(),
                &Version::parse("0.1.0-beta.99").unwrap()
            ),
            CompareOrdering::Equal
        );
        assert_eq!(
            compare_build_numbers("1.10", "1.2").unwrap(),
            CompareOrdering::Greater
        );
        assert_eq!(
            compare_build_numbers("1", "1.0").unwrap(),
            CompareOrdering::Equal
        );
    }

    #[test]
    fn requires_a_positive_bundle_build_number() {
        assert_eq!(validated_build_number(Some("7")).unwrap(), "7");
        assert_eq!(validated_build_number(Some(" 7 ")).unwrap(), "7");
        assert!(validated_build_number(None).is_err());
        assert!(validated_build_number(Some("0")).is_err());
        assert!(validated_build_number(Some("01")).is_err());
        assert!(validated_build_number(Some("1.2.3.4")).is_err());
    }
}
