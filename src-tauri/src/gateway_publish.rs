//! Versioned gateway publishing for Windows packaged installs.
//!
//! Client MCP configs point at `%APPDATA%\Roaming\Toolport\bin\toolport-gateway-{version}.exe`
//! (legacy leaf `Conduit` is still accepted until launch migrates it) instead of the
//! install-dir copy NSIS must overwrite on update. Publishing copies the bundled gateway
//! to a new versioned filename (never fighting a lock on the old file), records the path
//! in `gateway-manifest.json`, and lets `repoint_stale_gateways` migrate client configs.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const MANIFEST_FILE: &str = "gateway-manifest.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GatewayManifest {
    pub version: String,
    pub path: String,
    pub size: u64,
}

/// True on Windows packaged builds (not `cargo run` from `target/`).
pub fn should_publish_client_gateway() -> bool {
    #[cfg(windows)]
    {
        if let Ok(exe) = std::env::current_exe() {
            let lower = exe.to_string_lossy().to_ascii_lowercase();
            return !lower.contains("\\target\\");
        }
    }
    #[cfg(not(windows))]
    let _ = ();
    false
}

fn gateway_bin_dir() -> Option<PathBuf> {
    Some(crate::registry::conduit_dir()?.join("bin"))
}

fn manifest_path() -> Option<PathBuf> {
    Some(gateway_bin_dir()?.join(MANIFEST_FILE))
}

fn versioned_dest(version: &str) -> Option<PathBuf> {
    let ext = std::env::consts::EXE_SUFFIX;
    Some(
        gateway_bin_dir()?.join(format!("toolport-gateway-{version}{ext}")),
    )
}

/// Gateway binary bundled next to the running app (install dir).
pub fn bundled_gateway_source() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    let ext = std::env::consts::EXE_SUFFIX;
    let version = env!("CARGO_PKG_VERSION");

    let versioned = dir.join(format!("toolport-gateway-{version}{ext}"));
    if versioned.is_file() {
        return Some(versioned);
    }

    let plain = dir.join(format!("toolport-gateway{ext}"));
    if plain.is_file() {
        return Some(plain);
    }

    let legacy = dir.join(format!("conduit-gateway{ext}"));
    if legacy.is_file() {
        return Some(legacy);
    }

    if let Some(triple) = option_env!("CONDUIT_TARGET_TRIPLE").filter(|t| !t.is_empty()) {
        for name in ["toolport-gateway", "conduit-gateway"] {
            let suffixed = dir.join(format!("{name}-{triple}{ext}"));
            if suffixed.is_file() {
                return Some(suffixed);
            }
        }
    }

    None
}

fn file_size(path: &Path) -> Option<u64> {
    std::fs::metadata(path).ok().map(|m| m.len())
}

/// Copy the install-dir gateway into `Toolport/bin` when needed and write the manifest.
pub fn publish_bundled_gateway() -> Option<PathBuf> {
    if !should_publish_client_gateway() {
        return None;
    }
    let src = bundled_gateway_source()?;
    let version = env!("CARGO_PKG_VERSION").to_string();
    let dest = versioned_dest(&version)?;
    let src_size = file_size(&src)?;

    let needs_copy = match file_size(&dest) {
        Some(dest_size) => dest_size != src_size,
        None => true,
    };
    if needs_copy {
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).ok()?;
        }
        std::fs::copy(&src, &dest).ok()?;
    }

    let manifest = GatewayManifest {
        version: version.clone(),
        path: dest.to_string_lossy().into_owned(),
        size: file_size(&dest).unwrap_or(src_size),
    };
    if let Some(path) = manifest_path() {
        if let Ok(json) = serde_json::to_string_pretty(&manifest) {
            let _ = crate::registry::atomic_write(&path, &json);
        }
    }

    Some(dest)
}

