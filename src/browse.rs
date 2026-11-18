//! Search and preview: a query lists YouTube videos (with durations);
//! picking one downloads its audio into `<cache>/rs_mod_ytdlp/preview` and
//! plays it through the player; "Save to library" moves that file to the
//! music folder and adds it to the library.
//!
//! - only audio is ever downloaded (the same audio-only formats as a plain
//!   download), so no video is left anywhere;
//! - one search / preview process at a time: a new search or a pick of
//!   another video kills the current one ([`Shared::browse_gen`]);
//! - Save on a video still downloading saves it when it's done; Save on one
//!   not downloaded queues a plain download; a cancelled preview that was to
//!   be saved becomes a plain download too, so a Save is never lost;
//! - unsaved previews are deleted on a new search and when the mod stops.

use crate::{
    Job, Shared, core_api, lock, opt_path,
    search::{self, Hit},
    settings::Settings,
    ytdlp::{self, Event},
};
use std::{
    path::{Path, PathBuf},
    sync::{atomic::Ordering, mpsc::Sender},
};

/// what happened to a result.
#[derive(Debug, Clone, PartialEq)]
pub enum Mark {
    None,
    /// its audio is being downloaded for listening.
    Loading,
    /// downloaded into the preview folder.
    Ready(PathBuf),
    /// queued as a plain download.
    Queued,
    /// in the music folder.
    Saved(PathBuf),
    Failed,
}

#[derive(Debug, Clone)]
pub struct Item {
    pub hit: Hit,
    pub mark: Mark,
    /// Save was pressed while it was loading.
    pub save_when_ready: bool,
}

/// the results list and what the user does with it.
#[derive(Debug, Default)]
pub struct Results {
    pub items: Vec<Item>,
    /// the id of the last picked result (Save acts on it).
    pub selected: Option<String>,
    /// the id of the result the player was told to play.
    pub playing: Option<String>,
}

impl Results {
    fn get(&mut self, id: &str) -> Option<&mut Item> {
        self.items.iter_mut().find(|i| i.hit.id == id)
    }

    /// the lines of the list on the tab.
    pub fn lines(&self) -> Vec<String> {
        self.items
            .iter()
            .map(|i| {
                let mark = match &i.mark {
                    _ if self.playing.as_deref() == Some(&i.hit.id) => "▶",
                    Mark::Loading => "…",
                    Mark::Queued => "⇣",
                    Mark::Saved(_) => "✓",
                    Mark::Failed => "✗",
                    Mark::None | Mark::Ready(_) => "·",
                };
                let dur = search::fmt_duration(i.hit.duration);
                match i.hit.channel.as_str() {
                    "" => format!("{mark} [{dur}] {}", i.hit.title),
                    ch => format!("{mark} [{dur}] {} — {ch}", i.hit.title),
                }
            })
            .collect()
    }
}

/// work for the browse thread.
pub enum Task {
    Search { query: String, settings: Settings },
    Fetch { hit: Hit, settings: Settings },
}

pub struct TaskMsg {
    /// the [`Shared::browse_gen`] it was sent in.
    pub generation: u64,
    pub task: Task,
}

fn show(shared: &Shared, r: &Results) {
    shared.ui.set_results(&r.lines());
}

/// `<cache>/rs_mod_ytdlp/preview`.
pub fn preview_dir(shared: &Shared) -> Result<PathBuf, String> {
    Ok(core_api::get_dir(&shared.host, core_api::Dir::Cache)?
        .join("rs_mod_ytdlp")
        .join("preview"))
}

/// deletes every preview not saved yet.
pub fn clear_previews(shared: &Shared) {
    if let Ok(d) = preview_dir(shared) {
        let _ = std::fs::remove_dir_all(d);
    }
}

