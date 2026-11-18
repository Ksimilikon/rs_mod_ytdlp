//! Running yt-dlp as a child process: building the command line, reading
//! progress and the final file paths, killing it on cancel.
//!
//! The process is spawned with `std::process`, so it works wherever the OS
//! lets an app run an executable: linux / windows / macos, and android when
//! the frontend ships a runnable yt-dlp (its path goes to the settings).
//! iOS forbids spawning processes — the mod reports that instead of trying.

use std::{
    ffi::OsString,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::Mutex,
};

/// output format. Only what the core can play: no opus / webm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioFormat {
    /// the site's own stream as is, no conversion, no ffmpeg needed: AAC
    /// (m4a — YouTube), else mp3 (SoundCloud, Bandcamp, ...).
    M4a,
    Mp3,
    /// ogg vorbis.
    Vorbis,
    Flac,
}

impl AudioFormat {
    /// in the order of the format `Select` on the tab.
    pub const ALL: [Self; 4] = [Self::M4a, Self::Mp3, Self::Vorbis, Self::Flac];

    pub fn key(self) -> &'static str {
        match self {
            Self::M4a => "m4a",
            Self::Mp3 => "mp3",
            Self::Vorbis => "vorbis",
            Self::Flac => "flac",
        }
    }

    pub fn from_key(k: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|f| f.key() == k)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::M4a => "original m4a / mp3 (no ffmpeg)",
            Self::Mp3 => "mp3 (needs ffmpeg)",
            Self::Vorbis => "ogg vorbis (needs ffmpeg)",
            Self::Flac => "flac (needs ffmpeg, big files)",
        }
    }

    pub fn index(self) -> usize {
        Self::ALL.iter().position(|&f| f == self).unwrap_or(0)
    }

    pub fn from_index(i: u64) -> Option<Self> {
        Self::ALL.get(usize::try_from(i).ok()?).copied()
    }

    pub fn needs_ffmpeg(self) -> bool {
        self != Self::M4a
    }
}

/// everything one yt-dlp run needs.
#[derive(Debug, Clone)]
pub struct DlOptions {
    pub ytdlp: PathBuf,
    /// `--ffmpeg-location`; `None` = `PATH`.
    pub ffmpeg: Option<PathBuf>,
    pub format: AudioFormat,
    pub embed_metadata: bool,
    pub embed_thumbnail: bool,
    /// `--no-playlist`: a `watch?v=…&list=…` link gives one video.
    pub single_video: bool,
    pub out_dir: PathBuf,
    /// partial files go here, so the music folder never sees them.
    pub temp_dir: Option<PathBuf>,
    pub extra_args: Vec<String>,
}

/// line markers of our `--print` / `--progress-template` output.
const M_TITLE: &str = "[rsmod:t]";
const M_FILE: &str = "[rsmod:f]";
const M_PROGRESS: &str = "[rsmod:p]";

