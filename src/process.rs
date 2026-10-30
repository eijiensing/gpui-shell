//! Running-process discovery and signalling for the tray's fallback view.
//!
//! Not every background application exports a StatusNotifierItem, so the tray
//! can switch to the current user's processes from `/proc`. This keeps the
//! original `htop`/`pkill` use case available. Like the launcher module, it
//! deliberately knows nothing about GPUI: it scans, filters, and sends signals,
//! and a view renders the result.

use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

/// Upper bound on the command line stored for display. Some processes (web
/// browsers in particular) have command lines long enough to waste memory and
/// make every row huge.
const MAX_COMMAND_CHARS: usize = 240;

/// A process shown in the tray.
pub struct Process {
    /// Process ID.
    pub pid: u32,
    /// Short display name: the basename of `argv[0]`, or `comm` when the
    /// executable path is empty.
    pub name: String,
    /// Full command line, truncated for display.
    pub command: String,
    /// Kernel start time from `/proc/<pid>/stat`, in clock ticks since boot.
    ///
    /// Together with the PID this identifies a particular incarnation of a
    /// process, so a signal sent after the list was built cannot hit a PID that
    /// was recycled in the meantime.
    pub start_time: u64,
}

/// The current user's processes, sorted case-insensitively by display name.
#[derive(Default)]
pub struct Processes {
    entries: Vec<Process>,
}

impl Processes {
    /// Reads `/proc` and builds the process list.
    ///
    /// This touches the filesystem, so call it from a background task rather
    /// than from a render path.
    pub fn scan() -> Self {
        let Some(uid) = current_uid() else {
            return Self::default();
        };
        let self_pid = std::process::id();

        let Ok(dir) = fs::read_dir("/proc") else {
            return Self::default();
        };

        let mut entries = Vec::new();
        for entry in dir.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<u32>().ok())
            else {
                continue;
            };

            // Never offer to stop ourselves or init.
            if pid <= 1 || pid == self_pid {
                continue;
            }

            // The metadata of the `/proc/<pid>` directory carries its owner.
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if metadata.uid() != uid {
                continue;
            }

            let Some(stat) = read_stat(pid) else {
                continue;
            };
            // Kernel threads have an empty command line; they also aren't
            // useful things to stop from here.
            let Some(args) = read_cmdline(pid) else {
                continue;
            };

            let name = display_name(&args, &stat.comm);
            let command = summarize(&args);
            entries.push(Process {
                pid,
                name,
                command,
                start_time: stat.start_time,
            });
        }

        entries.sort_by_cached_key(|process| (process.name.to_lowercase(), process.pid));
        Self { entries }
    }

    /// Indices of the processes matching `query`, best matches first.
    ///
    /// An empty query matches everything in catalog order. Name and PID prefix
    /// matches rank above name, command, and PID substring matches.
    pub fn search(&self, query: &str) -> Vec<usize> {
        let query = query.trim().to_lowercase();
        if query.is_empty() {
            return (0..self.entries.len()).collect();
        }

        let mut prefix = Vec::new();
        let mut substring = Vec::new();
        for (index, process) in self.entries.iter().enumerate() {
            let name = process.name.to_lowercase();
            let pid = process.pid.to_string();

            if name.starts_with(&query) || pid.starts_with(&query) {
                prefix.push(index);
            } else if name.contains(&query)
                || process.command.to_lowercase().contains(&query)
                || pid.contains(&query)
            {
                substring.push(index);
            }
        }

        prefix.extend(substring);
        prefix
    }

    pub fn entry(&self, index: usize) -> Option<&Process> {
        self.entries.get(index)
    }

    pub fn contains(&self, pid: u32) -> bool {
        self.entries.iter().any(|process| process.pid == pid)
    }
}

/// Politely asks the process to stop.
pub fn stop(pid: u32, start_time: u64) -> io::Result<()> {
    signal(pid, start_time, libc::SIGTERM)
}