/// Published client gateway path from the manifest, when it matches this build.
pub fn published_gateway_path() -> Option<PathBuf> {
    if !should_publish_client_gateway() {
        return None;
    }
    let path = manifest_path()?;
    let raw = std::fs::read_to_string(&path).ok()?;
    let manifest: GatewayManifest = serde_json::from_str(&raw).ok()?;
    if manifest.version != env!("CARGO_PKG_VERSION") {
        return None;
    }
    let p = PathBuf::from(&manifest.path);
    if p.is_file() {
        Some(p)
    } else {
        None
    }
}

/// Resolve the path MCP clients should spawn: publish if needed, else read manifest.
pub fn client_gateway_path() -> Option<PathBuf> {
    if !should_publish_client_gateway() {
        return None;
    }
    if let Some(p) = published_gateway_path() {
        return Some(p);
    }
    publish_bundled_gateway()
}

// ---------------------------------------------------------------------------
// Cross-platform gateway process reaper (SOU-414 / residual of SOU-306)
//
// Product rule: same outcome on Windows, macOS, and Linux. Staleness is decided
// by process *image identity* (full path + basename rules), not by Windows
// versioned filenames alone. macOS/Linux clients spawn unversioned
// `toolport-gateway`, so a basename-only reaper is a permanent no-op there.
//
// Two modes:
//   * stop_stale_gateways — every launch; keep current/resolved paths, kill obsolete
//   * stop_spawned_gateways — in-app updater; kill every Toolport gateway image so
//     the installer can replace locked files
//
// Parent agent apps (Cursor, Claude, …) are never touched. Clients that auto-respawn
// MCP on a dead stdio pipe pick up the repointed binary on the next tool call.
// ---------------------------------------------------------------------------

/// A running process that looks like a Toolport/Conduit gateway.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatewayProcess {
    pub pid: u32,
    /// Best-effort absolute path of the executable. Missing when the OS denied
    /// query access; decision falls back to basename rules only.
    pub path: Option<PathBuf>,
    /// Process image basename, e.g. `toolport-gateway-1.9.4.exe` or `toolport-gateway`.
    pub basename: String,
}

/// Inputs for the pure keep/kill decision. Built at the call site so tests do not
/// need a live process table or a real install layout.
#[derive(Debug, Clone)]
pub struct ReapContext {
    pub current_version: String,
    /// Executable paths that must survive a *stale* reap (current published binary,
    /// nested macOS helper, AppImage stable copy, etc.). Compared case-insensitively
    /// with normalized separators.
    pub keep_paths: Vec<PathBuf>,
    /// When true (updater), every gateway process is killed, including current.
    pub kill_all: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReapDecision {
    Keep,
    Kill,
}

/// Result of a reaper pass, for logging and the Tauri command surface.
#[derive(Debug, Clone, Default)]
pub struct ReapReport {
    pub killed: Vec<String>,
    pub kept: Vec<String>,
    pub failed: Vec<String>,
}

impl ReapReport {
    pub fn killed_labels(&self) -> Vec<String> {
        self.killed.clone()
    }
}

/// Build keep-paths known to this crate without calling into `clients` (avoids a
/// module cycle). Callers that can resolve the nested macOS helper should pass
/// extras via [`stop_stale_gateways_with_keep`].
fn default_keep_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let push = |paths: &mut Vec<PathBuf>, p: PathBuf| {
        if !paths.iter().any(|e| paths_equal(e, &p)) {
            paths.push(p);
        }
    };
    if let Some(p) = client_gateway_path() {
        push(&mut paths, p);
    }
    if let Some(p) = published_gateway_path() {
        push(&mut paths, p);
    }
    if let Some(p) = bundled_gateway_source() {
        push(&mut paths, p);
    }
    if let Some(p) = versioned_dest(env!("CARGO_PKG_VERSION")) {
        push(&mut paths, p);
    }
    if let Some(dir) = gateway_bin_dir() {
        let ext = std::env::consts::EXE_SUFFIX;
        push(
            &mut paths,
            dir.join(format!("toolport-gateway{ext}")),
        );
        push(
            &mut paths,
            dir.join(format!("conduit-gateway{ext}")),
        );
    }
    paths
}