/// the yt-dlp command line (without the program).
pub fn build_args(o: &DlOptions, url: &str) -> Vec<OsString> {
    let mut a: Vec<OsString> = Vec::new();
    let mut push = |s: &str| a.push(s.into());
    // the user's yt-dlp config could move files or change output and break
    // what we parse; extra args cover what a config would be used for.
    push("--ignore-config");
    push("--newline");
    push("--no-colors");
    push("--windows-filenames"); // the library may be synced to windows
    push("--no-overwrites");
    push("--no-part");
    if o.single_video {
        push("--no-playlist");
    }
    // `--print` implies `--quiet --simulate`: undo both, keep progress.
    push("--no-simulate");
    push("--progress");
    push("--print");
    a.push(format!("before_dl:{M_TITLE}%(title)s").into());
    a.push("--print".into());
    a.push(format!("after_move:{M_FILE}%(filepath)s").into());
    a.push("--progress-template".into());
    a.push(format!("download:{M_PROGRESS}%(progress._percent_str)s").into());

    let mut push = |s: &str| a.push(s.into());
    match o.format {
        AudioFormat::M4a => {
            push("-f");
            push(
                "bestaudio[ext=m4a]/bestaudio[acodec^=mp4a]/bestaudio[ext=mp3]/bestaudio[acodec=mp3]",
            );
        }
        f => {
            push("-f");
            push("bestaudio/best");
            push("-x");
            push("--audio-format");
            push(f.key());
            push("--audio-quality");
            push("0");
        }
    }
    if o.embed_metadata {
        push("--embed-metadata");
    }
    if o.embed_thumbnail {
        push("--embed-thumbnail");
    }
    push("-o");
    push("%(title)s [%(id)s].%(ext)s");
    a.push("--paths".into());
    a.push(prefixed("home:", &o.out_dir));
    if let Some(t) = &o.temp_dir {
        a.push("--paths".into());
        a.push(prefixed("temp:", t));
    }
    if let Some(f) = &o.ffmpeg {
        a.push("--ffmpeg-location".into());
        a.push(f.into());
    }
    a.extend(o.extra_args.iter().map(OsString::from));
    // nothing after `--` is an option, whatever the link looks like.
    a.push("--".into());
    a.push(url.into());
    a
}

fn prefixed(prefix: &str, p: &Path) -> OsString {
    let mut s = OsString::from(prefix);
    s.push(p.as_os_str());
    s
}

/// what yt-dlp is doing right now.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Title(String),
    Progress(String),
}

/// the running yt-dlp, shared with whoever may cancel it.
#[derive(Default)]
pub struct Running(Mutex<Option<Child>>);

impl Running {
    /// kills the current process (with its children where possible).
    pub fn kill(&self) {
        let mut g = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(child) = g.as_mut() {
            kill_tree(child);
        }
    }
}

#[cfg(unix)]
fn kill_tree(child: &mut Child) {
    // the child leads its own process group (see `spawn`): ffmpeg dies too.
    if let Ok(pid) = i32::try_from(child.id()) {
        unsafe { libc::kill(-pid, libc::SIGKILL) };
    }
    let _ = child.kill();
}

#[cfg(windows)]
fn kill_tree(child: &mut Child) {
    use std::os::windows::process::CommandExt;
    // yt-dlp.exe (PyInstaller) runs the real program in a child process:
    // killing only the parent would leave it downloading.
    let _ = Command::new("taskkill")
        .args(["/T", "/F", "/PID", &child.id().to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .status();
    let _ = child.kill();
}

#[cfg(not(any(unix, windows)))]
fn kill_tree(child: &mut Child) {
    let _ = child.kill();
}

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

fn command(program: &Path) -> Command {
    let mut c = Command::new(program);
    c.stdin(Stdio::null());
    // python writes pipes in the locale encoding otherwise (cp1252 on
    // windows), which breaks non-ASCII titles and paths.
    c.env("PYTHONIOENCODING", "utf-8").env("PYTHONUTF8", "1");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        c.process_group(0);
    }
    c
}

fn spawn_error(program: &Path, e: std::io::Error) -> String {
    if cfg!(target_os = "ios") {
        return "iOS doesn't allow running yt-dlp as a process".into();
    }
    match e.kind() {
        std::io::ErrorKind::NotFound => format!(
            "yt-dlp not found ({}): install it or set its path in the settings",
            program.display()
        ),
        _ => format!("can't run {}: {e}", program.display()),
    }
}

/// is ffmpeg runnable? `location` — the ffmpeg executable or its directory.
pub fn has_ffmpeg(location: Option<&Path>) -> bool {
    if cfg!(target_os = "ios") {
        return false;
    }
    let exe = match location {
        Some(l) if l.is_dir() => l.join(if cfg!(windows) {
            "ffmpeg.exe"
        } else {
            "ffmpeg"
        }),
        Some(l) => l.to_path_buf(),
        None => PathBuf::from("ffmpeg"),
    };
    command(&exe)
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// output bytes → path; on unix the bytes are the path itself.
fn line_to_path(line: &[u8]) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        PathBuf::from(std::ffi::OsStr::from_bytes(line))
    }
    #[cfg(not(unix))]
    {
        PathBuf::from(String::from_utf8_lossy(line).into_owned())
    }
}

