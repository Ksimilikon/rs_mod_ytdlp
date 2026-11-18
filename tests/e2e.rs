//! the mod inside a real core, registered the way mobile frontends do it
//! (`mods_add_static`), driven through the `mods_*` C ABI like a frontend.
//!
//! `cargo test -p rs_mod_ytdlp -- --ignored` also downloads a real video
//! (needs yt-dlp and the network).

use api::{
    idx::{FFIIdxStatus, FFITracksList, idx_get_tracks},
    init,
    mem::idx_free_tracks_list,
    mods::*,
};
use std::{
    ffi::{CStr, CString},
    ptr,
    time::{Duration, Instant},
};

const ID: &CStr = c"ytdlp";

struct Core {
    dir: tempfile::TempDir,
}

/// one core per test process: both tests share it (`init` is once only).
fn core() -> Option<&'static Core> {
    static CORE: std::sync::OnceLock<Option<Core>> = std::sync::OnceLock::new();
    CORE.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        let sub = |n: &str| {
            let p = dir.path().join(n);
            std::fs::create_dir_all(&p).unwrap();
            CString::new(p.to_str().unwrap()).unwrap()
        };
        let (cache, config, data, music) = (sub("cache"), sub("config"), sub("data"), sub("music"));
        let st = unsafe {
            init(
                cache.as_ptr(),
                config.as_ptr(),
                data.as_ptr(),
                music.as_ptr(),
                true,
            )
        };
        if st as u32 != 0 {
            eprintln!("core init failed ({st:?}), no audio device? skipping");
            return None;
        }
        assert_eq!(
            unsafe { mods_add_static(ID.as_ptr(), &rs_mod_ytdlp::MOD) },
            ModStatus::Ok
        );
        Some(Core { dir })
    })
    .as_ref()
}

/// starts the mod; stops it when dropped, even if the test panics. Holds a
/// lock: the tests share one core and one mod, so they run one at a time.
struct Started(#[allow(dead_code)] std::sync::MutexGuard<'static, ()>);

impl Started {
    fn new() -> Self {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(unsafe { mods_start(ID.as_ptr()) }, ModStatus::Ok);
        Self(guard)
    }
}

impl Drop for Started {
    fn drop(&mut self) {
        unsafe { mods_stop(ID.as_ptr()) };
    }
}

fn tab() -> &'static Tab {
    let mut count = 0;
    assert_eq!(
        unsafe { mods_get_tab_count(ID.as_ptr(), &mut count) },
        ModStatus::Ok
    );
    assert_eq!(count, 1);
    let mut tab = ptr::null();
    assert_eq!(
        unsafe { mods_get_tab(ID.as_ptr(), 0, &mut tab) },
        ModStatus::Ok
    );
    unsafe { &*tab }
}

/// every element, boxes flattened.
fn flat(es: &'static [Element]) -> Vec<&'static Element> {
    es.iter()
        .flat_map(|e| std::iter::once(e).chain(flat(e.children())))
        .collect()
}

fn label(e: &Element) -> Option<String> {
    e.title().map(|t| t.to_string_lossy().into_owned())
}

/// the element of `kind` titled `prefix…`.
fn find(prefix: &str, kind: fn(&Element) -> bool) -> &'static Element {
    flat(tab().elements())
        .into_iter()
        .find(|e| kind(e) && label(e).is_some_and(|l| l.starts_with(prefix)))
        .unwrap_or_else(|| panic!("no element {prefix}"))
}

fn toggle(prefix: &str) -> &'static Element {
    find(prefix, |e| matches!(e, Element::Toggle { .. }))
}

fn is_on(prefix: &str) -> bool {
    matches!(*toggle(prefix), Element::Toggle { on: true, .. })
}

/// the frontend asks for a state / an index.
fn activate(e: &Element, target: u64) {
    assert_eq!(
        unsafe { mods_activate(ID.as_ptr(), e, target) },
        ModStatus::Ok
    );
}

fn status() -> String {
    // the status line is the text right after the Download / Cancel row.
    let es = tab().elements();
    let i = es
        .iter()
        .position(|e| {
            matches!(
                e,
                Element::Box {
                    horizontal: true,
                    ..
                }
            )
        })
        .unwrap();
    label(&es[i + 1]).unwrap()
}

