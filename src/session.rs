//! Persistent session metadata so external `pause` / `resume` / `stop`
//! subcommands can find the active recorder process.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub pid: u32,
    pub output_path: PathBuf,
    pub sample_rate: u32,
    pub channels: u8,
    pub bitrate_bps: i32,
    pub started_unix_ms: i64,
}

pub fn session_dir() -> Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("dev", "kvr", "kagantic-voice-recorder")
        .ok_or_else(|| anyhow::anyhow!("cannot resolve project directories"))?;
    Ok(dirs.data_dir().to_path_buf())
}

pub fn session_path() -> Result<PathBuf> {
    Ok(session_dir()?.join("session.json"))
}

pub fn write_session(s: &Session) -> Result<()> {
    let p = session_path()?;
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).context("create session dir")?;
    }
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(s)?).context("write session tmp")?;
    std::fs::rename(&tmp, &p).context("rename session file")?;
    Ok(())
}

pub fn read_session() -> Result<Option<Session>> {
    let p = session_path()?;
    match std::fs::read(&p) {
        Ok(bytes) => {
            let s: Session = serde_json::from_slice(&bytes).context("parse session")?;
            Ok(Some(s))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).context("read session"),
    }
}

pub fn clear_session() -> Result<()> {
    let p = session_path()?;
    match std::fs::remove_file(&p) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).context("remove session file"),
    }
}

pub fn is_pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    unsafe {
        if libc::kill(pid as i32, 0) == 0 {
            return true;
        }
        std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    }
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::Foundation::{CloseHandle, ERROR_INVALID_PARAMETER};
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };

        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            // Only ERROR_INVALID_PARAMETER means "no such process"; any other
            // failure (access denied, …) conservatively stays alive, mirroring
            // the Unix EPERM behaviour.
            return std::io::Error::last_os_error().raw_os_error()
                != Some(ERROR_INVALID_PARAMETER as i32);
        }
        let mut exit_code: u32 = 0;
        let alive = if GetExitCodeProcess(handle, &mut exit_code) != 0 {
            exit_code == windows_sys::Win32::Foundation::STILL_ACTIVE as u32
        } else {
            // Exit code unavailable — conservatively keep the session.
            true
        };
        let _ = CloseHandle(handle);
        alive
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        false
    }
}

#[deprecated(
    since = "0.1.0",
    note = "use default_path_for — does not avoid collisions"
)]
pub fn output_path_for(dir: &Path, ext: &str) -> PathBuf {
    let stamp = unix_timestamp_string();
    dir.join(format!("recording-{stamp}.{ext}"))
}

/// Resolve a non-existent `dir/{stem}.{ext}` path by appending a `-1`, `-2`,
/// `-3`, … suffix until one is free. Used by both the CLI and the GUI so
/// the recorder never silently overwrites an existing take.
pub fn next_available_path(dir: &Path, stem: &str, ext: &str) -> PathBuf {
    let dot = if ext.is_empty() { "" } else { "." };
    let candidate = dir.join(format!("{stem}{dot}{ext}"));
    if !candidate.exists() {
        return candidate;
    }
    // 1-indexed fallback: the first duplicate is `-1`, then `-2`, `-3`, …
    // This is what users expect ("the second take should be `cat-1.opus`",
    // not "skip `-1` and use `-2`").
    let mut suffix: u32 = 1;
    loop {
        let candidate = dir.join(format!("{stem}-{suffix}{dot}{ext}"));
        if !candidate.exists() {
            return candidate;
        }
        suffix = match suffix.checked_add(1) {
            Some(next) => next,
            // Exclusive sink creation will reject this remaining collision.
            None => return dir.join(format!("{stem}-{suffix}{dot}{ext}")),
        };
    }
}