/// runs `program args` to the end and returns its stdout lines. A kill
/// through `running` ends it with an error.
pub fn run_lines(
    program: &Path,
    args: Vec<OsString>,
    running: &Running,
) -> Result<Vec<String>, String> {
    let mut child = command(program)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| spawn_error(program, e))?;
    let stdout = child.stdout.take().expect("piped");
    let stderr = child.stderr.take().expect("piped");
    *running.0.lock().unwrap_or_else(|e| e.into_inner()) = Some(child);
    let errors = std::thread::spawn(move || {
        let mut last = None;
        for line in BufReader::new(stderr).split(b'\n').map_while(Result::ok) {
            let line = String::from_utf8_lossy(&line).trim_end().to_owned();
            if let Some(e) = line.strip_prefix("ERROR: ") {
                last = Some(e.to_owned());
            } else if !line.is_empty() && last.is_none() {
                last = Some(line);
            }
        }
        last
    });
    let lines: Vec<String> = BufReader::new(stdout)
        .split(b'\n')
        .map_while(Result::ok)
        .map(|l| String::from_utf8_lossy(&l).into_owned())
        .collect();
    let err = errors.join().ok().flatten();
    let child = running.0.lock().unwrap_or_else(|e| e.into_inner()).take();
    let status = match child {
        Some(mut c) => c.wait().map_err(|e| format!("yt-dlp: {e}"))?,
        None => return Err("cancelled".into()),
    };
    if !status.success() {
        return Err(err.unwrap_or_else(|| format!("yt-dlp exited with {status}")));
    }
    Ok(lines)
}

