//! Pure file-based access to tt-inference-server's serving logs.
//!
//! run.py streams the serving container's stdout/stderr to
//! `<repo>/workflow_logs/docker_server/vllm_*.log` (this is where model-load
//! failures actually appear, and it persists after the container is removed),
//! and writes its own launch log to `<repo>/workflow_logs/run_logs/*.log`.
//! Everything here is "newest *.log in a dir, tail N lines, follow by offset" —
//! no `docker logs` subprocess. Kept pure (std only) so it unit-tests without a
//! router or a real box.

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

pub const DEFAULT_TAIL: usize = 200;
pub const MAX_TAIL: usize = 2000;

/// Which serving-log stream to read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogSource {
    /// The container's stdout/stderr (`workflow_logs/docker_server/*.log`).
    Container,
    /// run.py's own launch log (`workflow_logs/run_logs/*.log`).
    Run,
}

impl LogSource {
    pub fn subdir(&self) -> &'static str {
        match self {
            LogSource::Container => "docker_server",
            LogSource::Run => "run_logs",
        }
    }

    pub fn parse(s: &str) -> Option<LogSource> {
        match s {
            "container" => Some(LogSource::Container),
            "run" => Some(LogSource::Run),
            _ => None,
        }
    }
}

pub fn logs_dir(repo_dir: &Path, source: LogSource) -> PathBuf {
    repo_dir.join("workflow_logs").join(source.subdir())
}

/// Newest `*.log` in `dir` by mtime. `Ok(None)` if `dir` is absent or has no logs.
pub fn newest_log_file(dir: &Path) -> std::io::Result<Option<PathBuf>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("log") {
            continue;
        }
        let mtime = entry.metadata()?.modified()?;
        if best.as_ref().map(|(t, _)| mtime > *t).unwrap_or(true) {
            best = Some((mtime, path));
        }
    }
    Ok(best.map(|(_, p)| p))
}

/// Last `max` lines of `path` (newline-stripped). Reads the whole file; log
/// files here are bounded (run.py rotates per-serve) so this is fine.
pub fn tail_lines(path: &Path, max: usize) -> std::io::Result<Vec<String>> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut lines: Vec<String> = Vec::new();
    for line in reader.lines() {
        lines.push(line?);
    }
    let start = lines.len().saturating_sub(max);
    Ok(lines.split_off(start))
}

/// Lines fully written after byte `from_offset`. Returns the lines and the new
/// end offset (positioned after the last complete line; a trailing partial line
/// is not emitted and not counted, so it re-reads whole on the next call).
pub fn read_new_lines(path: &Path, from_offset: u64) -> std::io::Result<(Vec<String>, u64)> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    if from_offset >= len {
        return Ok((Vec::new(), from_offset.min(len)));
    }
    file.seek(SeekFrom::Start(from_offset))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    // Find the last newline; everything after it is a partial line.
    let last_nl = buf.iter().rposition(|&b| b == b'\n');
    let complete_len = match last_nl {
        Some(idx) => idx + 1, // include the newline
        None => 0,            // no complete line yet
    };
    let complete = &buf[..complete_len];
    let lines: Vec<String> = complete
        .split(|&b| b == b'\n')
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect();
    Ok((lines, from_offset + complete_len as u64))
}

/// Mask obvious secret shapes. Cheap defense-in-depth for an unauthed surface.
pub fn redact_line(line: &str) -> String {
    // hf_<20+ alnum> → hf_***
    let mut out = mask_prefixed(line, "hf_", 20);
    // sk-<20+ alnum> → sk-***
    out = mask_prefixed(&out, "sk-", 20);
    // "Bearer <token>" → "Bearer ***"
    out = mask_after(&out, "Bearer ");
    out
}

fn mask_prefixed(s: &str, prefix: &str, min_len: usize) -> String {
    let mut result = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find(prefix) {
        let (head, tail) = rest.split_at(pos);
        result.push_str(head);
        let after = &tail[prefix.len()..];
        let tok_len = after
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .count();
        if tok_len >= min_len {
            result.push_str(prefix);
            result.push_str("***");
            rest = &after[tok_len..];
        } else {
            result.push_str(prefix);
            rest = after;
        }
    }
    result.push_str(rest);
    result
}