fn paths_equal(a: &Path, b: &Path) -> bool {
    // Resolve symlinks when the files exist so macOS helper vs Contents/MacOS
    // symlink to the same binary both match a keep path.
    let ca = std::fs::canonicalize(a).unwrap_or_else(|_| a.to_path_buf());
    let cb = std::fs::canonicalize(b).unwrap_or_else(|_| b.to_path_buf());
    normalize_path(&ca) == normalize_path(&cb)
}

/// Normalize for comparison only. Always uses `\` so callers check one form.
fn normalize_path(p: &Path) -> String {
    p.to_string_lossy()
        .replace('/', "\\")
        .trim_end_matches(['\\', '/'])
        .to_ascii_lowercase()
}

/// Basename matches a Toolport/Conduit gateway image (versioned or not).
pub fn is_gateway_basename(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    let stem = lower.strip_suffix(".exe").unwrap_or(&lower);
    stem == "toolport-gateway"
        || stem == "conduit-gateway"
        || stem.starts_with("toolport-gateway-")
        || stem.starts_with("conduit-gateway-")
}

/// True only for names like `toolport-gateway-1.9.4`, not `toolport-gateway-shim`.
fn is_versioned_gateway_basename(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    let stem = lower.strip_suffix(".exe").unwrap_or(&lower);
    let rest = if let Some(r) = stem.strip_prefix("toolport-gateway-") {
        r
    } else if let Some(r) = stem.strip_prefix("conduit-gateway-") {
        r
    } else {
        return false;
    };
    looks_like_version_suffix(rest)
}

/// Version suffix: starts with a digit, contains `.`, and only version-ish chars
/// (e.g. `1.9.4`, `1.9.4-beta.1`). Rejects `shim`, `helper`, target triples alone.
fn looks_like_version_suffix(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_digit() || !s.contains('.') {
        return false;
    }
    s.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+' | '_'))
}

fn basename_matches_current_version(name: &str, version: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    let stem = lower.strip_suffix(".exe").unwrap_or(&lower);
    let want_tp = format!("toolport-gateway-{version}").to_ascii_lowercase();
    let want_cd = format!("conduit-gateway-{version}").to_ascii_lowercase();
    stem == want_tp || stem == want_cd
}

/// Path looks like a cargo `target/...` dev binary — keep under stale mode so
/// `tauri dev` is not murdered by a packaged Toolport on the same machine.
fn is_dev_target_path(path: &Path) -> bool {
    let n = normalize_path(path);
    n.contains("\\target\\debug\\") || n.contains("\\target\\release\\")
}

/// Path is under a Toolport/Conduit product location (data dir bin, install leaf,
/// macOS helper bundle, AppImage stable copy). Used to avoid killing a foreign
/// binary that merely starts with a similar name.
fn path_looks_like_our_install(path: &Path) -> bool {
    let n = normalize_path(path);
    n.contains("\\toolport\\")
        || n.contains("\\conduit\\")
        || n.contains("toolportgateway.app")
        || n.contains("conduitgateway.app")
        || n.contains("toolport.app")
        || n.contains("conduit.app")
}