/// runs yt-dlp to the end. `on_event` gets the title and progress; the
/// result is the files it wrote. A kill through `running` ends it with an
/// error.
pub fn download(
    o: &DlOptions,
    url: &str,
    running: &Running,
    mut on_event: impl FnMut(Event),
) -> Result<Vec<PathBuf>, String> {
    let mut child = command(&o.ytdlp)
        .args(build_args(o, url))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| spawn_error(&o.ytdlp, e))?;
    let stdout = child.stdout.take().expect("piped");
    let stderr = child.stderr.take().expect("piped");
    *running.0.lock().unwrap_or_else(|e| e.into_inner()) = Some(child);

    // stderr is drained on its own thread, or a chatty yt-dlp blocks on a
    // full pipe while we wait on stdout.
    let errors = std::thread::spawn(move || {
        let mut last_error = None;
        let mut last_line = None;
        for line in BufReader::new(stderr).split(b'\n').map_while(Result::ok) {
            let line = String::from_utf8_lossy(&line).trim_end().to_owned();
            if line.is_empty() {
                continue;
            }
            if let Some(e) = line.strip_prefix("ERROR: ") {
                last_error = Some(e.to_owned());
            }
            last_line = Some(line);
        }
        last_error.or(last_line)
    });

    let mut files = Vec::new();
    for line in BufReader::new(stdout).split(b'\n').map_while(Result::ok) {
        let line = line.strip_suffix(b"\r").unwrap_or(&line);
        if let Some(p) = line.strip_prefix(M_FILE.as_bytes()) {
            files.push(line_to_path(p));
            continue;
        }
        let text = String::from_utf8_lossy(line);
        if let Some(t) = text.strip_prefix(M_TITLE) {
            on_event(Event::Title(t.to_owned()));
        } else if let Some(p) = text.strip_prefix(M_PROGRESS) {
            on_event(Event::Progress(p.trim().to_owned()));
        }
    }

    let err = errors.join().ok().flatten();
    let child = running.0.lock().unwrap_or_else(|e| e.into_inner()).take();
    let status = match child {
        Some(mut c) => c.wait().map_err(|e| format!("yt-dlp: {e}"))?,
        None => return Err("cancelled".into()),
    };
    if !status.success() {
        return Err(err.unwrap_or_else(|| format!("yt-dlp exited with {status}")));
    }
    if files.is_empty() {
        // e.g. `--no-overwrites` and the file is already there.
        return Err(err.unwrap_or_else(|| "yt-dlp wrote no file (already downloaded?)".into()));
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> DlOptions {
        DlOptions {
            ytdlp: "yt-dlp".into(),
            ffmpeg: None,
            format: AudioFormat::M4a,
            embed_metadata: false,
            embed_thumbnail: false,
            single_video: true,
            out_dir: "/music/YouTube".into(),
            temp_dir: Some("/cache/ytdlp".into()),
            extra_args: vec![],
        }
    }

    fn args(o: &DlOptions) -> Vec<String> {
        build_args(o, "https://www.youtube.com/watch?v=dQw4w9WgXcQ")
            .into_iter()
            .map(|s| s.into_string().unwrap())
            .collect()
    }

    fn has_seq(a: &[String], seq: &[&str]) -> bool {
        a.windows(seq.len())
            .any(|w| w.iter().zip(seq).all(|(x, y)| x == y))
    }

    #[test]
    fn m4a_needs_no_conversion() {
        let a = args(&opts());
        assert!(has_seq(
            &a,
            &[
                "-f",
                "bestaudio[ext=m4a]/bestaudio[acodec^=mp4a]/bestaudio[ext=mp3]/bestaudio[acodec=mp3]"
            ]
        ));
        assert!(!a.contains(&"-x".to_owned()));
        assert!(a.contains(&"--no-playlist".to_owned()));
        assert!(has_seq(
            &a,
            &[
                "--paths",
                "home:/music/YouTube",
                "--paths",
                "temp:/cache/ytdlp"
            ]
        ));
        // the url is last, after `--`.
        assert_eq!(
            &a[a.len() - 2..],
            ["--", "https://www.youtube.com/watch?v=dQw4w9WgXcQ"]
        );
    }

    #[test]
    fn conversion_and_extras() {
        let o = DlOptions {
            format: AudioFormat::Mp3,
            embed_metadata: true,
            embed_thumbnail: true,
            single_video: false,
            ffmpeg: Some("/opt/ffmpeg".into()),
            temp_dir: None,
            extra_args: vec!["--cookies".into(), "c.txt".into()],
            ..opts()
        };
        let a = args(&o);
        assert!(has_seq(&a, &["-x", "--audio-format", "mp3"]));
        assert!(
            a.contains(&"--embed-metadata".to_owned())
                && a.contains(&"--embed-thumbnail".to_owned())
        );
        assert!(!a.contains(&"--no-playlist".to_owned()));
        assert!(has_seq(&a, &["--ffmpeg-location", "/opt/ffmpeg"]));
        assert!(has_seq(&a, &["--cookies", "c.txt", "--"]));
        assert!(!a.iter().any(|s| s.starts_with("temp:")));
    }

    #[test]
    fn formats_roundtrip() {
        for f in AudioFormat::ALL {
            assert_eq!(AudioFormat::from_key(f.key()), Some(f));
            assert_eq!(AudioFormat::from_index(f.index() as u64), Some(f));
        }
        assert_eq!(AudioFormat::from_index(AudioFormat::ALL.len() as u64), None);
    }

    #[test]
    fn missing_ytdlp_is_reported() {
        let o = DlOptions {
            ytdlp: "/definitely/not/yt-dlp".into(),
            ..opts()
        };
        let e = download(&o, "https://x", &Running::default(), |_| {}).unwrap_err();
        assert!(e.contains("not found"), "{e}");
    }
}
