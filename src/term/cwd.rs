//! The live working directory of a child process, via libproc.
//!
//! Shells only report their directory through OSC 7 when the user configures it, so a
//! new tab cannot rely on the terminal's PWD alone. macOS can answer directly.

use std::ffi::c_void;
use std::path::PathBuf;

#[cfg(target_os = "macos")]
mod imp {
    use super::*;

    const PROC_PIDVNODEPATHINFO: i32 = 9;
    const MAXPATHLEN: usize = 1024;

    #[repr(C)]
    #[allow(dead_code)]
    struct VinfoStat {
        vst_dev: u32,
        vst_mode: u16,
        vst_nlink: u16,
        vst_ino: u64,
        vst_uid: u32,
        vst_gid: u32,
        vst_atime: i64,
        vst_atimensec: i64,
        vst_mtime: i64,
        vst_mtimensec: i64,
        vst_ctime: i64,
        vst_ctimensec: i64,
        vst_birthtime: i64,
        vst_birthtimensec: i64,
        vst_size: i64,
        vst_blocks: i64,
        vst_blksize: i32,
        vst_flags: u32,
        vst_gen: u32,
        vst_rdev: u32,
        vst_qspare: [i64; 2],
    }

    #[repr(C)]
    #[allow(dead_code)]
    struct VnodeInfo {
        vi_stat: VinfoStat,
        vi_type: i32,
        vi_pad: i32,
        vi_fsid: [i32; 2],
    }

    #[repr(C)]
    #[allow(dead_code)]
    struct VnodeInfoPath {
        vip_vi: VnodeInfo,
        vip_path: [u8; MAXPATHLEN],
    }

    #[repr(C)]
    #[allow(dead_code)]
    struct ProcVnodePathInfo {
        pvi_cdir: VnodeInfoPath,
        pvi_rdir: VnodeInfoPath,
    }

    unsafe extern "C" {
        fn proc_pidinfo(pid: i32, flavor: i32, arg: u64, buffer: *mut c_void, size: i32) -> i32;
    }

    pub fn cwd_of(pid: u32) -> Option<PathBuf> {
        let mut info: ProcVnodePathInfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<ProcVnodePathInfo>() as i32;
        let read = unsafe {
            proc_pidinfo(
                pid as i32,
                PROC_PIDVNODEPATHINFO,
                0,
                (&raw mut info).cast::<c_void>(),
                size,
            )
        };
        if read != size {
            return None;
        }
        let path = &info.pvi_cdir.vip_path;
        let len = path.iter().position(|&b| b == 0)?;
        if len == 0 {
            return None;
        }
        std::str::from_utf8(&path[..len]).ok().map(PathBuf::from)
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use super::*;

    pub fn cwd_of(_pid: u32) -> Option<PathBuf> {
        None
    }
}

/// The process's current directory, or `None` if it exited or the kernel refused.
pub fn cwd_of(pid: u32) -> Option<PathBuf> {
    imp::cwd_of(pid)
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    #[test]
    fn our_own_cwd_is_visible_to_libproc() {
        let here = std::env::current_dir().unwrap();
        let seen = super::cwd_of(std::process::id()).expect("self cwd");
        assert_eq!(seen.canonicalize().unwrap(), here.canonicalize().unwrap());
    }
}