/// Pure keep/kill decision. Getting this wrong is expensive both ways: too eager
/// kills the bridge we just started; too shy leaves users on old gateway code.
pub fn decide_reap(proc: &GatewayProcess, ctx: &ReapContext) -> ReapDecision {
    if !is_gateway_basename(&proc.basename) {
        return ReapDecision::Keep;
    }
    if ctx.kill_all {
        return ReapDecision::Kill;
    }

    if let Some(ref path) = proc.path {
        if ctx.keep_paths.iter().any(|k| paths_equal(k, path)) {
            return ReapDecision::Keep;
        }
        if is_dev_target_path(path) {
            return ReapDecision::Keep;
        }
        if basename_matches_current_version(&proc.basename, &ctx.current_version) {
            // Same version, different install path: still our build; keep unless
            // we have keep_paths that explicitly name a different current (handled above).
            return ReapDecision::Keep;
        }
        if is_versioned_gateway_basename(&proc.basename) {
            // Only kill versioned images under our product paths (not toolport-gateway-shim
            // in /opt/other-vendor, and not non-version suffixes that snuck through).
            if path_looks_like_our_install(path) {
                return ReapDecision::Kill;
            }
            return ReapDecision::Keep;
        }
        // Unversioned basename (macOS/Linux/AppImage/dev packaged): path is the
        // identity. Not equal to keep_paths and looks like ours → obsolete copy.
        if path_looks_like_our_install(path) {
            if ctx.keep_paths.is_empty() {
                // No known current path — refuse to guess.
                return ReapDecision::Keep;
            }
            return ReapDecision::Kill;
        }
        // Unknown location with gateway basename: do not kill strangers.
        return ReapDecision::Keep;
    }

    // Path unavailable: basename-only fallback (Windows ACL edge cases).
    if basename_matches_current_version(&proc.basename, &ctx.current_version) {
        return ReapDecision::Keep;
    }
    if is_versioned_gateway_basename(&proc.basename) {
        return ReapDecision::Kill;
    }
    // Unversioned without path: cannot tell current from stale.
    ReapDecision::Keep
}

fn label_process(proc: &GatewayProcess) -> String {
    match &proc.path {
        Some(p) => format!("{} (pid {} @ {})", proc.basename, proc.pid, p.display()),
        None => format!("{} (pid {})", proc.basename, proc.pid),
    }
}

fn reap_with_context(ctx: &ReapContext) -> ReapReport {
    let mut report = ReapReport::default();
    let procs = list_gateway_processes();
    let mut to_kill: Vec<GatewayProcess> = Vec::new();
    for proc in procs {
        match decide_reap(&proc, ctx) {
            ReapDecision::Keep => report.kept.push(label_process(&proc)),
            ReapDecision::Kill => to_kill.push(proc),
        }
    }
    for proc in to_kill {
        let label = label_process(&proc);
        if kill_gateway_process(&proc) {
            report.killed.push(label);
        } else {
            report.failed.push(label);
        }
    }
    // Verify: anything still present that should be gone?
    if !report.killed.is_empty() || !report.failed.is_empty() {
        std::thread::sleep(std::time::Duration::from_millis(150));
        let still = list_gateway_processes();
        for proc in still {
            if decide_reap(&proc, ctx) == ReapDecision::Kill {
                let label = label_process(&proc);
                if kill_gateway_process(&proc) {
                    if !report.killed.iter().any(|k| k == &label) {
                        report.killed.push(format!("{label} [retry]"));
                    }
                } else if !report.failed.iter().any(|f| f == &label) {
                    report.failed.push(format!("{label} [still running]"));
                }
            }
        }
    }
    report
}

fn log_reap_report(kind: &str, report: &ReapReport) {
    if !report.killed.is_empty() {
        eprintln!(
            "toolport: {kind} stopped {} gateway process(es): {}",
            report.killed.len(),
            report.killed.join("; ")
        );
    }
    if !report.failed.is_empty() {
        eprintln!(
            "toolport: {kind} failed to stop {} gateway process(es): {}",
            report.failed.len(),
            report.failed.join("; ")
        );
    }
}

/// Terminate every Toolport/Conduit gateway process (all platforms). Used before
/// in-app update so locked binaries can be replaced. Does not touch parent apps.
/// Returns how many processes were successfully killed.
pub fn stop_spawned_gateways() -> u32 {
    let ctx = ReapContext {
        current_version: env!("CARGO_PKG_VERSION").to_string(),
        keep_paths: Vec::new(),
        kill_all: true,
    };
    let report = reap_with_context(&ctx);
    log_reap_report("updater reaper", &report);
    report.killed.len() as u32
}

/// Terminate gateway processes that are not the current install. Safe on every
/// launch: keeps processes whose path matches the current resolved/published
/// binary (and current-version basenames). Returns labels of killed processes.
pub fn stop_stale_gateways() -> Vec<String> {
    stop_stale_gateways_with_keep(&[])
}