fn mask_after(s: &str, marker: &str) -> String {
    match s.find(marker) {
        Some(pos) => {
            let after = &s[pos + marker.len()..];
            let tok_len = after
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == '-' || *c == '_')
                .count();
            if tok_len >= 8 {
                format!("{}{}***{}", &s[..pos], marker, &after[tok_len..])
            } else {
                s.to_string()
            }
        }
        None => s.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn source_parse_and_subdir() {
        assert!(matches!(
            LogSource::parse("container"),
            Some(LogSource::Container)
        ));
        assert!(matches!(LogSource::parse("run"), Some(LogSource::Run)));
        assert!(LogSource::parse("bogus").is_none());
        assert_eq!(LogSource::Container.subdir(), "docker_server");
        assert_eq!(LogSource::Run.subdir(), "run_logs");
    }

    #[test]
    fn logs_dir_joins_workflow_logs() {
        let d = logs_dir(Path::new("/repo"), LogSource::Container);
        assert_eq!(d, Path::new("/repo/workflow_logs/docker_server"));
    }

    #[test]
    fn newest_log_file_picks_newest_and_handles_missing() {
        use std::time::{Duration, SystemTime};
        let dir = tempfile::tempdir().unwrap();
        // missing subdir → None
        assert!(newest_log_file(&dir.path().join("nope")).unwrap().is_none());
        // empty dir → None
        assert!(newest_log_file(dir.path()).unwrap().is_none());
        let old = dir.path().join("a.log");
        let new = dir.path().join("b.log");
        std::fs::write(&old, "old\n").unwrap();
        std::fs::write(&new, "new\n").unwrap();
        // force distinct mtimes
        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        filetime::set_file_mtime(&old, filetime::FileTime::from_system_time(base)).unwrap();
        filetime::set_file_mtime(
            &new,
            filetime::FileTime::from_system_time(base + Duration::from_secs(10)),
        )
        .unwrap();
        assert_eq!(newest_log_file(dir.path()).unwrap().unwrap(), new);
    }

    #[test]
    fn tail_lines_returns_last_n() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.log");
        std::fs::write(&p, "l1\nl2\nl3\nl4\n").unwrap();
        assert_eq!(tail_lines(&p, 2).unwrap(), vec!["l3", "l4"]);
        assert_eq!(tail_lines(&p, 99).unwrap(), vec!["l1", "l2", "l3", "l4"]);
        let empty = dir.path().join("e.log");
        std::fs::write(&empty, "").unwrap();
        assert!(tail_lines(&empty, 5).unwrap().is_empty());
    }

    #[test]
    fn read_new_lines_skips_trailing_partial() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f.log");
        std::fs::write(&p, "one\ntwo\npar").unwrap(); // "par" has no newline yet
        let (lines, off) = read_new_lines(&p, 0).unwrap();
        assert_eq!(lines, vec!["one", "two"]);
        // offset must sit right after "two\n", so the partial re-reads whole later
        assert_eq!(off, "one\ntwo\n".len() as u64);
        // append the rest of the partial line + a new one
        std::fs::write(&p, "one\ntwo\npartial\nthree\n").unwrap();
        let (lines2, _off2) = read_new_lines(&p, off).unwrap();
        assert_eq!(lines2, vec!["partial", "three"]);
    }

    #[test]
    fn redact_masks_known_secret_shapes() {
        assert_eq!(
            redact_line("token hf_abcdefghijklmnopqrstuvwx done"),
            "token hf_*** done"
        );
        assert_eq!(
            redact_line("Authorization: Bearer deadbeefcafebabe0123"),
            "Authorization: Bearer ***"
        );
        assert_eq!(
            redact_line("plain line, no secrets"),
            "plain line, no secrets"
        );
    }
}