/// Resolve the actual recording path from the user's typed template text
/// and the chosen file extension. The template stem is replaced with a
/// fresh `YYYYMMDD-HHMMSS` timestamp when the user kept the placeholder
/// (`recording` or empty); otherwise the user's stem is preserved
/// verbatim, with `-1`, `-2`, … appended on collision.
///
/// Callers MUST NOT mutate their template field after this resolves the
/// path, otherwise the next call will see the previous resolved
/// timestamp as the "user input" and start suffixing instead of producing
/// a fresh timestamp.
pub fn resolve_recording_path(typed: &Path, ext: &str) -> PathBuf {
    match (typed.parent(), typed.file_stem()) {
        (Some(parent), Some(stem)) => {
            let stem = stem.to_string_lossy().into_owned();
            let stem = if stem.is_empty() || stem == "recording" {
                started_at_string()
            } else {
                stem
            };
            next_available_path(parent, &stem, ext)
        }
        _ => typed.to_path_buf(),
    }
}

/// Default recording path: `dir/<YYYYMMDD-HHMMSS>.<ext>`, falling back to
/// `<…>-2.<ext>`, `<…>-3.<ext>`, … if even the seconds-based name collides
/// (e.g. back-to-back clicks within the same second, or a clock skew).
pub fn default_path_for(dir: &Path, ext: &str) -> PathBuf {
    next_available_path(dir, &started_at_string(), ext)
}

/// Human-readable local-time stamp `YYYYMMDD-HHMMSS`, used as the default
/// filename stem so recordings started minutes/hours apart never collide.
pub fn started_at_string() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Convert to local time using the system offset (small fudge that
    // doesn't depend on the `chrono` crate; the date helpers below are
    // pure arithmetic and never panic).
    let (y, mo, d, h, mi, s) = local_ymdhms(secs);
    format!("{y:04}{mo:02}{d:02}-{h:02}{mi:02}{s:02}")
}

/// Epoch seconds → local (year, month, day, hour, minute, second).
fn local_ymdhms(secs: u64) -> (u32, u32, u32, u32, u32, u32) {
    // Local timezone offset in seconds (best effort — failures fall back to
    // UTC, which is still correct for the uniqueness guarantee).
    let offset = std::env::var("TZ")
        .ok()
        .and_then(|_| local_tz_offset_seconds(secs))
        .unwrap_or(0);
    let local = secs.wrapping_add(offset);
    let (y, mo, d) = civil_from_days(days_from_epoch(local));
    let (h, mi, s) = seconds_to_hms(local % 86_400);
    (y, mo, d, h, mi, s)
}

/// Calendar days since 1970-01-01 → (year, month, day) (Howard Hinnant's
/// `civil_from_days` algorithm).
fn civil_from_days(z: u64) -> (u32, u32, u32) {
    let z = z.wrapping_add(719_468);
    let era = (z / 146_097) as i64;
    let doe = (z % 146_097) as i64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = (y + if m <= 2 { 1 } else { 0 }) as u32;
    (y, m, d)
}

fn days_from_epoch(secs: u64) -> u64 {
    secs / 86_400
}

fn seconds_to_hms(secs: u64) -> (u32, u32, u32) {
    (
        (secs / 3600) as u32,
        ((secs / 60) % 60) as u32,
        (secs % 60) as u32,
    )
}

/// Best-effort local timezone offset using libc. Returns `None` on any
/// failure; callers fall back to UTC.
#[cfg(unix)]
fn local_tz_offset_seconds(secs: u64) -> Option<u64> {
    // `localtime_r` writes into its second argument; pass a heap-allocated
    // struct so we don't depend on std lib internals.
    // SAFETY: `localtime_r` only reads from its input and writes to the
    // caller-provided `tm`. We zero-initialise the struct so unused padding
    // cannot leak stack data into libglibc.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let t = secs as libc::time_t;
    // SAFETY: `localtime_r` is async-signal-safe and the `tm` is large
    // enough to receive every field.
    let ptr = unsafe { libc::localtime_r(&t, &mut tm) };
    if ptr.is_null() {
        return None;
    }
    // SAFETY: `timegm` treats `tm` as input and only reads from it.
    let utc_secs = unsafe { libc::timegm(&mut tm) };
    if utc_secs == -1 {
        return None;
    }
    Some(secs.abs_diff(utc_secs as u64))
}

