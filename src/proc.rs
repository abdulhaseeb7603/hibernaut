//! Processes, per platform: which ones are running, and how much memory a process tree holds.

pub struct Proc {
    pub pid: u32,
    pub ppid: u32,
    pub name: String,
    /// Resident bytes, when listing already gave it.
    rss: Option<u64>,
}

pub fn is_claude(name: &str) -> bool {
    name.eq_ignore_ascii_case("claude") || name.eq_ignore_ascii_case("claude.exe")
}

/// `root` and everything it started.
// ponytail: walks parent ids, so on Windows a reused parent pid can adopt strangers; good enough
// for a memory estimate, not for killing.
pub fn tree(all: &[Proc], root: u32) -> Vec<u32> {
    let mut out = vec![root];
    let mut i = 0;
    while i < out.len() {
        let parent = out[i];
        out.extend(all.iter().filter(|p| p.ppid == parent && p.pid != parent && !out.contains(&p.pid)).map(|p| p.pid).collect::<Vec<_>>());
        i += 1;
    }
    out
}

/// Resident memory of `root` and its descendants (claude plus its MCP servers and shells).
pub fn tree_rss(root: u32) -> u64 {
    tree_rss_in(&list(), root)
}

/// `tree_rss` from a listing already made.
pub fn tree_rss_in(all: &[Proc], root: u32) -> u64 {
    tree(all, root).into_iter().map(|pid| all.iter().find(|p| p.pid == pid).and_then(|p| p.rss).unwrap_or_else(|| rss(pid))).sum()
}

#[cfg(windows)]
mod imp {
    use super::Proc;
    use std::mem::{size_of, zeroed};
    use windows_sys::Win32::{
        Foundation::{CloseHandle, INVALID_HANDLE_VALUE},
        System::{
            Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS},
            ProcessStatus::{K32EmptyWorkingSet, K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS},
            Threading::{GetCurrentProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
        },
    };

    pub fn list() -> Vec<Proc> {
        let mut out = Vec::new();
        unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            if snap == INVALID_HANDLE_VALUE {
                return out;
            }
            let mut e: PROCESSENTRY32W = zeroed();
            e.dwSize = size_of::<PROCESSENTRY32W>() as u32;
            let mut ok = Process32FirstW(snap, &mut e);
            while ok != 0 {
                let len = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
                let name = String::from_utf16_lossy(&e.szExeFile[..len]);
                out.push(Proc { pid: e.th32ProcessID, ppid: e.th32ParentProcessID, name, rss: None });
                ok = Process32NextW(snap, &mut e);
            }
            CloseHandle(snap);
        }
        out
    }

    pub fn rss(pid: u32) -> u64 {
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h.is_null() {
                return 0;
            }
            let mut c: PROCESS_MEMORY_COUNTERS = zeroed();
            c.cb = size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
            let ok = K32GetProcessMemoryInfo(h, &mut c, c.cb);
            CloseHandle(h);
            if ok != 0 { c.WorkingSetSize as u64 } else { 0 }
        }
    }

    /// Gives ccnap's own pages back to Windows while it waits; they fault back in on wake.
    pub fn trim_self() {
        unsafe { K32EmptyWorkingSet(GetCurrentProcess()) };
    }

    /// When the process started, in unix ms.
    pub fn started_ms(pid: u32) -> Option<u64> {
        use windows_sys::Win32::{Foundation::FILETIME, System::Threading::GetProcessTimes};
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h.is_null() {
                return None;
            }
            let (mut created, mut a, mut b, mut c): (FILETIME, FILETIME, FILETIME, FILETIME) = (zeroed(), zeroed(), zeroed(), zeroed());
            let ok = GetProcessTimes(h, &mut created, &mut a, &mut b, &mut c);
            CloseHandle(h);
            let ft = (created.dwHighDateTime as u64) << 32 | created.dwLowDateTime as u64;
            // FILETIME counts 100 ns steps from 1601.
            (ok != 0).then(|| (ft / 10_000).saturating_sub(11_644_473_600_000))
        }
    }

    /// Ends one process with `code` as its exit code.
    pub fn terminate(pid: u32, code: u32) -> bool {
        use windows_sys::Win32::System::Threading::{TerminateProcess, PROCESS_TERMINATE};
        unsafe {
            let h = OpenProcess(PROCESS_TERMINATE, 0, pid);
            if h.is_null() {
                return false;
            }
            let ok = TerminateProcess(h, code);
            CloseHandle(h);
            ok != 0
        }
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use super::Proc;
    use std::fs;

    pub fn list() -> Vec<Proc> {
        fs::read_dir("/proc")
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let pid: u32 = e.file_name().to_str()?.parse().ok()?;
                let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
                // "pid (name) state ppid ...", where the name may itself hold spaces and parens.
                let (open, close) = (stat.find('(')?, stat.rfind(')')?);
                let ppid = stat[close + 1..].split_whitespace().nth(1)?.parse().ok()?;
                Some(Proc { pid, ppid, name: stat[open + 1..close].to_string(), rss: None })
            })
            .collect()
    }

    pub fn rss(pid: u32) -> u64 {
        let pages: u64 = fs::read_to_string(format!("/proc/{pid}/statm"))
            .ok()
            .and_then(|s| s.split_whitespace().nth(1)?.parse().ok())
            .unwrap_or(0);
        pages * unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64
    }

    pub fn trim_self() {}
}