fn press(title_prefix: &str) {
    activate(
        find(title_prefix, |e| matches!(e, Element::Button { .. })),
        0,
    );
}

fn type_url(text: &str) {
    let Element::Input { buf, cap, .. } = tab().elements()[1] else {
        panic!("url field")
    };
    let c = CString::new(text).unwrap();
    let b = c.as_bytes_with_nul();
    assert!(b.len() <= cap);
    unsafe { ptr::copy_nonoverlapping(b.as_ptr().cast(), buf, b.len()) };
}

fn library_len() -> usize {
    let mut list = FFITracksList {
        ptr: ptr::null_mut(),
        len: 0,
    };
    assert_eq!(unsafe { idx_get_tracks(&mut list) }, FFIIdxStatus::Ok);
    let n = list.len;
    unsafe { idx_free_tracks_list(list) };
    n
}

#[test]
fn lifecycle_and_settings() {
    let Some(core) = core() else { return };
    let started = Started::new();
    assert_eq!(status(), "Ready.", "core_get_dir is resolved");

    // a toggle takes the requested state, the select the chosen format;
    // both are saved to <config>/mods.
    assert!(is_on("Remove playlist"));
    activate(toggle("Remove playlist"), 0);
    assert!(!is_on("Remove playlist"));
    activate(toggle("Remove playlist"), 0); // asking again changes nothing
    assert!(!is_on("Remove playlist"));
    assert_eq!(
        unsafe { mods_activate(ID.as_ptr(), toggle("Remove playlist"), 2) },
        ModStatus::InvalidArg
    );
    let format = find("Format", |e| matches!(e, Element::Select { .. }));
    activate(format, 1);
    assert!(matches!(*format, Element::Select { selected: 1, .. }));
    let conf = core.dir.path().join("config/mods/rs_mod_ytdlp.conf");
    let saved = std::fs::read_to_string(&conf).unwrap();
    assert!(
        saved.contains("strip_playlist=false") && saved.contains("format=mp3"),
        "{saved}"
    );
    activate(format, 0);
    activate(toggle("Remove playlist"), 1);

    // bad input is rejected without touching yt-dlp.
    type_url("");
    press("Download");
    assert_eq!(status(), "Paste a link first.");
    type_url("https://www.youtube.com/playlist?list=PLx");
    press("Download");
    assert!(status().starts_with("Bad link"), "{}", status());

    // stop / start again: settings come back from the file.
    drop(started);
    let _started = Started::new();
    assert!(is_on("Remove playlist"));
    activate(toggle("Embed cover"), 1);
    drop(_started);
    let _started = Started::new();
    assert!(is_on("Embed cover"), "restored from the file");
    activate(toggle("Embed cover"), 0);
}

#[test]
#[ignore = "downloads from YouTube: needs yt-dlp and the network"]
fn downloads_into_the_library() {
    let Some(core) = core() else { return };
    let _started = Started::new();
    let before = library_len();
    // "Me at the zoo", 19 s; the junk parameters are stripped.
    type_url("https://youtu.be/jNQXAC9IVRw?si=tracking&t=3&list=PLjunk");
    press("Download");

    let start = Instant::now();
    let last = loop {
        let s = status();
        if s.starts_with("Done") || s.starts_with("Failed") {
            break s;
        }
        assert!(start.elapsed() < Duration::from_secs(180), "timed out: {s}");
        std::thread::sleep(Duration::from_millis(200));
    };
    eprintln!("status: {last}");
    assert!(
        last.starts_with("Done") && last.contains("added to the library"),
        "{last}"
    );
    assert_eq!(library_len(), before + 1);

    let dir = core.dir.path().join("music/YouTube");
    let files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    eprintln!("files: {files:?}");
    assert!(
        files
            .iter()
            .any(|f| f.to_string_lossy().ends_with("[jNQXAC9IVRw].m4a"))
    );
    // the Info element carries the path for the frontend.
    let info = tab()
        .elements()
        .iter()
        .find(|e| matches!(e, Element::Info { .. }))
        .unwrap();
    assert!(
        info.strings()
            .next()
            .unwrap()
            .to_string_lossy()
            .ends_with(".m4a")
    );
}