#[cfg(not(unix))]
fn local_tz_offset_seconds(_secs: u64) -> Option<u64> {
    None
}

fn unix_timestamp_string() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{secs}")
}

#[cfg(test)]
mod path_tests {
    use super::*;
    use std::fs;

    #[test]
    fn empty_dir_returns_plain_stem() {
        let dir = tempfile::tempdir().unwrap();
        let p = next_available_path(dir.path(), "recording", "opus");
        assert_eq!(p, dir.path().join("recording.opus"));
    }

    #[test]
    fn collision_appends_dash_one() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("recording.opus"), b"x").unwrap();
        let p = next_available_path(dir.path(), "recording", "opus");
        assert_eq!(p, dir.path().join("recording-1.opus"));
    }

    #[test]
    fn two_collisions_append_dash_two() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("recording.opus"), b"x").unwrap();
        fs::write(dir.path().join("recording-1.opus"), b"x").unwrap();
        let p = next_available_path(dir.path(), "recording", "opus");
        assert_eq!(p, dir.path().join("recording-2.opus"));
    }

    #[test]
    fn rapid_fire_hundred_calls_are_unique() {
        let dir = tempfile::tempdir().unwrap();
        let mut seen = std::collections::HashSet::new();
        for _ in 0..100 {
            let p = next_available_path(dir.path(), "recording", "opus");
            assert!(seen.insert(p.clone()), "duplicate path: {}", p.display());
            // Pre-create the file so the next call must advance the suffix.
            fs::write(&p, b"x").unwrap();
        }
        assert_eq!(seen.len(), 100);
    }

    #[test]
    fn started_at_string_is_15_chars_wide() {
        let s = started_at_string();
        assert_eq!(s.len(), 15, "got {s}");
        let bytes = s.as_bytes();
        assert_eq!(bytes[8], b'-');
        // Every other position is an ASCII digit.
        for (i, b) in bytes.iter().enumerate() {
            if i == 8 {
                continue;
            }
            assert!(b.is_ascii_digit(), "non-digit at {i}: {s}");
        }
    }

    #[test]
    fn started_at_string_advances_each_second() {
        let a = started_at_string();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let b = started_at_string();
        assert_ne!(a, b, "two calls 1.1s apart must produce different stems");
    }

    #[test]
    fn default_path_for_falls_back_to_dash_one_on_same_second_collision() {
        let dir = tempfile::tempdir().unwrap();
        // Pre-create a file at the expected default name so the resolver has
        // to advance the suffix even when the stem itself is the second.
        let first = next_available_path(dir.path(), &started_at_string(), "opus");
        fs::write(&first, b"x").unwrap();
        let second = default_path_for(dir.path(), "opus");
        // The fallback suffix must be `-1`, never `-2`/`-22`/`-2222`/`<epoch>`.
        let file_name = second.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            file_name.ends_with("-1.opus"),
            "got {file_name} — expected the resolver to start at `-1`"
        );
    }

    #[test]
    fn default_path_for_three_presses_within_same_second_are_unique() {
        // Simulates three RECORD clicks in the same second. The first
        // takes the timestamped default; the second and third fall back to
        // `-1` and `-2` respectively — never `-22` or `-222`, and never
        // both pressing down to `-2` (the bug we're fixing here).
        let dir = tempfile::tempdir().unwrap();
        let mut taken = std::collections::HashSet::new();
        for _ in 0..3 {
            let p = default_path_for(dir.path(), "opus");
            assert!(
                taken.insert(p.clone()),
                "duplicate default: {}",
                p.display()
            );
            fs::write(&p, b"x").unwrap();
        }
        let names: Vec<_> = taken
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        // Each press must add a unique `-N` suffix that increments by 1.
        // The stem already contains `-` (timestamp separator), so look for
        // `-<digits>.opus` AFTER the timestamp separator at index 8.
        let mut suffixes: Vec<u32> = names
            .iter()
            .map(|s| s.as_str())
            .filter_map(|s| s.get(15..))
            .map(|s| s.to_string())
            .filter_map(|tail| tail.strip_suffix(".opus").map(str::to_string))
            .filter_map(|s| s.strip_prefix('-').map(str::to_string))
            .filter_map(|s| s.parse::<u32>().ok())
            .collect();
        suffixes.sort();
        assert_eq!(
            suffixes,
            vec![1, 2],
            "expected suffixes [1, 2] across three presses, got {suffixes:?} for {names:?}"
        );
        assert!(
            names
                .iter()
                .all(|s| !s.contains("-22.opus") && !s.contains("-222.opus")),
            "no `-22` / `-222` chains: {names:?}"
        );
    }

    #[test]
    fn resolve_recording_path_substitutes_timestamp_for_placeholder() {
        // The field pre-fills with `<home>/recording.opus`. resolve_recording_path
        // MUST replace that with a fresh `YYYYMMDD-HHMMSS.opus` *at the time it
        // is called*, not at app launch.
        let dir = tempfile::tempdir().unwrap();
        let typed = dir.path().join("recording.opus");
        let a = resolve_recording_path(&typed, "opus");
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let b = resolve_recording_path(&typed, "opus");
        assert_ne!(
            a, b,
            "two calls 1.1s apart on the same placeholder must yield different names"
        );
        // Both must start with the timestamp pattern (15 chars + `.opus`).
        for p in [&a, &b] {
            let name = p.file_name().unwrap().to_string_lossy();
            assert_eq!(name.len(), 20, "{name} (YYYYMMDD-HHMMSS + .opus)");
            assert!(name.contains(".opus"));
        }
    }

    #[test]
    fn resolve_recording_path_preserves_user_stem_and_collides_dash_one() {
        let dir = tempfile::tempdir().unwrap();
        let typed = dir.path().join("cat.opus");
        let first = resolve_recording_path(&typed, "opus");
        assert_eq!(first.file_name().unwrap().to_string_lossy(), "cat.opus");
        fs::write(&first, b"x").unwrap();
        let second = resolve_recording_path(&typed, "opus");
        assert_eq!(second.file_name().unwrap().to_string_lossy(), "cat-1.opus");
    }

    #[test]
    fn civil_from_days_known_dates() {
        // 1970-01-01 → (1970, 1, 1); 2000-01-01 → (2000, 1, 1);
        // 2025-01-01 → (2025, 1, 1); 2026-03-15 → (2026, 3, 15).
        let cases: &[(u64, (u32, u32, u32))] = &[
            (0, (1970, 1, 1)),
            (10957, (2000, 1, 1)),
            (20089, (2025, 1, 1)),
            (20527, (2026, 3, 15)),
        ];
        for &(days, expected) in cases {
            assert_eq!(civil_from_days(days), expected, "days={days}");
        }
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::is_pid_alive;

    /// RAII guard so the spawned helper is killed and reaped even if the test
    /// panics — no leaked child processes.
    struct Child(std::process::Child);

    impl Drop for Child {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// Windows-only regression: `is_pid_alive` must reflect real native process
    /// state, so `cmd_start` / `cmd_status` neither clear a live session nor
    /// report a live session as dead.
    #[test]
    fn reports_real_process_liveness() {
        // The test process itself is alive.
        assert!(is_pid_alive(std::process::id()));

        // Spawn this test executable's ignored helper, which blocks until we
        // terminate it — a real running child, not a stubbed PID.
        let mut child = Child(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "session::windows_tests::liveness_helper",
                    "--ignored",
                ])
                .stdin(std::process::Stdio::piped())
                .spawn()
                .expect("spawn liveness helper"),
        );
        let pid = child.0.id();

        assert!(is_pid_alive(pid));

        child.0.kill().expect("kill helper");
        child.0.wait().expect("reap helper");

        assert!(!is_pid_alive(pid));
    }

    /// Ignored helper only: blocks until killed by `reports_real_process_liveness`.
    #[test]
    #[ignore]
    fn liveness_helper() {
        use std::io::Read;
        // The parent retains the write end until it kills and reaps us.
        std::io::stdin()
            .read_exact(&mut [0u8; 1])
            .expect("parent holds helper stdin open");
    }
}
