//! The mod's tab. Its memory is owned here and read by the frontend at any
//! time without synchronization (see `mod_sys::ui`), so:
//! - every array the frontend sees is allocated once and never moved;
//! - a changing string is swapped in with one atomic pointer store; the old
//!   string is kept for a while (a frontend may be reading it right now) and
//!   freed only after [`RETAIN`] more swaps, or when the tab is dropped;
//! - list lengths change with one atomic store, and every slot of a list
//!   always holds a valid string, so any length the frontend sees is safe.

use crate::{search, settings::Settings, ytdlp::AudioFormat};
use mod_sys::{Element, Tab, abi::ActionFn};
use std::{
    collections::VecDeque,
    ffi::{CString, c_char, c_void},
    ptr,
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering},
    },
};

/// lines kept in the "recent" list.
pub const HISTORY: usize = 20;
/// swapped-out strings kept alive before they are freed.
const RETAIN: usize = 512;
const URL_CAP: usize = 8192;
const PATH_CAP: usize = 1024;

/// what a button / list does; passed to the action as `ctx`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum Act {
    Download = 1,
    Cancel,
    StripPlaylist,
    StripTracking,
    StripTime,
    StripOther,
    Canonical,
    Format,
    EmbedMetadata,
    EmbedThumbnail,
    AddToLibrary,
    Save,
    ClearHistory,
    Search,
    /// a result picked in the list (`target` = its index).
    Pick,
    StopListening,
    SaveResult,
    SearchCount,
    SearchSource,
}

impl Act {
    const ALL: [Self; 19] = [
        Self::Download,
        Self::Cancel,
        Self::StripPlaylist,
        Self::StripTracking,
        Self::StripTime,
        Self::StripOther,
        Self::Canonical,
        Self::Format,
        Self::EmbedMetadata,
        Self::EmbedThumbnail,
        Self::AddToLibrary,
        Self::Save,
        Self::ClearHistory,
        Self::Search,
        Self::Pick,
        Self::StopListening,
        Self::SaveResult,
        Self::SearchCount,
        Self::SearchSource,
    ];

    pub fn from_ctx(ctx: *mut c_void) -> Option<Self> {
        Self::ALL.into_iter().find(|a| *a as usize == ctx as usize)
    }

    fn ctx(self) -> *mut c_void {
        self as usize as *mut c_void
    }
}

/// a heap array with a fixed address, accessed only through raw pointers
/// (the frontend reads it concurrently, so no `&mut` to it ever exists
/// after it is published).
struct Fixed<T> {
    ptr: *mut T,
    len: usize,
}

impl<T> Fixed<T> {
    fn new(v: Vec<T>) -> Self {
        let b = v.into_boxed_slice();
        let len = b.len();
        Self {
            ptr: Box::into_raw(b).cast(),
            len,
        }
    }

    fn at(&self, i: usize) -> *mut T {
        assert!(i < self.len);
        unsafe { self.ptr.add(i) }
    }
}

impl<T> Drop for Fixed<T> {
    fn drop(&mut self) {
        drop(unsafe { Box::from_raw(ptr::slice_from_raw_parts_mut(self.ptr, self.len)) });
    }
}

/// a text field's buffer: the frontend writes into it, we read on demand.
struct Input(Fixed<c_char>);

impl Input {
    fn new(cap: usize, initial: &str) -> Self {
        let mut v = vec![0 as c_char; cap];
        // the last byte stays 0: the buffer is always terminated.
        let bytes = initial.as_bytes();
        let n = floor_char_boundary(initial, bytes.len().min(cap - 1));
        for (d, &s) in v.iter_mut().zip(&bytes[..n]) {
            *d = s as c_char;
        }
        Self(Fixed::new(v))
    }

    fn read(&self) -> String {
        let mut out = Vec::new();
        for i in 0..self.0.len {
            let b = unsafe { self.0.at(i).read_volatile() } as u8;
            if b == 0 {
                break;
            }
            out.push(b);
        }
        String::from_utf8_lossy(&out).trim().to_owned()
    }
}

fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn cstring(s: &str) -> CString {
    CString::new(s.replace('\0', " ")).expect("no NUL left")
}

/// where a changing string lives: the address of a `*const c_char` field
/// inside a published element / list slot.
#[derive(Clone, Copy)]
struct StrField(*mut *const c_char);

/// the address of a list's `len` / a select's `selected` field.
#[derive(Clone, Copy)]
struct LenField(*mut usize);

/// the address of a toggle's `on` field.
#[derive(Clone, Copy)]
struct BoolField(*mut bool);

/// the mutable side of the tab, under one lock.
struct Inner {
    retired: VecDeque<*mut c_char>,
    history: VecDeque<String>,
    files: VecDeque<String>,
}

pub struct Ui {
    tab: Box<Tab>,
    // kept alive for the pointers in `tab` / elements; never read directly.
    _elements: Fixed<Element>,
    _actions_row: Fixed<Element>,
    _search_row: Fixed<Element>,
    _count_items: Fixed<*const c_char>,
    _source_items: Fixed<*const c_char>,
    _clean_box: Fixed<Element>,
    _options_box: Fixed<Element>,
    _format_items: Fixed<*const c_char>,
    _labels: Vec<CString>,
    history_items: Fixed<*const c_char>,
    files_items: Fixed<*const c_char>,
    results_items: Fixed<*const c_char>,

    url: Input,
    query: Input,
    ytdlp: Input,
    ffmpeg: Input,
    subfolder: Input,
    extra: Input,

    status: StrField,
    clean_toggles: [BoolField; 5],
    format: LenField,
    option_toggles: [BoolField; 3],
    search_count: LenField,
    search_source: LenField,
    history_len: LenField,
    results_len: LenField,
    files_len: LenField,
    /// every field holding a string we allocated (freed on drop).
    owned: Vec<StrField>,
    inner: Mutex<Inner>,
}

// all shared mutation goes through atomics / the `inner` lock; raw pointers
// point into memory this struct owns.
unsafe impl Send for Ui {}
unsafe impl Sync for Ui {}

fn button(labels: &mut Vec<CString>, title: &str, action: ActionFn, act: Act) -> Element {
    let t = cstring(title);
    let e = Element::Button {
        title: t.as_ptr(),
        action,
        ctx: act.ctx(),
    };
    labels.push(t);
    e
}

fn text(labels: &mut Vec<CString>, s: &str) -> Element {
    let t = cstring(s);
    let e = Element::Text { text: t.as_ptr() };
    labels.push(t);
    e
}

/// a switch; `on` is set from the settings right after construction.
fn toggle(labels: &mut Vec<CString>, title: &str, action: ActionFn, act: Act) -> Element {
    let t = cstring(title);
    let e = Element::Toggle {
        title: t.as_ptr(),
        on: false,
        action,
        ctx: act.ctx(),
    };
    labels.push(t);
    e
}

/// the field of a text / button title. Only before the element is
/// published (creates a `&mut` to it).
unsafe fn str_field(e: *mut Element) -> StrField {
    match unsafe { &mut *e } {
        Element::Text { text } => StrField(text),
        Element::Button { title, .. } => StrField(title),
        _ => unreachable!("not a text element"),
    }
}

unsafe fn len_field(e: *mut Element) -> LenField {
    match unsafe { &mut *e } {
        Element::List { len, .. } | Element::Info { len, .. } => LenField(len),
        Element::Select { selected, .. } => LenField(selected),
        _ => unreachable!("not a list"),
    }
}

unsafe fn bool_field(e: *mut Element) -> BoolField {
    match unsafe { &mut *e } {
        Element::Toggle { on, .. } => BoolField(on),
        _ => unreachable!("not a toggle"),
    }
}

