//! `rs_mod_ytdlp` — a dream_player mod that downloads audio from YouTube
//! (and other sites yt-dlp supports) with [yt-dlp] and puts it into the
//! library.
//!
//! - one tab, "YouTube": a link field, Download / Cancel, link cleanup
//!   flags ([`clean`]), format and other options, a list of recent results;
//! - links are cleaned when Download is pressed and queued; one worker
//!   thread runs yt-dlp for them one by one;
//! - files go to `<music>/<subfolder>` (partial files to `<cache>`), then
//!   to the library through `idx_add_tracks`;
//! - search ([`search`], `browse`): a query lists YouTube videos or
//!   SoundCloud tracks with their
//!   durations; picking one downloads its audio to the cache and plays it,
//!   "Save to library" moves it to the music folder. A second thread runs
//!   searches / previews, so they never wait for the download queue;
//! - settings live in `<config>/mods/rs_mod_ytdlp.conf`.
//!
//! needs `core_get_dir` in the core and a yt-dlp executable (ffmpeg only for
//! conversion / embedding). Platforms: see [`ytdlp`].
//!
//! [yt-dlp]: https://github.com/yt-dlp/yt-dlp

mod browse;
pub mod clean;
mod core_api;
pub mod search;
pub mod settings;
mod ui;
pub mod ytdlp;

use mod_sys::{
    ModStatus,
    sdk::{Host, Meta, ModEntry},
};
use settings::Settings;
use std::{
    ffi::c_void,
    panic::{AssertUnwindSafe, catch_unwind},
    path::PathBuf,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicU64, AtomicUsize, Ordering},
        mpsc::{Sender, channel},
    },
    thread::JoinHandle,
};
use ui::{Act, Ui};
use ytdlp::{AudioFormat, DlOptions, Event};

/// one queued link.
struct Job {
    /// the [`Shared::generation`] it was queued in; Cancel bumps it.
    generation: u64,
    url: String,
    settings: Settings,
    /// the search result it was queued from (gets marked when done).
    hit: Option<String>,
}

/// state shared by the frontend's thread (actions) and the worker.
struct Shared {
    host: Host,
    ui: Box<Ui>,
    settings: Mutex<Settings>,
    config_file: Option<PathBuf>,
    generation: AtomicU64,
    queued: AtomicUsize,
    running: ytdlp::Running,
    results: Mutex<browse::Results>,
    /// bumped to cancel the running search / preview.
    browse_gen: AtomicU64,
    browse_running: ytdlp::Running,
}

struct Running {
    shared: Arc<Shared>,
    jobs: Sender<Job>,
    worker: JoinHandle<()>,
    tasks: Sender<browse::TaskMsg>,
    browser: JoinHandle<()>,
}

static RUNNING: Mutex<Option<Running>> = Mutex::new(None);

fn running() -> MutexGuard<'static, Option<Running>> {
    RUNNING.lock().unwrap_or_else(|e| e.into_inner())
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

struct YtDlp;

impl ModEntry for YtDlp {
    fn hello() -> Meta {
        Meta {
            author: Some(c"dream_player"),
            description: Some(
                c"Searches YouTube / SoundCloud, previews and downloads audio with yt-dlp into the library",
            ),
            ..Meta::new(c"yt-dlp", c"0.3.0")
        }
    }