fn queue_download(shared: &Shared, jobs: &Sender<Job>, hit: &Hit, settings: &Settings) -> bool {
    let generation = shared.generation.load(Ordering::Acquire);
    shared.queued.fetch_add(1, Ordering::AcqRel);
    let job = Job {
        generation,
        url: hit.url(),
        settings: settings.clone(),
        hit: Some(hit.id.clone()),
    };
    if jobs.send(job).is_err() {
        shared.queued.fetch_sub(1, Ordering::AcqRel);
        return false;
    }
    true
}

/// kills the running search / preview. Loading results go back to
/// nothing, or to a plain download when Save was pressed on them.
fn cancel(shared: &Shared, r: &mut Results, jobs: &Sender<Job>, settings: &Settings) {
    shared.browse_gen.fetch_add(1, Ordering::AcqRel);
    shared.browse_running.kill();
    for i in r.items.iter_mut().filter(|i| i.mark == Mark::Loading) {
        i.mark = if i.save_when_ready && queue_download(shared, jobs, &i.hit, settings) {
            Mark::Queued
        } else {
            Mark::None
        };
        i.save_when_ready = false;
    }
}

// ───────────────────────── actions (frontend thread) ─────────────────────────

pub fn search(shared: &Shared, tasks: &Sender<TaskMsg>, jobs: &Sender<Job>, s: &Settings) {
    let query = shared.ui.query_text();
    if query.is_empty() {
        shared.ui.set_status("Type what to search for first.");
        return;
    }
    let mut r = lock(&shared.results);
    cancel(shared, &mut r, jobs, s);
    for i in &r.items {
        if let Mark::Ready(p) = &i.mark {
            let _ = std::fs::remove_file(p);
        }
    }
    *r = Results {
        playing: r.playing.take(),
        ..Results::default()
    };
    show(shared, &r);
    let generation = shared.browse_gen.load(Ordering::Acquire);
    shared.ui.set_status(&format!("Searching: {query}"));
    let _ = tasks.send(TaskMsg {
        generation,
        task: Task::Search {
            query,
            settings: s.clone(),
        },
    });
}

pub fn pick(
    shared: &Shared,
    tasks: &Sender<TaskMsg>,
    jobs: &Sender<Job>,
    s: &Settings,
    index: u64,
) -> Result<(), mod_sys::ModStatus> {
    let ui = &shared.ui;
    let mut r = lock(&shared.results);
    let item = usize::try_from(index)
        .ok()
        .and_then(|i| r.items.get(i))
        .ok_or(mod_sys::ModStatus::InvalidArg)?;
    let (hit, mark) = (item.hit.clone(), item.mark.clone());
    r.selected = Some(hit.id.clone());
    match mark {
        Mark::Ready(p) | Mark::Saved(p) => {
            if r.playing.as_deref() == Some(&hit.id) {
                match core_api::play_pause(&shared.host) {
                    Ok(()) => ui.set_status(&format!("Pause / resume: {}", hit.title)),
                    Err(e) => ui.set_status(&format!("Can't pause: {e}")),
                }
            } else {
                play(shared, &mut r, &hit, &p);
            }
        }
        Mark::Loading => ui.set_status(&format!("Still downloading: {}", hit.title)),
        Mark::None | Mark::Failed | Mark::Queued => {
            cancel(shared, &mut r, jobs, s);
            if let Some(i) = r.get(&hit.id) {
                i.mark = Mark::Loading;
            }
            let generation = shared.browse_gen.load(Ordering::Acquire);
            ui.set_status(&format!("Loading for listening: {}", hit.title));
            let _ = tasks.send(TaskMsg {
                generation,
                task: Task::Fetch {
                    hit,
                    settings: s.clone(),
                },
            });
        }
    }
    show(shared, &r);
    Ok(())
}

fn play(shared: &Shared, r: &mut Results, hit: &Hit, file: &Path) {
    match core_api::play_file(&shared.host, file) {
        Ok(()) => {
            r.playing = Some(hit.id.clone());
            shared.ui.set_status(&format!(
                "Playing: {} [{}]. Save to library keeps it.",
                hit.title,
                search::fmt_duration(hit.duration)
            ));
        }
        Err(e) => shared
            .ui
            .set_status(&format!("Can't play {}: {e}", hit.title)),
    }
}