/// indices in the top-level element array.
mod idx {
    pub const STATUS: usize = 3;
    pub const SOURCE: usize = 5;
    #[cfg(test)]
    pub const QUERY: usize = 6;
    pub const RESULTS: usize = 9;
    pub const HISTORY: usize = 21;
    pub const FILES: usize = 22;
}

impl Ui {
    /// builds the tab; `action` is the mod's single action dispatcher.
    pub fn new(s: &Settings, action: ActionFn) -> Box<Self> {
        let mut labels = Vec::new();
        let actions_row = Fixed::new(vec![
            button(&mut labels, "Download", action, Act::Download),
            button(&mut labels, "Cancel", action, Act::Cancel),
        ]);
        let search_row = Fixed::new(vec![
            button(&mut labels, "Search", action, Act::Search),
            button(&mut labels, "Stop listening", action, Act::StopListening),
            button(&mut labels, "Save to library", action, Act::SaveResult),
        ]);
        let l = &mut labels;
        let clean_box = Fixed::new(vec![
            toggle(
                l,
                "Remove playlist position (list, index, start_radio)",
                action,
                Act::StripPlaylist,
            ),
            toggle(
                l,
                "Remove share tracking (si, feature, pp, utm_*)",
                action,
                Act::StripTracking,
            ),
            toggle(l, "Remove start time (t, start)", action, Act::StripTime),
            toggle(l, "Remove any other parameters", action, Act::StripOther),
            toggle(
                l,
                "Rewrite to youtube.com/watch?v=ID (shorts, youtu.be, music)",
                action,
                Act::Canonical,
            ),
        ]);
        let format_items = Fixed::new(
            AudioFormat::ALL
                .iter()
                .map(|f| {
                    let c = cstring(f.label());
                    let p = c.as_ptr();
                    l.push(c);
                    p
                })
                .collect(),
        );
        let count_items = Fixed::new(
            search::COUNTS
                .iter()
                .map(|n| {
                    let c = cstring(&n.to_string());
                    let p = c.as_ptr();
                    l.push(c);
                    p
                })
                .collect(),
        );
        let count_title = cstring("Search results");
        let format_title = cstring("Format");
        let options_box = Fixed::new(vec![
            Element::Select {
                title: format_title.as_ptr(),
                items: format_items.ptr,
                len: format_items.len,
                selected: 0,
                on_select: action,
                ctx: Act::Format.ctx(),
            },
            toggle(
                l,
                "Embed title / artist (ffmpeg)",
                action,
                Act::EmbedMetadata,
            ),
            toggle(l, "Embed cover (ffmpeg)", action, Act::EmbedThumbnail),
            toggle(l, "Add to the library", action, Act::AddToLibrary),
            Element::Select {
                title: count_title.as_ptr(),
                items: count_items.ptr,
                len: count_items.len,
                selected: 0,
                on_select: action,
                ctx: Act::SearchCount.ctx(),
            },
        ]);
        l.push(format_title);
        l.push(count_title);
        let source_items = Fixed::new(
            search::Source::ALL
                .iter()
                .map(|s| {
                    let c = cstring(s.label());
                    let p = c.as_ptr();
                    l.push(c);
                    p
                })
                .collect(),
        );
        let source_title = cstring("Search on");
        let source_title_ptr = source_title.as_ptr();
        l.push(source_title);
        let empty_slots = |n| {
            Fixed::new(
                (0..n)
                    .map(|_| cstring("").into_raw().cast_const())
                    .collect(),
            )
        };
        let history_items = empty_slots(HISTORY);
        let files_items = empty_slots(HISTORY);
        let results_items = empty_slots(search::MAX);

        let url = Input::new(URL_CAP, "");
        let query = Input::new(PATH_CAP, "");
        let ytdlp = Input::new(PATH_CAP, &s.ytdlp_path);
        let ffmpeg = Input::new(PATH_CAP, &s.ffmpeg_path);
        let subfolder = Input::new(PATH_CAP, &s.subfolder);
        let extra = Input::new(PATH_CAP, &s.extra_args);

        let input = |labels: &mut Vec<CString>, hint: &str, i: &Input| {
            let h = cstring(hint);
            let e = Element::Input {
                hint: h.as_ptr(),
                buf: i.0.ptr,
                cap: i.0.len,
            };
            labels.push(h);
            e
        };
        let elements = Fixed::new(vec![
            text(
                &mut labels,
                "Download audio with yt-dlp. Paste one or more links (space separated) and press Download.",
            ),
            input(&mut labels, "https://www.youtube.com/watch?v=...", &url),
            Element::Box {
                children: actions_row.ptr,
                len: actions_row.len,
                horizontal: true,
            },
            Element::Text {
                text: cstring("Ready.").into_raw(),
            }, // STATUS
            text(&mut labels, "Search YouTube / SoundCloud"),
            Element::Select {
                title: source_title_ptr,
                items: source_items.ptr,
                len: source_items.len,
                selected: 0,
                on_select: action,
                ctx: Act::SearchSource.ctx(),
            }, // SOURCE
            input(&mut labels, "search query, e.g. artist - song", &query), // QUERY
            Element::Box {
                children: search_row.ptr,
                len: search_row.len,
                horizontal: true,
            },
            text(
                &mut labels,
                "Pick a result to listen to it (pick it again: pause / resume). Save to library keeps the audio.",
            ),
            // RESULTS: "[duration] title — channel", a mark in front:
            // … loading, ▶ playing, ✓ saved, ✗ failed.
            Element::List {
                items: results_items.ptr,
                len: 0,
                on_select: Some(action),
                ctx: Act::Pick.ctx(),
            },
            text(&mut labels, "Link cleanup"),
            Element::Box {
                children: clean_box.ptr,
                len: clean_box.len,
                horizontal: false,
            },
            text(&mut labels, "Download options"),
            Element::Box {
                children: options_box.ptr,
                len: options_box.len,
                horizontal: false,
            },
            input(
                &mut labels,
                "yt-dlp executable (empty: yt-dlp from PATH)",
                &ytdlp,
            ),
            input(
                &mut labels,
                "ffmpeg executable or folder (empty: from PATH)",
                &ffmpeg,
            ),
            input(
                &mut labels,
                "subfolder in the music folder (empty: music folder itself)",
                &subfolder,
            ),
            input(
                &mut labels,
                "extra yt-dlp arguments, e.g. --cookies cookies.txt",
                &extra,
            ),
            button(&mut labels, "Save settings", action, Act::Save),
            text(&mut labels, "Recent"),
            button(&mut labels, "Clear recent", action, Act::ClearHistory),
            Element::List {
                items: history_items.ptr,
                len: 0,
                on_select: None,
                ctx: ptr::null_mut(),
            }, // HISTORY
            // for frontends: paths of the latest downloaded files, newest
            // first (e.g. to offer playing them with `engine_add_track_path`).
            Element::Info {
                strings: files_items.ptr,
                len: 0,
            }, // FILES
        ]);
        let title = cstring("YouTube");
        let tab = Box::new(Tab {
            title: title.as_ptr(),
            elements: elements.ptr,
            len: elements.len,
        });
        labels.push(title);

        // field addresses, taken before the tab is published.
        let status = unsafe { str_field(elements.at(idx::STATUS)) };
        let clean_toggles = std::array::from_fn(|i| unsafe { bool_field(clean_box.at(i)) });
        let format = unsafe { len_field(options_box.at(0)) };
        let option_toggles = std::array::from_fn(|i| unsafe { bool_field(options_box.at(i + 1)) });
        let search_count = unsafe { len_field(options_box.at(4)) };
        let results_len = unsafe { len_field(elements.at(idx::RESULTS)) };
        let search_source = unsafe { len_field(elements.at(idx::SOURCE)) };
        let history_len = unsafe { len_field(elements.at(idx::HISTORY)) };
        let files_len = unsafe { len_field(elements.at(idx::FILES)) };

        let mut owned = vec![status];
        owned.extend((0..HISTORY).map(|i| StrField(history_items.at(i))));
        owned.extend((0..HISTORY).map(|i| StrField(files_items.at(i))));
        owned.extend((0..search::MAX).map(|i| StrField(results_items.at(i))));

        let ui = Box::new(Self {
            tab,
            _elements: elements,
            _actions_row: actions_row,
            _search_row: search_row,
            _count_items: count_items,
            _source_items: source_items,
            _clean_box: clean_box,
            _options_box: options_box,
            _format_items: format_items,
            _labels: labels,
            history_items,
            files_items,
            results_items,
            url,
            query,
            ytdlp,
            ffmpeg,
            subfolder,
            extra,
            status,
            clean_toggles,
            format,
            option_toggles,
            search_count,
            search_source,
            history_len,
            results_len,
            files_len,
            owned,
            inner: Mutex::new(Inner {
                retired: VecDeque::new(),
                history: VecDeque::new(),
                files: VecDeque::new(),
            }),
        });
        ui.show_settings(s);
        ui
    }

