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

    fn proc_name(
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
pub fn get_name(pid: i32) -> Option<String> {
    unsafe {
        let mut buf = [0u8; 256];
        let ret = proc_name(pid, buf.as_mut_ptr() as *mut libc::c_void, buf.len() as u32);
        if ret <= 0 {
            return None;
        }
        let cstr = std::ffi::CStr::from_ptr(buf.as_ptr() as *const libc::c_char);
        Some(cstr.to_string_lossy().into_owned())
    }
}

#[cfg(target_os = "linux")]
pub fn get_cwd(pid: i32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{}/cwd", pid)).ok()
}

#[cfg(target_os = "linux")]
pub fn get_name(pid: i32) -> Option<String> {
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