pub fn stop_listening(shared: &Shared, jobs: &Sender<Job>, s: &Settings) {
    let mut r = lock(&shared.results);
    cancel(shared, &mut r, jobs, s);
    if r.playing.take().is_some() {
        let _ = core_api::stop(&shared.host);
    }
    show(shared, &r);
    shared.ui.set_status("Stopped.");
}

pub fn save_selected(shared: &Shared, jobs: &Sender<Job>, s: &Settings) {
    let ui = &shared.ui;
    let mut r = lock(&shared.results);
    let Some(id) = r.selected.clone() else {
        ui.set_status("Pick a result first.");
        return;
    };
    let Some(item) = r.get(&id) else {
        ui.set_status("Pick a result first.");
        return;
    };
    let hit = item.hit.clone();
    match item.mark.clone() {
        Mark::Ready(p) => {
            let line = save_file(shared, s, item, &p);
            ui.set_status(&line);
        }
        Mark::Loading => {
            item.save_when_ready = true;
            ui.set_status(&format!("Will be saved when downloaded: {}", hit.title));
        }
        Mark::Saved(_) => ui.set_status(&format!("Already saved: {}", hit.title)),
        Mark::Queued => ui.set_status(&format!("Already queued: {}", hit.title)),
        Mark::None | Mark::Failed => {
            if queue_download(shared, jobs, &hit, s) {
                item.mark = Mark::Queued;
                ui.set_status(&format!("Queued for download: {}", hit.title));
            }
        }
    }
    show(shared, &r);
}

/// moves a downloaded preview into the music folder (+ library); marks the
/// item. Returns the status line.
fn save_file(shared: &Shared, s: &Settings, item: &mut Item, file: &Path) -> String {
    match move_to_music(shared, s, file) {
        Ok(dest) => {
            let library = if s.add_to_library {
                match core_api::add_tracks(&shared.host, std::slice::from_ref(&dest)) {
                    Ok(1) => ", added to the library".to_owned(),
                    Ok(_) => ", but the player can't read it".to_owned(),
                    Err(e) => format!(", not added to the library: {e}"),
                }
            } else {
                String::new()
            };
            shared.ui.push_history(
                &format!("ok: {}", item.hit.title),
                Some(&dest.to_string_lossy()),
            );
            item.mark = Mark::Saved(dest);
            item.save_when_ready = false;
            format!("Saved: {}{library}", item.hit.title)
        }
        Err(e) => {
            item.save_when_ready = false;
            shared
                .ui
                .push_history(&format!("failed: {} ({e})", item.hit.title), None);
            format!("Not saved: {e}")
        }
    }
}

/// `<music>/<subfolder>/<file name>`; never replaces an existing file.
fn move_to_music(shared: &Shared, s: &Settings, file: &Path) -> Result<PathBuf, String> {
    let music = core_api::get_dir(&shared.host, core_api::Dir::Music)?;
    let dir = if s.subfolder.is_empty() {
        music
    } else {
        music.join(&s.subfolder)
    };
    std::fs::create_dir_all(&dir).map_err(|e| format!("can't create {}: {e}", dir.display()))?;
    let name = file.file_name().ok_or("no file name")?;
    let dest = dir.join(name);
    if dest.exists() {
        return Err(format!("already in the music folder: {}", dest.display()));
    }
    if std::fs::rename(file, &dest).is_err() {
        // another filesystem: copy, then delete the preview.
        std::fs::copy(file, &dest).map_err(|e| format!("can't copy to {}: {e}", dest.display()))?;
        let _ = std::fs::remove_file(file);
    }
    Ok(dest)
}

// ───────────────────────────── the browse thread ─────────────────────────────

fn current(shared: &Shared, generation: u64) -> bool {
    shared.browse_gen.load(Ordering::Acquire) == generation
}