/// serves `body` as `audio/mp4` on 127.0.0.1 for any path; returns the port.
fn serve(body: Vec<u8>) -> u16 {
    use std::io::{BufRead, BufReader, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming().map_while(Result::ok) {
            let body = body.clone();
            std::thread::spawn(move || {
                let mut r = BufReader::new(&stream);
                let mut head = String::new();
                r.read_line(&mut head).unwrap_or(0);
                let mut range_from = 0;
                loop {
                    let mut l = String::new();
                    if r.read_line(&mut l).unwrap_or(0) == 0 || l == "\r\n" {
                        break;
                    }
                    if let Some(v) = l.to_ascii_lowercase().strip_prefix("range: bytes=") {
                        range_from = v
                            .split('-')
                            .next()
                            .and_then(|n| n.trim().parse().ok())
                            .unwrap_or(0);
                    }
                }
                let part = &body[range_from.min(body.len())..];
                let status = if range_from > 0 {
                    "206 Partial Content"
                } else {
                    "200 OK"
                };
                let mut w = &stream;
                let _ = write!(
                    w,
                    "HTTP/1.1 {status}\r\nContent-Type: audio/mp4\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                    part.len()
                );
                if !head.starts_with("HEAD") {
                    let _ = w.write_all(part);
                }
            });
        }
    });
    port
}

fn have(cmd: &str) -> bool {
    std::process::Command::new(cmd)
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
        || std::process::Command::new(cmd)
            .arg("-version")
            .output()
            .is_ok_and(|o| o.status.success())
}

/// the whole path without the internet: yt-dlp downloads a local m4a over
/// http (its generic extractor), the mod moves it to the music folder and
/// adds it to the library.
#[test]
fn downloads_a_local_file_into_the_library() {
    let Some(core) = core() else { return };
    if !have("yt-dlp") || !have("ffmpeg") {
        eprintln!("yt-dlp / ffmpeg not installed, skipping");
        return;
    }
    // a real m4a the core can read.
    let src = core.dir.path().join("src.m4a");
    let ok = std::process::Command::new("ffmpeg")
        .args([
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=3",
            "-c:a",
            "aac",
        ])
        .arg(&src)
        .status()
        .unwrap();
    assert!(ok.success());
    let port = serve(std::fs::read(&src).unwrap());

    let _started = Started::new();
    let before = library_len();
    type_url(&format!(
        "http://127.0.0.1:{port}/local-song.m4a?utm_source=x"
    ));
    press("Download");
    let start = Instant::now();
    let last = loop {
        let s = status();
        if s.starts_with("Done") || s.starts_with("Failed") {
            break s;
        }
        assert!(start.elapsed() < Duration::from_secs(60), "timed out: {s}");
        std::thread::sleep(Duration::from_millis(100));
    };
    eprintln!("status: {last}");
    assert!(
        last.starts_with("Done") && last.contains("added to the library"),
        "{last}"
    );
    assert_eq!(library_len(), before + 1);
    let info = tab()
        .elements()
        .iter()
        .find(|e| matches!(e, Element::Info { .. }))
        .unwrap();
    let path = info
        .strings()
        .next()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert!(
        path.starts_with(core.dir.path().join("music/YouTube").to_str().unwrap()),
        "{path}"
    );
    assert!(std::path::Path::new(&path).is_file());
    // nothing partial is left in the music folder.
    assert_eq!(
        std::fs::read_dir(core.dir.path().join("music/YouTube"))
            .unwrap()
            .count(),
        1
    );
}

/// the text field whose hint starts with `prefix`.
fn input(prefix: &str) -> &'static Element {
    flat(tab().elements())
        .into_iter()
        .find(|e| match **e {
            Element::Input { hint, .. } => unsafe { CStr::from_ptr(hint) }
                .to_string_lossy()
                .starts_with(prefix),
            _ => false,
        })
        .unwrap_or_else(|| panic!("no input {prefix}"))
}

fn type_into(prefix: &str, text: &str) {
    let Element::Input { buf, cap, .. } = *input(prefix) else {
        unreachable!()
    };
    let c = CString::new(text).unwrap();
    let b = c.as_bytes_with_nul();
    assert!(b.len() <= cap);
    unsafe { ptr::copy_nonoverlapping(b.as_ptr().cast(), buf, b.len()) };
}

