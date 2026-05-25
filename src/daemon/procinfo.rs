use std::path::PathBuf;

use nix::libc;

#[cfg(target_os = "macos")]
extern "C" {
    fn proc_pidinfo(
        pid: libc::c_int,
        flavor: libc::c_int,
        arg: u64,
        buffer: *mut libc::c_void,
        buffersize: libc::c_int,
    ) -> libc::c_int;

    fn proc_pidpath(
        pid: libc::c_int,
        buffer: *mut libc::c_void,
        buffersize: u32,
    ) -> libc::c_int;
}

#[cfg(target_os = "macos")]
const PROC_PIDVNODEPATHINFO: libc::c_int = 9;

#[cfg(target_os = "macos")]
const MAXPATHLEN: usize = 1024;

#[cfg(target_os = "macos")]
#[repr(C)]
struct VnodeInfoPath {
    _vnode_info: [u8; 152],
    path: [u8; MAXPATHLEN],
}

#[cfg(target_os = "macos")]
#[repr(C)]
struct ProcVnodePathInfo {
    cdir: VnodeInfoPath,
    _rdir: VnodeInfoPath,
}

#[cfg(target_os = "macos")]
pub fn get_cwd(pid: i32) -> Option<PathBuf> {
    unsafe {
        let mut info: ProcVnodePathInfo = std::mem::zeroed();
        let size = std::mem::size_of::<ProcVnodePathInfo>() as libc::c_int;
        let ret = proc_pidinfo(pid, PROC_PIDVNODEPATHINFO, 0, &mut info as *mut _ as *mut libc::c_void, size);
        if ret <= 0 {
            return None;
        }
        let cstr = std::ffi::CStr::from_ptr(info.cdir.path.as_ptr() as *const libc::c_char);
        Some(PathBuf::from(cstr.to_string_lossy().into_owned()))
    }
}

#[cfg(target_os = "macos")]
fn get_proc_args(pid: i32) -> Option<Vec<String>> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    let mut size: libc::size_t = 0;

    unsafe {
        if libc::sysctl(
            mib.as_mut_ptr(),
            3,
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        ) != 0
        {
            return None;
        }

        let mut buf = vec![0u8; size];
        if libc::sysctl(
            mib.as_mut_ptr(),
            3,
            buf.as_mut_ptr() as *mut libc::c_void,
            &mut size,
            std::ptr::null_mut(),
            0,
        ) != 0
        {
            return None;
        }

        if buf.len() < 4 {
            return None;
        }

        let argc = i32::from_ne_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;

        // Skip argc (4 bytes), then skip exec_path (null-terminated)
        let mut pos = 4;
        while pos < buf.len() && buf[pos] != 0 {
            pos += 1;
        }
        // Skip null terminators between exec_path and argv
        while pos < buf.len() && buf[pos] == 0 {
            pos += 1;
        }

        let mut args = Vec::new();
        for _ in 0..argc {
            if pos >= buf.len() {
                break;
            }
            let start = pos;
            while pos < buf.len() && buf[pos] != 0 {
                pos += 1;
            }
            if let Ok(s) = std::str::from_utf8(&buf[start..pos]) {
                args.push(s.to_string());
            }
            pos += 1;
        }

        Some(args)
    }
}

const INTERPRETERS: &[&str] = &["node", "python", "python3", "ruby", "perl", "bash", "sh", "zsh"];

#[cfg(target_os = "macos")]
pub fn get_name(pid: i32) -> Option<String> {
    let args = get_proc_args(pid)?;
    if args.is_empty() {
        return None;
    }

    let exe = PathBuf::from(&args[0]);
    let name = exe.file_name()?.to_string_lossy().into_owned();

    if args.len() > 1 && INTERPRETERS.iter().any(|i| *i == name) {
        let script = PathBuf::from(&args[1]);
        return script.file_name().map(|n| n.to_string_lossy().into_owned());
    }

    Some(name)
}

#[cfg(target_os = "linux")]
pub fn get_cwd(pid: i32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{}/cwd", pid)).ok()
}

#[cfg(target_os = "linux")]
pub fn get_name(pid: i32) -> Option<String> {
    if let Ok(cmdline) = std::fs::read(format!("/proc/{}/cmdline", pid)) {
        let args: Vec<String> = cmdline
            .split(|&b| b == 0)
            .filter(|s| !s.is_empty())
            .filter_map(|s| std::str::from_utf8(s).ok().map(String::from))
            .collect();

        if !args.is_empty() {
            let exe = PathBuf::from(&args[0]);
            let name = exe.file_name().map(|n| n.to_string_lossy().into_owned())?;

            if args.len() > 1 && INTERPRETERS.iter().any(|i| *i == name) {
                let script = PathBuf::from(&args[1]);
                return script.file_name().map(|n| n.to_string_lossy().into_owned());
            }

            return Some(name);
        }
    }

    std::fs::read_to_string(format!("/proc/{}/comm", pid))
        .ok()
        .map(|s| s.trim().to_string())
}

pub fn get_foreground_pid(master_fd: i32) -> Option<i32> {
    let pgid = unsafe { libc::tcgetpgrp(master_fd) };
    if pgid < 0 {
        None
    } else {
        Some(pgid)
    }
}

pub fn get_git_branch(cwd: &PathBuf) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["-C", &cwd.to_string_lossy(), "rev-parse", "--abbrev-ref", "HEAD"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        None
    }
}