    /// the tab for `Host::add_tab`; valid while `self` lives.
    pub fn tab(&self) -> *const Tab {
        &*self.tab
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// swaps the string of `field`; the old one is retired, not freed.
    fn set(&self, inner: &mut Inner, field: StrField, s: &str) {
        let new = cstring(s).into_raw();
        let old = unsafe { AtomicPtr::from_ptr(field.0.cast::<*mut c_char>()) }
            .swap(new, Ordering::AcqRel);
        inner.retired.push_back(old);
        while inner.retired.len() > RETAIN {
            if let Some(p) = inner.retired.pop_front() {
                drop(unsafe { CString::from_raw(p) });
            }
        }
    }

    fn set_len(field: LenField, len: usize) {
        unsafe { AtomicUsize::from_ptr(field.0) }.store(len, Ordering::Release);
    }

    pub fn set_status(&self, s: &str) {
        let mut g = self.lock();
        self.set(&mut g, self.status, s);
    }

    /// toggle states and the selected format from the settings (the mod
    /// owns them: the frontend only asks, see `Element::Toggle`).
    pub fn show_settings(&self, s: &Settings) {
        let c = s.clean;
        let clean = [c.playlist, c.tracking, c.time, c.other, c.canonical];
        let options = [s.embed_metadata, s.embed_thumbnail, s.add_to_library];
        for (field, on) in self
            .clean_toggles
            .iter()
            .zip(clean)
            .chain(self.option_toggles.iter().zip(options))
        {
            unsafe { AtomicBool::from_ptr(field.0) }.store(on, Ordering::Release);
        }
        Self::set_len(self.format, s.format.index());
        Self::set_len(self.search_source, s.search_source.index());
        Self::set_len(
            self.search_count,
            search::COUNTS
                .iter()
                .position(|&n| n == s.search_count)
                .unwrap_or(usize::MAX),
        );
    }

    /// what the user typed into the search field.
    pub fn query_text(&self) -> String {
        self.query.read()
    }

    /// replaces the search results list (at most [`search::MAX`] lines).
    pub fn set_results(&self, lines: &[String]) {
        let mut g = self.lock();
        self.fill(
            &mut g,
            &self.results_items,
            self.results_len,
            &lines[..lines.len().min(search::MAX)],
        );
    }

    /// what the user typed into the url field.
    pub fn url_text(&self) -> String {
        self.url.read()
    }

    /// copies the text fields into `s`.
    pub fn read_inputs(&self, s: &mut Settings) {
        s.ytdlp_path = self.ytdlp.read();
        s.ffmpeg_path = self.ffmpeg.read();
        s.subfolder = self.subfolder.read();
        s.extra_args = self.extra.read();
    }

    /// a line on top of "recent"; `file` also goes to the `Info` element.
    pub fn push_history(&self, line: &str, file: Option<&str>) {
        let mut g = self.lock();
        g.history.push_front(line.to_owned());
        g.history.truncate(HISTORY);
        if let Some(f) = file {
            g.files.push_front(f.to_owned());
            g.files.truncate(HISTORY);
        }
        self.show_lists(&mut g);
    }

    pub fn clear_history(&self) {
        let mut g = self.lock();
        g.history.clear();
        g.files.clear();
        self.show_lists(&mut g);
    }

    fn show_lists(&self, g: &mut Inner) {
        let history: Vec<String> = g.history.iter().cloned().collect();
        let files: Vec<String> = g.files.iter().cloned().collect();
        self.fill(g, &self.history_items, self.history_len, &history);
        self.fill(g, &self.files_items, self.files_len, &files);
    }

    fn fill(&self, g: &mut Inner, slots: &Fixed<*const c_char>, len: LenField, lines: &[String]) {
        // shrink first, then rewrite, then grow: the frontend never sees a
        // slot past `len` it shouldn't (all slots are valid anyway).
        Self::set_len(len, len_now(len).min(lines.len()));
        for (i, l) in lines.iter().enumerate() {
            self.set(g, StrField(slots.at(i)), l);
        }
        Self::set_len(len, lines.len());
    }
}

fn len_now(f: LenField) -> usize {
    unsafe { AtomicUsize::from_ptr(f.0) }.load(Ordering::Acquire)
}

impl Drop for Ui {
    fn drop(&mut self) {
        // the host has dropped its reference to the tab (the mod is being
        // stopped): free every string we allocated.
        for f in &self.owned {
            let p = unsafe { *f.0 }.cast_mut();
            if !p.is_null() {
                drop(unsafe { CString::from_raw(p) });
            }
        }
        for p in self.lock().retired.drain(..) {
            drop(unsafe { CString::from_raw(p) });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    unsafe extern "C" fn noop(_: *mut c_void, _: u64) -> u32 {
        0
    }

    fn flat(es: &[Element]) -> Vec<&Element> {
        es.iter()
            .flat_map(|e| std::iter::once(e).chain(flat(e.children())))
            .collect()
    }

    fn texts(es: &[Element]) -> Vec<String> {
        flat(es)
            .iter()
            .filter_map(|e| e.title())
            .map(|t| t.to_string_lossy().into_owned())
            .collect()
    }

    /// `on` of the toggle titled `prefix…`.
    fn toggle_on(es: &[Element], prefix: &str) -> bool {
        flat(es)
            .into_iter()
            .find_map(|e| match *e {
                Element::Toggle { on, .. }
                    if e.title().unwrap().to_string_lossy().starts_with(prefix) =>
                {
                    Some(on)
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("no toggle {prefix}"))
    }

    fn selected_format(es: &[Element]) -> (usize, Vec<String>) {
        select(es, "Format")
    }

    /// the select titled `title`: (selected, items).
    fn select(es: &[Element], title: &str) -> (usize, Vec<String>) {
        flat(es)
            .into_iter()
            .find_map(|e| match *e {
                Element::Select { selected, .. }
                    if e.title().unwrap().to_string_lossy() == title =>
                {
                    Some((
                        selected,
                        e.strings()
                            .map(|c| c.to_string_lossy().into_owned())
                            .collect(),
                    ))
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("no select {title}"))
    }

    #[test]
    fn tab_reflects_state() {
        let mut s = Settings::default();
        let ui = Ui::new(&s, noop);
        let tab = unsafe { &*ui.tab() };
        assert_eq!(tab.title(), Some(c"YouTube"));
        let es = tab.elements();
        assert!(texts(es).contains(&"Ready.".to_owned()));
        assert!(toggle_on(es, "Remove playlist"));
        assert!(!toggle_on(es, "Embed cover"));
        let (sel, items) = selected_format(es);
        assert_eq!(items.len(), AudioFormat::ALL.len());
        assert_eq!(items[sel], AudioFormat::M4a.label());
        assert_eq!(
            select(es, "Search on"),
            (0, vec!["YouTube".into(), "SoundCloud".into()])
        );
        assert_eq!(select(es, "Search results").0, 1);

        s.clean.playlist = false;
        s.search_source = search::Source::SoundCloud;
        s.embed_thumbnail = true;
        s.format = AudioFormat::Mp3;
        ui.show_settings(&s);
        ui.set_status("working");
        assert!(!toggle_on(es, "Remove playlist"));
        assert!(toggle_on(es, "Embed cover"));
        let (sel, items) = selected_format(es);
        assert_eq!(items[sel], AudioFormat::Mp3.label());
        assert_eq!(select(es, "Search on").0, 1);
        assert!(texts(es).contains(&"working".to_owned()));

        // history: newest first, capped, mirrored into `Info`.
        for i in 0..HISTORY + 5 {
            ui.push_history(
                &format!("line {i}"),
                (i % 2 == 0).then(|| format!("/f{i}")).as_deref(),
            );
        }
        let list = &tab.elements()[idx::HISTORY];
        let lines: Vec<_> = list
            .strings()
            .map(|c| c.to_string_lossy().into_owned())
            .collect();
        assert_eq!(lines.len(), HISTORY);
        assert_eq!(lines[0], format!("line {}", HISTORY + 4));
        let info = &tab.elements()[idx::FILES];
        assert_eq!(info.strings().next(), Some(c"/f24"));
        ui.clear_history();
        assert_eq!(tab.elements()[idx::HISTORY].strings().count(), 0);

        // search results: replaced as a whole, capped to the slots.
        let lines: Vec<String> = (0..search::MAX + 3).map(|i| format!("hit {i}")).collect();
        ui.set_results(&lines);
        assert_eq!(tab.elements()[idx::RESULTS].strings().count(), search::MAX);
        ui.set_results(&lines[..2]);
        let got: Vec<_> = tab.elements()[idx::RESULTS]
            .strings()
            .map(|c| c.to_string_lossy().into_owned())
            .collect();
        assert_eq!(got, ["hit 0", "hit 1"]);
        assert!(matches!(
            tab.elements()[idx::RESULTS],
            Element::List {
                on_select: Some(_),
                ..
            }
        ));
    }

    #[test]
    fn inputs_roundtrip() {
        let s = Settings {
            ytdlp_path: "/usr/bin/yt-dlp".into(),
            extra_args: "-v".into(),
            ..Settings::default()
        };
        let ui = Ui::new(&s, noop);
        // a frontend typing into the url field.
        let Element::Input { buf, cap, .. } = unsafe { &*ui.tab() }.elements()[1] else {
            panic!()
        };
        let typed = b" https://youtu.be/x \0";
        assert!(typed.len() <= cap);
        unsafe { ptr::copy_nonoverlapping(typed.as_ptr().cast(), buf, typed.len()) };
        assert_eq!(ui.url_text(), "https://youtu.be/x");
        let Element::Input { buf, .. } = unsafe { &*ui.tab() }.elements()[idx::QUERY] else {
            panic!()
        };
        unsafe { ptr::copy_nonoverlapping(c"lofi beats".as_ptr(), buf, 11) };
        assert_eq!(ui.query_text(), "lofi beats");
        let mut back = Settings::default();
        ui.read_inputs(&mut back);
        assert_eq!(back.ytdlp_path, "/usr/bin/yt-dlp");
        assert_eq!(back.extra_args, "-v");
        assert_eq!(back.subfolder, "YouTube");
    }

    #[test]
    fn acts_roundtrip() {
        for a in Act::ALL {
            assert_eq!(Act::from_ctx(a.ctx()), Some(a));
        }
        assert_eq!(Act::from_ctx(ptr::null_mut()), None);
    }
}