    fn init(host: Host) -> Result<(), ModStatus> {
        let mut slot = running();
        if slot.is_some() {
            return Err(ModStatus::AlreadyRunning);
        }
        let config_file = core_api::get_dir(&host, core_api::Dir::Config)
            .ok()
            .map(|d| d.join("mods").join("rs_mod_ytdlp.conf"));
        let settings = config_file
            .as_deref()
            .map(Settings::load)
            .unwrap_or_default();
        let ui = Ui::new(&settings, on_action);
        if config_file.is_none() {
            ui.set_status(
                "The core has no core_get_dir: downloads are unavailable, settings are not saved.",
            );
        }
        host.add_tab(unsafe { &*ui.tab() })?;

        let shared = Arc::new(Shared {
            host,
            ui,
            settings: Mutex::new(settings),
            config_file,
            generation: AtomicU64::new(0),
            queued: AtomicUsize::new(0),
            running: ytdlp::Running::default(),
            results: Mutex::new(browse::Results::default()),
            browse_gen: AtomicU64::new(0),
            browse_running: ytdlp::Running::default(),
        });
        browse::clear_previews(&shared);
        let (jobs, rx) = channel::<Job>();
        let worker = {
            let shared = shared.clone();
            std::thread::Builder::new()
                .name("rs_mod_ytdlp".into())
                .spawn(move || {
                    for job in rx {
                        shared.queued.fetch_sub(1, Ordering::AcqRel);
                        if catch_unwind(AssertUnwindSafe(|| run_job(&shared, job))).is_err() {
                            shared
                                .host
                                .report_error(ModStatus::Panic, "the download worker panicked");
                            return;
                        }
                    }
                })
                .map_err(|_| ModStatus::InitFailed)?
        };
        let (tasks, task_rx) = channel::<browse::TaskMsg>();
        let browser = {
            let shared = shared.clone();
            std::thread::Builder::new()
                .name("rs_mod_ytdlp_search".into())
                .spawn(move || {
                    for msg in task_rx {
                        if catch_unwind(AssertUnwindSafe(|| browse::run_task(&shared, msg)))
                            .is_err()
                        {
                            shared
                                .host
                                .report_error(ModStatus::Panic, "the search worker panicked");
                            return;
                        }
                    }
                })
                .map_err(|_| ModStatus::InitFailed)?
        };
        *slot = Some(Running {
            shared,
            jobs,
            worker,
            tasks,
            browser,
        });
        Ok(())
    }

    fn stop() -> Result<(), ModStatus> {
        let Some(Running {
            shared,
            jobs,
            worker,
            tasks,
            browser,
        }) = running().take()
        else {
            return Ok(());
        };
        shared.browse_gen.fetch_add(1, Ordering::AcqRel);
        drop(tasks);
        shared.browse_running.kill();
        let browser_ok = browser.join().is_ok();
        shared.generation.fetch_add(1, Ordering::AcqRel);
        drop(jobs); // the worker's loop ends after the current job
        shared.running.kill();
        let worker_ok = worker.join().is_ok();
        // the player keeps what it plays in memory: the files can go.
        browse::clear_previews(&shared);
        if !(browser_ok && worker_ok) {
            return Err(ModStatus::StopFailed);
        }
        // the host has already forgotten the tab; this frees it.
        drop(shared);
        Ok(())
    }
}

mod_sys::export_mod!(YtDlp);

/// every button of the tab ends up here; `ctx` says which ([`Act`]).
/// `target`: the requested state of a toggle (`0` / `1`), the chosen index
/// of the format select, `0` for buttons.
unsafe extern "C" fn on_action(ctx: *mut c_void, target: u64) -> u32 {
    let r = catch_unwind(AssertUnwindSafe(|| {
        let act = Act::from_ctx(ctx).ok_or(ModStatus::InvalidArg)?;
        let (shared, jobs, tasks) = {
            let g = running();
            let r = g.as_ref().ok_or(ModStatus::NotRunning)?;
            (r.shared.clone(), r.jobs.clone(), r.tasks.clone())
        };
        handle(&shared, &jobs, &tasks, act, target)
    }));
    match r {
        Ok(Ok(())) => ModStatus::Ok.code(),
        Ok(Err(e)) => e.code(),
        Err(_) => ModStatus::Panic.code(),
    }
}