#[cfg(all(unix, not(target_os = "linux")))]
mod imp {
    use super::Proc;
    use std::{path::Path, process::Command};

    /// macOS and the BSDs: one `ps` call has everything.
    pub fn list() -> Vec<Proc> {
        let Ok(o) = Command::new("ps").args(["-A", "-o", "pid=,ppid=,rss=,comm="]).output() else { return Vec::new() };
        String::from_utf8_lossy(&o.stdout)
            .lines()
            .filter_map(|l| {
                let mut f = l.split_whitespace();
                let pid = f.next()?.parse().ok()?;
                let ppid = f.next()?.parse().ok()?;
                let kb: u64 = f.next()?.parse().ok()?;
                let comm = f.collect::<Vec<_>>().join(" ");
                let name = Path::new(&comm).file_name()?.to_string_lossy().into_owned();
                Some(Proc { pid, ppid, name, rss: Some(kb * 1024) })
            })
            .collect()
    }

    pub fn rss(_pid: u32) -> u64 {
        0
    }

    pub fn trim_self() {}
}

use imp::rss;
pub use imp::{list, trim_self};
#[cfg(windows)]
pub use imp::{started_ms, terminate};

/// Ends `root` and everything it started: a leftover claude along with its MCP servers.
pub fn end_tree(root: u32) {
    #[cfg(windows)]
    crate::term::kill_tree(root, true);
    #[cfg(unix)]
    for pid in tree(&list(), root).into_iter().rev() {
        // SIGTERM, so claude can shut its servers down itself.
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measures_this_process() {
        let me = std::process::id();
        assert!(list().iter().any(|p| p.pid == me));
        assert!(tree_rss(me) > 1 << 20, "a running test uses more than 1 MB");
    }

    #[test]
    fn trees_follow_parents() {
        let p = |pid, ppid| Proc { pid, ppid, name: String::new(), rss: None };
        let all = [p(1, 0), p(2, 1), p(3, 2), p(4, 1), p(5, 9)];
        assert_eq!(tree(&all, 1), [1, 2, 4, 3]);
        assert_eq!(tree(&all, 5), [5]);
        // Sizes that came with the listing (all macOS has) are the ones counted.
        let p = |pid, ppid, rss| Proc { pid, ppid, name: String::new(), rss: Some(rss) };
        assert_eq!(tree_rss_in(&[p(1, 0, 10), p(2, 1, 5), p(3, 9, 7)], 1), 15);
    }
}