fn results() -> (&'static Element, Vec<String>) {
    let list = flat(tab().elements())
        .into_iter()
        .find(|e| {
            matches!(
                e,
                Element::List {
                    on_select: Some(_),
                    ..
                }
            )
        })
        .expect("results list");
    (
        list,
        list.strings()
            .map(|c| c.to_string_lossy().into_owned())
            .collect(),
    )
}

/// waits until the status starts with one of `done`.
fn wait_status(done: &[&str], secs: u64) -> String {
    let start = Instant::now();
    loop {
        let s = status();
        if done.iter().any(|d| s.starts_with(d)) {
            return s;
        }
        assert!(
            start.elapsed() < Duration::from_secs(secs),
            "timed out: {s}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn files_in(dir: &std::path::Path) -> Vec<String> {
    let mut v: Vec<_> = std::fs::read_dir(dir)
        .map(|d| {
            d.map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

/// search → listen → save, without the internet: a fake yt-dlp answers the
/// search itself and hands downloads to the real yt-dlp with a local link.
#[cfg(unix)]
#[test]
fn search_listen_save() {
    use std::os::unix::fs::PermissionsExt;
    let Some(core) = core() else { return };
    if !have("yt-dlp") || !have("ffmpeg") {
        eprintln!("yt-dlp / ffmpeg not installed, skipping");
        return;
    }
    let src = core.dir.path().join("search-src.m4a");
    let ok = std::process::Command::new("ffmpeg")
        .args([
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=330:duration=2",
            "-c:a",
            "aac",
        ])
        .arg(&src)
        .status()
        .unwrap();
    assert!(ok.success());
    let port = serve(std::fs::read(&src).unwrap());
    let fake = core.dir.path().join("fake-yt-dlp");
    std::fs::write(
        &fake,
        format!(
            "#!/bin/bash\n\
             last=\"${{!#}}\"\n\
             for a in \"$@\"; do [ \"$a\" = --flat-playlist ] && {{ case \"$last\" in \
             scsearch*) printf '1250206453\\037355.776\\037Cloud track\\037Some DJ\\037https://soundcloud.com/some-dj/cloud-track\\n2\\0371\\037no link\\037x\\037NA\\n';; \
             *) printf 'aaaaaaaaaaa\\037185\\037First song\\037Chan\\nnot a result\\nbbbbbbbbbbb\\037NA\\037Live thing\\037NA\\n';; \
             esac; exit 0; }}; done\n\
             exec yt-dlp \"${{@:1:$#-1}}\" \"http://127.0.0.1:{port}/song-${{last##*[=/]}}.m4a\"\n"
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();

    let _started = Started::new();
    /// back to the settings the other tests expect, even on a panic.
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            type_into("yt-dlp executable", "");
            type_into("subfolder", "YouTube");
            press("Save settings");
        }
    }
    let _restore = Restore;
    type_into("yt-dlp executable", fake.to_str().unwrap());
    type_into("subfolder", "Search");
    let music = core.dir.path().join("music/Search");
    let preview = core.dir.path().join("cache/rs_mod_ytdlp/preview");
    let before = library_len();

    type_into("search query", "");
    press("Search");
    assert_eq!(status(), "Type what to search for first.");
    press("Save to library");
    assert_eq!(status(), "Pick a result first.");

    type_into("search query", "some song");
    press("Search");
    let s = wait_status(&["Found", "Search failed", "Nothing"], 30);
    assert_eq!(s, "Found 2: pick one to listen to it.");
    let (list, lines) = results();
    assert_eq!(
        lines,
        ["· [3:05] First song — Chan", "· [--:--] Live thing"]
    );

    // pick → the audio goes to the cache and plays; nothing in the music
    // folder yet.
    activate(list, 0);
    let s = wait_status(&["Playing", "Can't"], 60);
    assert!(s.starts_with("Playing: First song [3:05]"), "{s}");
    assert_eq!(results().1[0], "▶ [3:05] First song — Chan");
    assert_eq!(files_in(&preview).len(), 1, "{:?}", files_in(&preview));
    assert!(files_in(&music).is_empty());

    // save → moved to the music folder and into the library.
    press("Save to library");
    let s = status();
    assert_eq!(s, "Saved: First song, added to the library");
    assert_eq!(library_len(), before + 1);
    assert_eq!(files_in(&music).len(), 1);
    assert!(
        files_in(&music)[0].ends_with(".m4a"),
        "only audio: {:?}",
        files_in(&music)
    );
    assert!(files_in(&preview).is_empty());
    press("Save to library");
    assert_eq!(status(), "Already saved: First song");

    // pick another and save at once: saved as soon as it's downloaded.
    activate(list, 1);
    press("Save to library");
    let s = wait_status(&["Saved", "Not saved", "Can't load"], 60);
    assert_eq!(s, "Saved: Live thing, added to the library");
    assert_eq!(library_len(), before + 2);
    assert_eq!(files_in(&music).len(), 2);
    let lines = results().1;
    assert_eq!(
        lines,
        ["✓ [3:05] First song — Chan", "▶ [--:--] Live thing"]
    );

    press("Stop listening");
    assert_eq!(status(), "Stopped.");
    assert_eq!(results().1[1], "✓ [--:--] Live thing");
    assert!(files_in(&preview).is_empty());

    // the same on SoundCloud: numeric ids, the page link from the listing.
    let source = find("Search on", |e| matches!(e, Element::Select { .. }));
    activate(source, 1);
    assert!(matches!(*source, Element::Select { selected: 1, .. }));
    press("Search");
    let s = wait_status(&["Found", "Search failed", "Nothing"], 30);
    assert_eq!(s, "Found 1: pick one to listen to it.");
    let (list, lines) = results();
    assert_eq!(lines, ["· [5:56] Cloud track — Some DJ"]);
    activate(list, 0);
    let s = wait_status(&["Playing", "Can't"], 60);
    assert!(s.starts_with("Playing: Cloud track [5:56]"), "{s}");
    press("Save to library");
    assert_eq!(status(), "Saved: Cloud track, added to the library");
    assert_eq!(library_len(), before + 3);
    assert!(
        files_in(&music).iter().any(|f| f.contains("cloud-track")),
        "{:?}",
        files_in(&music)
    );
    press("Stop listening");
    activate(source, 0);
}

#[test]
#[ignore = "searches SoundCloud: needs yt-dlp and the network"]
fn real_soundcloud_search_and_preview() {
    let Some(core) = core() else { return };
    let _started = Started::new();
    let source = find("Search on", |e| matches!(e, Element::Select { .. }));
    activate(source, 1);
    type_into("search query", "lofi beats");
    press("Search");
    let s = wait_status(&["Found", "Search failed", "Nothing"], 60);
    assert!(s.starts_with("Found"), "{s}");
    let (list, lines) = results();
    eprintln!("{lines:#?}");
    // a short track: the preview is downloaded whole.
    let i = lines
        .iter()
        .position(|l| {
            let d = l.split(['[', ']']).nth(1).unwrap_or("");
            d.len() == 4 && d < "4:00"
        })
        .unwrap_or(0);
    activate(list, i as u64);
    let s = wait_status(&["Playing", "Can't"], 180);
    activate(source, 0);
    assert!(s.starts_with("Playing"), "{s}");
    let preview = files_in(&core.dir.path().join("cache/rs_mod_ytdlp/preview"));
    eprintln!("preview: {preview:?}");
    assert!(
        preview.len() == 1 && (preview[0].ends_with(".m4a") || preview[0].ends_with(".mp3")),
        "audio only: {preview:?}"
    );
    press("Stop listening");
}

#[test]
#[ignore = "searches YouTube: needs yt-dlp and the network"]
fn real_search_and_preview() {
    let Some(core) = core() else { return };
    let _started = Started::new();
    type_into("search query", "me at the zoo jawed");
    press("Search");
    let s = wait_status(&["Found", "Search failed", "Nothing"], 60);
    assert!(s.starts_with("Found"), "{s}");
    let (list, lines) = results();
    eprintln!("{lines:#?}");
    let i = lines
        .iter()
        .position(|l| l.contains("[0:19] Me at the zoo"))
        .expect("the zoo video, 19 s");
    activate(list, i as u64);
    let s = wait_status(&["Playing", "Can't"], 120);
    assert!(s.starts_with("Playing: Me at the zoo"), "{s}");
    let preview = files_in(&core.dir.path().join("cache/rs_mod_ytdlp/preview"));
    eprintln!("preview: {preview:?}");
    assert!(
        preview.len() == 1 && preview[0].ends_with("[jNQXAC9IVRw].m4a"),
        "audio only: {preview:?}"
    );
    press("Stop listening");
}