fn handle(
    shared: &Shared,
    jobs: &Sender<Job>,
    tasks: &Sender<browse::TaskMsg>,
    act: Act,
    target: u64,
) -> Result<(), ModStatus> {
    let ui = &shared.ui;
    let mut s = lock(&shared.settings);
    match act {
        Act::Download => {
            ui.read_inputs(&mut s);
            save(shared, &s);
            let text = ui.url_text();
            let (urls, bad): (Vec<_>, Vec<_>) = text
                .split_whitespace()
                .map(|l| (l, clean::clean(l, s.clean)))
                .partition(|(_, r)| r.is_ok());
            if let Some((link, Err(e))) = bad.first() {
                ui.set_status(&format!("Bad link {link}: {e}"));
                return Ok(());
            }
            if urls.is_empty() {
                ui.set_status("Paste a link first.");
                return Ok(());
            }
            let generation = shared.generation.load(Ordering::Acquire);
            for (_, url) in urls {
                shared.queued.fetch_add(1, Ordering::AcqRel);
                let job = Job {
                    generation,
                    url: url.expect("partitioned"),
                    settings: s.clone(),
                    hit: None,
                };
                if jobs.send(job).is_err() {
                    shared.queued.fetch_sub(1, Ordering::AcqRel);
                }
            }
            ui.set_status(&format!(
                "Queued: {}",
                shared.queued.load(Ordering::Acquire)
            ));
        }
        Act::Cancel => {
            shared.generation.fetch_add(1, Ordering::AcqRel);
            shared.running.kill();
            ui.set_status("Cancelled.");
        }
        Act::Save => {
            ui.read_inputs(&mut s);
            ui.set_status(if save(shared, &s) {
                "Settings saved."
            } else {
                "Settings could not be saved."
            });
        }
        Act::ClearHistory => ui.clear_history(),
        Act::Search => {
            ui.read_inputs(&mut s);
            browse::search(shared, tasks, jobs, &s);
        }
        Act::Pick => {
            ui.read_inputs(&mut s);
            browse::pick(shared, tasks, jobs, &s, target)?;
        }
        Act::StopListening => browse::stop_listening(shared, jobs, &s),
        Act::SaveResult => {
            ui.read_inputs(&mut s);
            save(shared, &s);
            browse::save_selected(shared, jobs, &s);
        }
        Act::SearchSource => {
            s.search_source = search::Source::from_index(target).ok_or(ModStatus::InvalidArg)?;
            ui.show_settings(&s);
            save(shared, &s);
        }
        Act::SearchCount => {
            s.search_count = *usize::try_from(target)
                .ok()
                .and_then(|i| search::COUNTS.get(i))
                .ok_or(ModStatus::InvalidArg)?;
            ui.show_settings(&s);
            save(shared, &s);
        }
        Act::Format => {
            s.format = AudioFormat::from_index(target).ok_or(ModStatus::InvalidArg)?;
            ui.show_settings(&s);
            save(shared, &s);
        }
        toggle => {
            let on = match target {
                0 => false,
                1 => true,
                _ => return Err(ModStatus::InvalidArg),
            };
            let flag = match toggle {
                Act::StripPlaylist => &mut s.clean.playlist,
                Act::StripTracking => &mut s.clean.tracking,
                Act::StripTime => &mut s.clean.time,
                Act::StripOther => &mut s.clean.other,
                Act::Canonical => &mut s.clean.canonical,
                Act::EmbedMetadata => &mut s.embed_metadata,
                Act::EmbedThumbnail => &mut s.embed_thumbnail,
                Act::AddToLibrary => &mut s.add_to_library,
                _ => unreachable!("handled above"),
            };
            *flag = on;
            ui.show_settings(&s);
            save(shared, &s);
        }
    }
    Ok(())
}

fn save(shared: &Shared, s: &Settings) -> bool {
    shared
        .config_file
        .as_deref()
        .is_some_and(|f| s.save(f).is_ok())
}

fn cancelled(shared: &Shared, job: &Job) -> bool {
    shared.generation.load(Ordering::Acquire) != job.generation
}

/// a non-empty path setting.
fn opt_path(s: &str) -> Option<PathBuf> {
    (!s.is_empty()).then(|| PathBuf::from(s))
}

fn run_job(shared: &Shared, job: Job) {
    if cancelled(shared, &job) {
        return;
    }
    let ui = &shared.ui;
    let result = download(shared, &job);
    if let Some(id) = &job.hit {
        let file = result
            .as_ref()
            .ok()
            .and_then(|r| r.as_ref())
            .and_then(|(_, f)| f.first());
        browse::job_done(shared, id, file.map(PathBuf::as_path));
    }
    match result.map(|r| r.map(|(line, _)| line)) {
        Ok(Some(line)) => {
            ui.set_status(&line);
        }
        Ok(None) => {}
        Err(_) if cancelled(shared, &job) => {
            ui.set_status("Cancelled.");
            ui.push_history(&format!("cancelled: {}", job.url), None);
        }
        Err(e) => {
            ui.set_status(&format!("Failed: {e}"));
            ui.push_history(&format!("failed: {} ({e})", job.url), None);
        }
    }
}

