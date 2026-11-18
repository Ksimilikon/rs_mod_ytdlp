//! Settings of the mod, kept as `key=value` lines in
//! `<config>/mods/rs_mod_ytdlp.conf` (no serde: the format is trivial and
//! the mod stays small).

use crate::{
    clean::CleanFlags,
    search::{self, Source},
    ytdlp::AudioFormat,
};
use std::{fs, io, path::Path};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub clean: CleanFlags,
    pub format: AudioFormat,
    pub embed_metadata: bool,
    pub embed_thumbnail: bool,
    /// put downloaded files into the library (`idx_add_tracks`).
    pub add_to_library: bool,
    /// the yt-dlp executable; empty = `yt-dlp` from `PATH`.
    pub ytdlp_path: String,
    /// ffmpeg executable or its directory (`--ffmpeg-location`); empty = `PATH`.
    pub ffmpeg_path: String,
    /// where files go, relative to the music folder; empty = its root.
    pub subfolder: String,
    /// extra yt-dlp arguments, split by whitespace (cookies, proxies, ...).
    pub extra_args: String,
    /// how many videos a search lists (one of [`search::COUNTS`]).
    pub search_count: usize,
    /// where to search.
    pub search_source: Source,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            clean: CleanFlags::default(),
            format: AudioFormat::M4a,
            embed_metadata: true,
            embed_thumbnail: false,
            add_to_library: true,
            ytdlp_path: String::new(),
            ffmpeg_path: String::new(),
            subfolder: "YouTube".into(),
            extra_args: String::new(),
            search_count: 10,
            search_source: Source::YouTube,
        }
    }
}

fn parse_bool(v: &str) -> Option<bool> {
    match v {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

impl Settings {
    /// unknown keys and broken values are ignored (defaults stay).
    pub fn parse(text: &str) -> Self {
        let mut s = Self::default();
        for line in text.lines() {
            let line = line.trim_start();
            if line.starts_with('#') {
                continue;
            }
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            let (k, v) = (k.trim(), v.trim());
            let flag = |dst: &mut bool| {
                if let Some(b) = parse_bool(v) {
                    *dst = b;
                }
            };
            match k {
                "strip_playlist" => flag(&mut s.clean.playlist),
                "strip_tracking" => flag(&mut s.clean.tracking),
                "strip_time" => flag(&mut s.clean.time),
                "strip_other" => flag(&mut s.clean.other),
                "canonical" => flag(&mut s.clean.canonical),
                "embed_metadata" => flag(&mut s.embed_metadata),
                "embed_thumbnail" => flag(&mut s.embed_thumbnail),
                "add_to_library" => flag(&mut s.add_to_library),
                "format" => {
                    if let Some(f) = AudioFormat::from_key(v) {
                        s.format = f;
                    }
                }
                "ytdlp_path" => s.ytdlp_path = v.into(),
                "ffmpeg_path" => s.ffmpeg_path = v.into(),
                "subfolder" => s.subfolder = v.into(),
                "extra_args" => s.extra_args = v.into(),
                "search_source" => {
                    if let Some(src) = Source::from_key(v) {
                        s.search_source = src;
                    }
                }
                "search_results" => {
                    if let Some(n) = v.parse().ok().filter(|n| search::COUNTS.contains(n)) {
                        s.search_count = n;
                    }
                }
                _ => {}
            }
        }
        s
    }

    pub fn serialize(&self) -> String {
        let b = |v: bool| if v { "true" } else { "false" };
        let line = |s: &str| s.replace(['\n', '\r'], " ");
        format!(
            "# rs_mod_ytdlp settings\n\
             strip_playlist={}\nstrip_tracking={}\nstrip_time={}\nstrip_other={}\ncanonical={}\n\
             format={}\nembed_metadata={}\nembed_thumbnail={}\nadd_to_library={}\n\
             ytdlp_path={}\nffmpeg_path={}\nsubfolder={}\nextra_args={}\nsearch_results={}\nsearch_source={}\n",
            b(self.clean.playlist),
            b(self.clean.tracking),
            b(self.clean.time),
            b(self.clean.other),
            b(self.clean.canonical),
            self.format.key(),
            b(self.embed_metadata),
            b(self.embed_thumbnail),
            b(self.add_to_library),
            line(&self.ytdlp_path),
            line(&self.ffmpeg_path),
            line(&self.subfolder),
            line(&self.extra_args),
            self.search_count,
            self.search_source.key(),
        )
    }

    /// a missing / unreadable file → defaults.
    pub fn load(path: &Path) -> Self {
        fs::read_to_string(path)
            .map(|t| Self::parse(&t))
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        // write + rename: a crash never leaves a half-written file.
        let tmp = path.with_extension("conf.tmp");
        fs::write(&tmp, self.serialize())?;
        fs::rename(&tmp, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let s = Settings {
            clean: CleanFlags {
                playlist: false,
                tracking: true,
                time: false,
                other: true,
                canonical: false,
            },
            format: AudioFormat::Mp3,
            embed_metadata: false,
            embed_thumbnail: true,
            add_to_library: false,
            ytdlp_path: "C:\\tools\\yt-dlp.exe".into(),
            ffmpeg_path: "/opt/ffmpeg/bin".into(),
            subfolder: "".into(),
            extra_args: "--cookies c.txt".into(),
            search_count: 30,
            search_source: Source::SoundCloud,
        };
        assert_eq!(Settings::parse(&s.serialize()), s);
    }

    #[test]
    fn garbage_keeps_defaults() {
        let s = Settings::parse(
            "search_results=7\nformat=wma\nstrip_time=maybe\nnope\n=1\nsubfolder = Music Box \n",
        );
        assert_eq!(
            s,
            Settings {
                subfolder: "Music Box".into(),
                ..Settings::default()
            }
        );
    }

    #[test]
    fn save_load() {
        let dir = std::env::temp_dir().join(format!("rs_mod_ytdlp_test_{}", std::process::id()));
        let path = dir.join("mods/rs_mod_ytdlp.conf");
        let s = Settings {
            format: AudioFormat::Vorbis,
            ..Settings::default()
        };
        s.save(&path).unwrap();
        assert_eq!(Settings::load(&path), s);
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(Settings::load(&path), Settings::default());
    }
}