/// Stops the process immediately, without letting it clean up.
pub fn force_stop(pid: u32, start_time: u64) -> io::Result<()> {
    signal(pid, start_time, libc::SIGKILL)
}

/// Politely asks the process with `pid` to stop, looked up by PID alone.
///
/// This is the fallback for tray items whose menu has no quit action: the
/// item's bus name tells us which process owns it, so there is no need to
/// guess from a display name. The start time is read immediately before
/// signalling so a PID recycled in the meantime is not hit.
pub fn stop_pid(pid: u32) -> io::Result<()> {
    if pid == std::process::id() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "refusing to signal this process",
        ));
    }

    let stat = read_stat(pid)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "process is no longer running"))?;
    signal(pid, stat.start_time, libc::SIGTERM)
}

fn signal(pid: u32, start_time: u64, signal: libc::c_int) -> io::Result<()> {
    // PID 0 would signal our whole process group and PID 1 is init.
    if pid <= 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "refusing to signal this process",
        ));
    }

    // The PID may have been reused since the list was scanned. Checking the
    // start time makes sure the signal still targets the process the user saw.
    if read_stat(pid).map(|stat| stat.start_time) != Some(start_time) {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "process is no longer running",
        ));
    }

    // SAFETY: `kill` is a thread-safe syscall and `pid` was validated above to
    // be greater than 1.
    let result = unsafe { libc::kill(pid as libc::pid_t, signal) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// The parts of `/proc/<pid>/stat` the tray cares about.
struct Stat {
    comm: String,
    start_time: u64,
}

fn read_stat(pid: u32) -> Option<Stat> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    parse_stat(&stat)
}

/// Parses a `/proc/<pid>/stat` line.
///
/// `comm` is wrapped in parentheses and may itself contain spaces and
/// parentheses, so the field is delimited by the first `(` and the last `)`
/// rather than split on whitespace.
fn parse_stat(stat: &str) -> Option<Stat> {
    let open = stat.find('(')?;
    let close = stat.rfind(')')?;
    if close <= open {
        return None;
    }

    let comm = stat[open + 1..close].to_string();
    let rest: Vec<&str> = stat[close + 1..].split_whitespace().collect();

    // `rest[0]` is field 3 (state); starttime is field 22 overall, so it is at
    // index 19 once the first two fields are gone.
    let state = rest.first()?.chars().next()?;
    if matches!(state, 'Z' | 'X') {
        // Zombies and dead tasks are not worth stopping.
        return None;
    }
    let start_time = rest.get(19)?.parse().ok()?;

    Some(Stat { comm, start_time })
}

/// Reads the NUL-separated command line, returning `None` for kernel threads
/// and processes that vanished during the scan.
fn read_cmdline(pid: u32) -> Option<Vec<String>> {
    let bytes = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let args: Vec<String> = bytes
        .split(|byte| *byte == 0)
        .filter(|arg| !arg.is_empty())
        .map(|arg| String::from_utf8_lossy(arg).into_owned())
        .collect();

    (!args.is_empty()).then_some(args)
}

/// A short, human-readable name for the process.
fn display_name(args: &[String], comm: &str) -> String {
    // Some applications rewrite argv to a single string containing the whole
    // command line. Use its first token so the name becomes the executable
    // rather than the last path component of the arguments.
    let program = args.first().map(|program| {
        if args.len() == 1 {
            program
                .split_whitespace()
                .next()
                .unwrap_or(program.as_str())
        } else {
            program.as_str()
        }
    });
    let program = program
        .and_then(|program| Path::new(program).file_name())
        .and_then(|name| name.to_str())
        .map(str::trim)
        .filter(|name| !name.is_empty());

    program.unwrap_or(comm).to_string()
}

/// Joins the arguments into a command line, truncated to a sane display size.
fn summarize(args: &[String]) -> String {
    let command = args.join(" ");
    if command.chars().count() <= MAX_COMMAND_CHARS {
        return command;
    }

    let mut truncated: String = command.chars().take(MAX_COMMAND_CHARS).collect();
    truncated.push('…');
    truncated
}

