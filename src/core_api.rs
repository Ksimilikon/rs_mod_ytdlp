//! The few core functions the mod needs, resolved through
//! `ModHost::get_api` (the mod never links the core).

use mod_sys::sdk::Host;
use std::{
    ffi::{CString, c_char},
    path::{Path, PathBuf},
    ptr,
};

/// `core_get_dir` kinds (`FFICoreDir` of the core).
#[derive(Debug, Clone, Copy)]
#[repr(u32)]
pub enum Dir {
    Cache = 0,
    Config = 1,
    Music = 3,
}

type GetDir = unsafe extern "C" fn(kind: u32, out: *mut *mut c_char) -> u32;
type FreeString = unsafe extern "C" fn(s: *mut c_char);
type AddTracks =
    unsafe extern "C" fn(paths: *const *const c_char, len: usize, out: *mut u64) -> u32;

/// `FFIIdxStatus` codes worth naming.
fn idx_status(code: u32) -> &'static str {
    match code {
        1 => "the core is not initialized",
        2 => "null pointer",
        3 => "invalid UTF-8",
        4 => "the core runs without a library",
        5 => "not found",
        6 => "library read failed",
        7 => "library write failed",
        9 => "the core panicked",
        10 => "invalid argument",
        _ => "unknown error",
    }
}

/// a core function by name, cast to `F` (must be its exact signature).
fn resolve<F: Copy>(host: &Host, name: &std::ffi::CStr) -> Result<F, String> {
    let p = host.get_api(name);
    if p.is_null() {
        return Err(format!(
            "the core has no `{}` (core too old?)",
            name.to_string_lossy()
        ));
    }
    debug_assert_eq!(size_of::<F>(), size_of::<*const ()>());
    // SAFETY: every caller passes the exact signature of `name`.
    Ok(unsafe { std::mem::transmute_copy(&p) })
}

fn c_to_path(p: *const c_char) -> PathBuf {
    let s = unsafe { std::ffi::CStr::from_ptr(p) };
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        PathBuf::from(std::ffi::OsStr::from_bytes(s.to_bytes()))
    }
    #[cfg(not(unix))]
    {
        PathBuf::from(s.to_string_lossy().into_owned())
    }
}

fn path_to_c(p: &Path) -> Option<CString> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        CString::new(p.as_os_str().as_bytes()).ok()
    }
    #[cfg(not(unix))]
    {
        // the core reads paths as UTF-8 off unix.
        CString::new(p.to_str()?).ok()
    }
}

/// a directory the core was started with.
pub fn get_dir(host: &Host, dir: Dir) -> Result<PathBuf, String> {
    let get: GetDir = resolve(host, c"core_get_dir")?;
    let free: FreeString = resolve(host, c"idx_free_string")?;
    let mut out = ptr::null_mut();
    let code = unsafe { get(dir as u32, &mut out) };
    if code != 0 || out.is_null() {
        return Err(format!("core_get_dir: {}", idx_status(code)));
    }
    let path = c_to_path(out);
    unsafe { free(out) };
    Ok(path)
}

/// adds files to the library; returns how many were written (files the core
/// can't read as music are skipped).
pub fn add_tracks(host: &Host, files: &[PathBuf]) -> Result<u64, String> {
    let add: AddTracks = resolve(host, c"idx_add_tracks")?;
    let owned: Vec<CString> = files
        .iter()
        .map(|p| path_to_c(p).ok_or_else(|| format!("bad path: {}", p.display())))
        .collect::<Result<_, _>>()?;
    let ptrs: Vec<*const c_char> = owned.iter().map(|c| c.as_ptr()).collect();
    let mut added = 0u64;
    let code = unsafe { add(ptrs.as_ptr(), ptrs.len(), &mut added) };
    match code {
        0 => Ok(added),
        c => Err(format!("idx_add_tracks: {}", idx_status(c))),
    }
}

type AddTrackPath =
    unsafe extern "C" fn(path: *const c_char, on_end: Option<extern "C" fn()>) -> u32;
type EngineFn = extern "C" fn() -> u32;

/// `FFIEngineStatus` codes worth naming.
fn engine_status(code: u32) -> &'static str {
    match code {
        1 => "the core is not initialized",
        7 => "the player can't read the file",
        8 => "playback failed",
        9 => "the core panicked",
        _ => "player error",
    }
}

fn engine_call(host: &Host, name: &std::ffi::CStr) -> Result<(), String> {
    let f: EngineFn = resolve(host, name)?;
    match f() {
        0 => Ok(()),
        c => Err(format!("{}: {}", name.to_string_lossy(), engine_status(c))),
    }
}

/// plays a file now (replaces what the player was playing).
pub fn play_file(host: &Host, file: &Path) -> Result<(), String> {
    let add: AddTrackPath = resolve(host, c"engine_add_track_path")?;
    let c = path_to_c(file).ok_or_else(|| format!("bad path: {}", file.display()))?;
    match unsafe { add(c.as_ptr(), None) } {
        0 => Ok(()),
        code => Err(engine_status(code).into()),
    }
}

pub fn play_pause(host: &Host) -> Result<(), String> {
    engine_call(host, c"engine_play_pause")
}

/// stops playback and clears the player's queue.
pub fn stop(host: &Host) -> Result<(), String> {
    engine_call(host, c"engine_stop")
}