/// Like [`stop_stale_gateways`], with extra keep-paths (e.g. nested macOS helper
/// from `clients::resolve_gateway_path`).
pub fn stop_stale_gateways_with_keep(extra_keep: &[PathBuf]) -> Vec<String> {
    let mut keep = default_keep_paths();
    for p in extra_keep {
        if !keep.iter().any(|e| paths_equal(e, p)) {
            keep.push(p.clone());
        }
    }
    let ctx = ReapContext {
        current_version: env!("CARGO_PKG_VERSION").to_string(),
        keep_paths: keep,
        kill_all: false,
    };
    let report = reap_with_context(&ctx);
    log_reap_report("stale reaper", &report);
    report.killed_labels()
}

// ----- OS process list / kill ------------------------------------------------

#[cfg(windows)]
fn list_gateway_processes() -> Vec<GatewayProcess> {
    windows_list_gateway_processes()
}

#[cfg(windows)]
fn kill_gateway_process(proc: &GatewayProcess) -> bool {
    windows_kill_pid(proc.pid)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn list_gateway_processes() -> Vec<GatewayProcess> {
    linux_list_gateway_processes()
}

#[cfg(target_os = "macos")]
fn list_gateway_processes() -> Vec<GatewayProcess> {
    macos_list_gateway_processes()
}

#[cfg(unix)]
fn kill_gateway_process(proc: &GatewayProcess) -> bool {
    unix_kill_pid(proc.pid)
}

#[cfg(windows)]
fn windows_list_gateway_processes() -> Vec<GatewayProcess> {
    use std::mem::{size_of, zeroed};
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return Vec::new();
        }
        let mut entry: PROCESSENTRY32W = zeroed();
        entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
        let mut out = Vec::new();
        if Process32FirstW(snap, &mut entry) != 0 {
            loop {
                let basename = widestr_to_string(&entry.szExeFile);
                if is_gateway_basename(&basename) {
                    let pid = entry.th32ProcessID;
                    let path = windows_process_path(pid);
                    out.push(GatewayProcess {
                        pid,
                        path,
                        basename,
                    });
                }
                if Process32NextW(snap, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snap);
        out
    }
}

#[cfg(windows)]
fn widestr_to_string(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

#[cfg(windows)]
fn windows_process_path(pid: u32) -> Option<PathBuf> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return None;
        }
        let mut buf = [0u16; 1024];
        let mut size = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(handle, 0, buf.as_mut_ptr(), &mut size);
        CloseHandle(handle);
        if ok == 0 || size == 0 {
            return None;
        }
        Some(PathBuf::from(String::from_utf16_lossy(
            &buf[..size as usize],
        )))
    }
}

#[cfg(windows)]
fn windows_kill_pid(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};
    unsafe {
        let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if handle.is_null() {
            // Fall back to taskkill when OpenProcess is denied.
            return windows_taskkill_pid(pid);
        }
        let ok = TerminateProcess(handle, 1) != 0;
        CloseHandle(handle);
        if ok {
            true
        } else {
            windows_taskkill_pid(pid)
        }
    }
}