pub fn run_task(shared: &Shared, msg: TaskMsg) {
    if !current(shared, msg.generation) {
        return;
    }
    match msg.task {
        Task::Search { query, settings } => run_search(shared, msg.generation, &query, &settings),
        Task::Fetch { hit, settings } => run_fetch(shared, msg.generation, &hit, &settings),
    }
}

fn run_search(shared: &Shared, generation: u64, query: &str, s: &Settings) {
    let ytdlp = opt_path(&s.ytdlp_path).unwrap_or_else(|| "yt-dlp".into());
    let extra: Vec<String> = s.extra_args.split_whitespace().map(str::to_owned).collect();
    let found = search::search(
        &ytdlp,
        query,
        s.search_count,
        &extra,
        &shared.browse_running,
    );
    let mut r = lock(&shared.results);
    if !current(shared, generation) {
        return;
    }
    match found {
        Ok(hits) if hits.is_empty() => shared.ui.set_status(&format!("Nothing found: {query}")),
        Ok(hits) => {
            r.items = hits
                .into_iter()
                .map(|hit| Item {
                    hit,
                    mark: Mark::None,
                    save_when_ready: false,
                })
                .collect();
            show(shared, &r);
            shared.ui.set_status(&format!(
                "Found {}: pick one to listen to it.",
                r.items.len()
            ));
        }
        Err(e) => shared.ui.set_status(&format!("Search failed: {e}")),
    }
}

fn run_fetch(shared: &Shared, generation: u64, hit: &Hit, s: &Settings) {
    let ui = &shared.ui;
    let fetched = preview_dir(shared).and_then(|dir| {
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("can't create {}: {e}", dir.display()))?;
        // a leftover with the same name would make yt-dlp skip the download.
        let tag = format!("[{}]", hit.id);
        for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            if e.file_name().to_string_lossy().contains(&tag) {
                let _ = std::fs::remove_file(e.path());
            }
        }
        let (opts, _note) = crate::dl_options(s, dir, None)?;
        ytdlp::download(&opts, &hit.url(), &shared.browse_running, |ev| {
            if let Event::Progress(p) = ev {
                ui.set_status(&format!("Loading for listening {p}: {}", hit.title));
            }
        })
    });

    // the settings of now (the subfolder may have changed); taken before
    // `results`: the frontend thread locks settings, then results.
    let now = lock(&shared.settings).clone();
    let mut r = lock(&shared.results);
    if !current(shared, generation) {
        // cancelled meanwhile; `cancel` already reset the item.
        if let Ok(files) = &fetched {
            files.iter().for_each(|f| drop(std::fs::remove_file(f)));
        }
        return;
    }
    let selected = r.selected.as_deref() == Some(&hit.id);
    let Some(item) = r.get(&hit.id) else { return };
    let to_play = match fetched {
        Ok(files) => {
            let file = files
                .into_iter()
                .next()
                .expect("download returns at least one file");
            item.mark = Mark::Ready(file.clone());
            let saved = item
                .save_when_ready
                .then(|| save_file(shared, &now, item, &file));
            match &item.mark {
                Mark::Ready(p) | Mark::Saved(p) => Some((p.clone(), saved)),
                _ => {
                    ui.set_status(saved.as_deref().unwrap_or_default());
                    None
                }
            }
        }
        Err(e) => {
            item.mark = Mark::Failed;
            item.save_when_ready = false;
            ui.set_status(&format!("Can't load {}: {e}", hit.title));
            None
        }
    };
    if let Some((p, saved)) = to_play {
        if selected {
            play(shared, &mut r, hit, &p);
        }
        // what happened to the Save matters more than "Playing".
        if let Some(line) = saved {
            ui.set_status(&line);
        }
    }
    show(shared, &r);
}

/// a plain download queued from the results finished: mark its result.
pub fn job_done(shared: &Shared, id: &str, file: Option<&Path>) {
    let mut r = lock(&shared.results);
    if let Some(i) = r.get(id) {
        i.mark = match file {
            Some(f) => Mark::Saved(f.to_path_buf()),
            None => Mark::Failed,
        };
        show(shared, &r);
    }
}