/// yt-dlp options from the settings; checks ffmpeg. The string is a note
/// for the status line (what was skipped for lack of ffmpeg).
fn dl_options(
    s: &Settings,
    out_dir: PathBuf,
    temp_dir: Option<PathBuf>,
) -> Result<(DlOptions, String), String> {
    let ffmpeg = opt_path(&s.ffmpeg_path);
    let mut note = String::new();
    let (mut embed_metadata, mut embed_thumbnail) = (s.embed_metadata, s.embed_thumbnail);
    if (s.format.needs_ffmpeg() || embed_metadata || embed_thumbnail)
        && !ytdlp::has_ffmpeg(ffmpeg.as_deref())
    {
        if s.format.needs_ffmpeg() {
            return Err(format!(
                "ffmpeg not found: it is needed for {}; install it, set its path or choose {}",
                s.format.key(),
                AudioFormat::M4a.key()
            ));
        }
        // the download itself works without ffmpeg; only tags are lost.
        (embed_metadata, embed_thumbnail) = (false, false);
        note = " (no ffmpeg: tags / cover not embedded)".into();
    }

    let opts = DlOptions {
        ytdlp: opt_path(&s.ytdlp_path).unwrap_or_else(|| "yt-dlp".into()),
        ffmpeg,
        format: s.format,
        embed_metadata,
        embed_thumbnail,
        single_video: s.clean.playlist,
        out_dir,
        temp_dir,
        extra_args: s.extra_args.split_whitespace().map(str::to_owned).collect(),
    };
    Ok((opts, note))
}

/// `Ok(Some((status, files)))` — done; `Ok(None)` — skipped (cancelled
/// before start).
fn download(shared: &Shared, job: &Job) -> Result<Option<(String, Vec<PathBuf>)>, String> {
    let (host, ui, s) = (&shared.host, &shared.ui, &job.settings);
    let music = core_api::get_dir(host, core_api::Dir::Music)?;
    let out_dir = if s.subfolder.is_empty() {
        music
    } else {
        music.join(&s.subfolder)
    };
    let temp_dir = core_api::get_dir(host, core_api::Dir::Cache)
        .ok()
        .map(|c| c.join("rs_mod_ytdlp"));
    std::fs::create_dir_all(&out_dir)
        .map_err(|e| format!("can't create {}: {e}", out_dir.display()))?;
    if let Some(t) = &temp_dir {
        let _ = std::fs::create_dir_all(t);
    }
    let (opts, note) = dl_options(s, out_dir, temp_dir)?;
    if cancelled(shared, job) {
        return Ok(None);
    }
    let queued = || match shared.queued.load(Ordering::Acquire) {
        0 => String::new(),
        n => format!(" (+{n} queued)"),
    };
    ui.set_status(&format!("Starting: {}{}", job.url, queued()));
    let mut title = job.url.clone();
    let files = ytdlp::download(&opts, &job.url, &shared.running, |ev| match ev {
        Event::Title(t) => {
            title = t;
            ui.set_status(&format!("Downloading: {title}{}", queued()));
        }
        Event::Progress(p) => ui.set_status(&format!("{p} {title}{}", queued())),
    })?;

    let mut library = String::new();
    if s.add_to_library {
        library = match core_api::add_tracks(host, &files) {
            Ok(n) if n as usize == files.len() => ", added to the library".into(),
            Ok(n) => format!(
                ", {n} of {} added to the library (the player can't read the rest)",
                files.len()
            ),
            Err(e) => format!(", not added to the library: {e}"),
        };
    }
    for f in &files {
        let name = f.file_stem().map_or_else(
            || f.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        );
        ui.push_history(&format!("ok: {name}"), Some(&f.to_string_lossy()));
    }
    let what = if files.len() == 1 {
        title
    } else {
        format!("{} files", files.len())
    };
    Ok(Some((
        format!("Done: {what}{library}{note}{}", queued()),
        files,
    )))
}