#[cfg(windows)]
fn windows_taskkill_pid(pid: u32) -> bool {
    use std::os::windows::process::CommandExt;
    let mut cmd = std::process::Command::new("taskkill");
    cmd.args(["/F", "/PID", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    cmd.status().map(|s| s.success()).unwrap_or(false)
}

/// Linux: `/proc/<pid>/exe` + `/proc/<pid>/comm`.
#[cfg(all(unix, not(target_os = "macos")))]
fn linux_list_gateway_processes() -> Vec<GatewayProcess> {
    let Ok(rd) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for ent in rd.flatten() {
        let name = ent.file_name();
        let name = name.to_string_lossy();
        if !name.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let pid: u32 = match name.parse() {
            Ok(p) => p,
            Err(_) => continue,
        };
        let comm_path = ent.path().join("comm");
        let basename = std::fs::read_to_string(&comm_path)
            .unwrap_or_default()
            .trim()
            .to_string();
        if !is_gateway_basename(&basename) {
            // Also check exe basename — comm can be truncated to 15 chars.
            let exe = std::fs::read_link(ent.path().join("exe")).ok();
            let exe_base = exe
                .as_ref()
                .and_then(|p| p.file_name())
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            if !is_gateway_basename(&exe_base) {
                continue;
            }
            out.push(GatewayProcess {
                pid,
                path: exe,
                basename: exe_base,
            });
            continue;
        }
        let path = std::fs::read_link(ent.path().join("exe")).ok();
        // read_link may append " (deleted)" on some kernels via weird paths — keep as-is.
        out.push(GatewayProcess {
            pid,
            path,
            basename,
        });
    }
    out
}

/// macOS: `ps` for pid + comm (no spaces), then `proc_pidpath` for the real
/// executable path. Do not parse `command=` — default install paths contain
/// spaces (`Application Support`) and break whitespace splits.
#[cfg(target_os = "macos")]
fn macos_list_gateway_processes() -> Vec<GatewayProcess> {
    let Ok(out) = std::process::Command::new("ps")
        .args(["-axo", "pid=", "comm="])
        .output()
    else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut procs = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split_whitespace();
        let Some(pid_s) = parts.next() else {
            continue;
        };
        let Ok(pid) = pid_s.parse::<u32>() else {
            continue;
        };
        // comm= is a single field (no path, no args).
        let comm = parts.collect::<Vec<_>>().join(" ");
        if comm.is_empty() || !is_gateway_basename(&comm) {
            continue;
        }
        let path = macos_proc_pidpath(pid);
        let basename = path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or(comm);
        if !is_gateway_basename(&basename) {
            continue;
        }
        procs.push(GatewayProcess {
            pid,
            path,
            basename,
        });
    }
    procs
}

/// Full executable path for a pid via libproc (handles spaces and symlinks).
#[cfg(target_os = "macos")]
fn macos_proc_pidpath(pid: u32) -> Option<PathBuf> {
    // proc_pidpath is in libSystem on macOS; no extra crate needed.
    extern "C" {
        fn proc_pidpath(pid: i32, buffer: *mut std::ffi::c_void, buffersize: u32) -> i32;
    }
    let mut buf = [0u8; 4096];
    let n = unsafe {
        proc_pidpath(
            pid as i32,
            buf.as_mut_ptr() as *mut std::ffi::c_void,
            buf.len() as u32,
        )
    };
    if n <= 0 {
        return None;
    }
    let s = std::str::from_utf8(&buf[..n as usize]).ok()?;
    if s.is_empty() {
        return None;
    }
    Some(PathBuf::from(s))
}

#[cfg(unix)]
fn unix_kill_pid(pid: u32) -> bool {
    // SIGTERM first, then SIGKILL — matches polite shutdown without depending on libc.
    let term = std::process::Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !term {
        return std::process::Command::new("kill")
            .args(["-KILL", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
    }
    std::thread::sleep(std::time::Duration::from_millis(100));
    // Still alive?
    let alive = std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if alive {
        return std::process::Command::new("kill")
            .args(["-KILL", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
    }
    true
}

/// Last path segment, regardless of OS path separator (client configs store Windows paths).
fn path_basename(stored: &str) -> &str {
    stored.rsplit(['\\', '/']).next().unwrap_or(stored)
}

/// Whether a stored client config still points at an install-dir (unversioned) gateway.
pub fn is_unversioned_install_gateway_path(stored: &str) -> bool {
    let lower = stored.to_ascii_lowercase();
    if lower.contains("conduit-gateway") {
        return true;
    }
    let name_lower = path_basename(stored).to_ascii_lowercase();
    if name_lower != "toolport-gateway.exe" && name_lower != "conduit-gateway.exe" {
        return false;
    }
    // Unversioned basename outside Conduit/bin → install dir or other stale layout.
    !(lower.contains("\\conduit\\bin\\") || lower.contains("/conduit/bin/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(version: &str, keep: &[&str], kill_all: bool) -> ReapContext {
        ReapContext {
            current_version: version.into(),
            keep_paths: keep.iter().map(PathBuf::from).collect(),
            kill_all,
        }
    }

    fn proc(pid: u32, basename: &str, path: Option<&str>) -> GatewayProcess {
        GatewayProcess {
            pid,
            basename: basename.into(),
            path: path.map(PathBuf::from),
        }
    }

    #[test]
    fn unversioned_install_path_detected() {
        assert!(is_unversioned_install_gateway_path(
            r"C:\Users\me\AppData\Local\Toolport\toolport-gateway.exe"
        ));
        assert!(!is_unversioned_install_gateway_path(
            r"C:\Users\me\AppData\Roaming\Conduit\bin\toolport-gateway-1.6.0.exe"
        ));
        assert!(is_unversioned_install_gateway_path(
            "/Applications/Toolport.app/Contents/MacOS/conduit-gateway"
        ));
    }

    #[test]
    fn decide_keeps_current_versioned_basename() {
        // SOU-306 regression: current versioned Windows binary must survive.
        let c = ctx("1.9.6", &[r"C:\Users\me\AppData\Roaming\Toolport\bin\toolport-gateway-1.9.6.exe"], false);
        assert_eq!(
            decide_reap(
                &proc(
                    1,
                    "toolport-gateway-1.9.6.exe",
                    Some(r"C:\Users\me\AppData\Roaming\Toolport\bin\toolport-gateway-1.9.6.exe")
                ),
                &c
            ),
            ReapDecision::Keep
        );
    }

    #[test]
    fn decide_kills_older_versioned_basenames() {
        let c = ctx("1.9.6", &[r"C:\Users\me\AppData\Roaming\Toolport\bin\toolport-gateway-1.9.6.exe"], false);
        for (name, path) in [
            (
                "toolport-gateway-1.9.4.exe",
                r"C:\Users\me\AppData\Roaming\Toolport\bin\toolport-gateway-1.9.4.exe",
            ),
            (
                "toolport-gateway-1.9.5.exe",
                r"C:\Users\me\AppData\Roaming\Toolport\bin\toolport-gateway-1.9.5.exe",
            ),
            (
                "conduit-gateway-1.7.2.exe",
                r"C:\Users\me\AppData\Roaming\Conduit\bin\conduit-gateway-1.7.2.exe",
            ),
        ] {
            assert_eq!(
                decide_reap(&proc(2, name, Some(path)), &c),
                ReapDecision::Kill,
                "{name}"
            );
        }
    }

    #[test]
    fn decide_kills_unversioned_when_path_differs_from_keep() {
        // macOS / AppImage: same basename, obsolete path.
        let current = "/Users/me/Library/Application Support/Toolport/bin/toolport-gateway";
        let c = ctx("1.9.6", &[current], false);
        assert_eq!(
            decide_reap(
                &proc(
                    3,
                    "toolport-gateway",
                    Some("/Applications/Toolport.app/Contents/MacOS/toolport-gateway")
                ),
                &c
            ),
            ReapDecision::Kill
        );
        assert_eq!(
            decide_reap(&proc(4, "toolport-gateway", Some(current)), &c),
            ReapDecision::Keep
        );
    }

    #[test]
    fn decide_keeps_dev_target_paths() {
        let c = ctx("1.9.6", &[r"C:\Users\me\AppData\Roaming\Toolport\bin\toolport-gateway-1.9.6.exe"], false);
        assert_eq!(
            decide_reap(
                &proc(
                    5,
                    "toolport-gateway.exe",
                    Some(r"C:\projects\toolport\src-tauri\target\debug\toolport-gateway.exe")
                ),
                &c
            ),
            ReapDecision::Keep
        );
    }

    #[test]
    fn decide_keeps_unknown_location_unversioned() {
        // Do not kill a foreign binary that happens to share the name.
        let c = ctx("1.9.6", &[r"C:\Users\me\AppData\Roaming\Toolport\bin\toolport-gateway-1.9.6.exe"], false);
        assert_eq!(
            decide_reap(
                &proc(6, "toolport-gateway", Some("/opt/other-vendor/toolport-gateway")),
                &c
            ),
            ReapDecision::Keep
        );
    }

    #[test]
    fn decide_basename_only_kills_old_versioned() {
        let c = ctx("1.9.6", &[], false);
        assert_eq!(
            decide_reap(&proc(7, "toolport-gateway-1.9.4.exe", None), &c),
            ReapDecision::Kill
        );
        assert_eq!(
            decide_reap(&proc(8, "toolport-gateway-1.9.6.exe", None), &c),
            ReapDecision::Keep
        );
        assert_eq!(
            decide_reap(&proc(9, "toolport-gateway.exe", None), &c),
            ReapDecision::Keep
        );
    }

    #[test]
    fn decide_keeps_versioned_looking_stranger_outside_install() {
        // CodeRabbit: versioned branch must not kill /opt/other-vendor/toolport-gateway-1.0.0
        // style strangers (and non-version suffixes must not count as versioned).
        let c = ctx(
            "1.9.6",
            &[r"C:\Users\me\AppData\Roaming\Toolport\bin\toolport-gateway-1.9.6.exe"],
            false,
        );
        assert_eq!(
            decide_reap(
                &proc(
                    11,
                    "toolport-gateway-1.0.0",
                    Some("/opt/other-vendor/toolport-gateway-1.0.0")
                ),
                &c
            ),
            ReapDecision::Keep
        );
        assert_eq!(
            decide_reap(
                &proc(
                    12,
                    "toolport-gateway-shim",
                    Some(r"C:\Users\me\AppData\Roaming\Toolport\bin\toolport-gateway-shim.exe")
                ),
                &c
            ),
            // Non-version suffix under our path, not in keep list → unversioned-path kill
            ReapDecision::Kill
        );
    }

    #[test]
    fn version_suffix_requires_digit_and_dot() {
        assert!(looks_like_version_suffix("1.9.4"));
        assert!(looks_like_version_suffix("1.9.4-beta.1"));
        assert!(!looks_like_version_suffix("shim"));
        assert!(!looks_like_version_suffix("helper"));
        assert!(!looks_like_version_suffix("aarch64-apple-darwin"));
        assert!(!is_versioned_gateway_basename("toolport-gateway-shim.exe"));
        assert!(is_versioned_gateway_basename("toolport-gateway-1.9.4.exe"));
    }

    #[test]
    fn decide_kill_all_kills_everything_gateway() {
        let c = ctx("1.9.6", &[r"C:\keep\toolport-gateway-1.9.6.exe"], true);
        assert_eq!(
            decide_reap(
                &proc(10, "toolport-gateway-1.9.6.exe", Some(r"C:\keep\toolport-gateway-1.9.6.exe")),
                &c
            ),
            ReapDecision::Kill
        );
    }

    #[test]
    fn is_gateway_basename_accepts_versioned_and_plain() {
        assert!(is_gateway_basename("toolport-gateway.exe"));
        assert!(is_gateway_basename("toolport-gateway-1.9.6.exe"));
        assert!(is_gateway_basename("conduit-gateway"));
        assert!(!is_gateway_basename("cursor.exe"));
        assert!(!is_gateway_basename("my-toolport-gateway-shim"));
        // Prefix only — must be exact stem or stem-version
        assert!(!is_gateway_basename("toolport-gatewayed"));
    }

    #[test]
    fn manifest_roundtrip_fields() {
        let m = GatewayManifest {
            version: "1.6.0".into(),
            path: r"C:\x\toolport-gateway-1.6.0.exe".into(),
            size: 42,
        };
        let json = serde_json::to_string(&m).unwrap();
        let back: GatewayManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(m, back);
    }
}