fn current_uid() -> Option<u32> {
    fs::metadata("/proc/self")
        .ok()
        .map(|metadata| metadata.uid())
}

#[cfg(test)]
mod tests {
    use super::{Processes, display_name, parse_stat, summarize};

    #[test]
    fn search_ranks_prefix_before_substring() {
        let processes = Processes {
            entries: vec![
                process(10, "Alpha", "/usr/bin/alpha"),
                process(20, "Xylophone", "xylophone --play"),
                process(30, "Alchemy", "/opt/alchemy"),
            ],
        };

        // "al" matches "Alpha" and "Alchemy" as prefixes.
        assert_eq!(processes.search("al"), vec![0, 2]);

        // "ph" only matches as a substring: both "Alpha" and "Xylophone".
        assert_eq!(processes.search("ph"), vec![0, 1]);

        // An empty query keeps catalog order.
        assert_eq!(processes.search("  "), vec![0, 1, 2]);
    }

    #[test]
    fn search_matches_commands_and_pids() {
        let processes = Processes {
            entries: vec![
                process(4242, "Firefox", "/usr/lib/firefox/firefox --private-window"),
                process(7, "music", "mpd --no-daemon"),
            ],
        };

        assert_eq!(processes.search("private"), vec![0]);
        assert_eq!(processes.search("mpd"), vec![1]);
        assert_eq!(processes.search("424"), vec![0]);
    }

    #[test]
    fn parse_stat_handles_parentheses_in_comm() {
        // Fields 3..22: state, ppid, pgrp, session, tty_nr, tpgid, flags,
        // minflt, cminflt, majflt, cmajflt, utime, stime, cutime, cstime,
        // priority, nice, num_threads, itrealvalue, starttime.
        let rest = [
            "S", "1", "1234", "1234", "0", "-1", "4194304", "100", "0", "0", "0", "0", "0", "0",
            "0", "0", "20", "4", "0", "987654",
        ];
        let stat = format!("1234 (my (weird) app) {}", rest.join(" "));

        let parsed = parse_stat(&stat).expect("stat should parse");
        assert_eq!(parsed.comm, "my (weird) app");
        assert_eq!(parsed.start_time, 987654);
    }

    #[test]
    fn parse_stat_skips_zombies() {
        let stat = "1234 (gone) Z 1 1234 1234 0 -1 0 0 0 0 0 0 0 0 0 0 0 20 0 987654";
        assert!(parse_stat(stat).is_none());
    }

    #[test]
    fn display_name_uses_the_program_basename() {
        assert_eq!(
            display_name(&["/usr/bin/foo".to_string(), "--bar".to_string()], "foo"),
            "foo"
        );
        // An empty argv[0] falls back to comm.
        assert_eq!(display_name(&["".to_string()], "foo"), "foo");

        // Some apps pack the whole command line into a single argv[0].
        assert_eq!(
            display_name(&["/usr/bin/foo --bar --baz".to_string()], "foo"),
            "foo"
        );
    }

    #[test]
    fn summarize_truncates_long_commands() {
        let args = vec!["app".to_string(), "x".repeat(500)];
        let command = summarize(&args);
        assert_eq!(command.chars().count(), super::MAX_COMMAND_CHARS + 1);
        assert!(command.ends_with('…'));
    }

    #[test]
    fn scan_does_not_panic() {
        let _ = Processes::scan();
    }

    #[test]
    fn stop_pid_refuses_self_and_init() {
        assert!(super::stop_pid(std::process::id()).is_err());
        assert!(super::stop_pid(1).is_err());
    }

    fn process(pid: u32, name: &str, command: &str) -> super::Process {
        super::Process {
            pid,
            name: name.to_string(),
            command: command.to_string(),
            start_time: 0,
        }
    }
}
