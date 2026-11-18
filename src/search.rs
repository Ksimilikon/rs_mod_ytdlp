//! Search through yt-dlp: YouTube (`ytsearchN:<query>`) or SoundCloud
//! (`scsearchN:<query>`). A flat listing, no download, one line per result —
//! id, duration, title, channel / uploader, page link.

use crate::ytdlp::{self, Running};
use std::{ffi::OsString, path::Path};

/// choices of the "Search results" select, in its order.
pub const COUNTS: [usize; 4] = [5, 10, 20, 30];
/// the most results a search can give (slots of the results list).
pub const MAX: usize = COUNTS[COUNTS.len() - 1];

/// separates the fields of a result line (titles never contain it).
const SEP: char = '\u{1f}';

/// where to search.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    YouTube,
    SoundCloud,
}

impl Source {
    /// in the order of the "Search on" select.
    pub const ALL: [Self; 2] = [Self::YouTube, Self::SoundCloud];

    pub fn key(self) -> &'static str {
        match self {
            Self::YouTube => "youtube",
            Self::SoundCloud => "soundcloud",
        }
    }

    pub fn from_key(k: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.key() == k)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::YouTube => "YouTube",
            Self::SoundCloud => "SoundCloud",
        }
    }

    pub fn index(self) -> usize {
        Self::ALL.iter().position(|&s| s == self).unwrap_or(0)
    }

    pub fn from_index(i: u64) -> Option<Self> {
        Self::ALL.get(usize::try_from(i).ok()?).copied()
    }

    /// yt-dlp's search prefix.
    fn prefix(self) -> &'static str {
        match self {
            Self::YouTube => "ytsearch",
            Self::SoundCloud => "scsearch",
        }
    }
}

/// one result.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    /// the site's id (YouTube: 11 chars, SoundCloud: a number); also in the
    /// file name (`title [id].ext`).
    pub id: String,
    pub title: String,
    pub channel: String,
    /// seconds; `None` — unknown (live streams, premieres).
    pub duration: Option<u64>,
    /// the page to download from.
    pub url: String,
}

/// `3:05`, `1:02:03`; `--:--` when unknown.
pub fn fmt_duration(d: Option<u64>) -> String {
    match d {
        None => "--:--".into(),
        Some(s) if s >= 3600 => format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60),
        Some(s) => format!("{}:{:02}", s / 60, s % 60),
    }
}

/// the yt-dlp command line (without the program).
pub fn build_args(
    source: Source,
    query: &str,
    count: usize,
    extra_args: &[String],
) -> Vec<OsString> {
    let mut a: Vec<OsString> = [
        "--ignore-config",
        "--flat-playlist",
        "--no-warnings",
        "--no-colors",
        "--print",
    ]
    .into_iter()
    .map(OsString::from)
    .collect();
    a.push(
        format!("%(id)s{SEP}%(duration)s{SEP}%(title)s{SEP}%(channel,uploader|)s{SEP}%(webpage_url,url|)s").into(),
    );
    a.extend(extra_args.iter().map(OsString::from));
    a.push("--".into());
    // newlines would end the query early; yt-dlp treats the rest as text.
    a.push(
        format!(
            "{}{count}:{}",
            source.prefix(),
            query.replace(['\n', '\r'], " ")
        )
        .into(),
    );
    a
}

fn known(v: &str) -> Option<&str> {
    let v = v.trim();
    (!v.is_empty() && v != "NA").then_some(v)
}

/// one output line → a hit; anything else (warnings, junk) → `None`.
pub fn parse_line(source: Source, line: &str) -> Option<Hit> {
    let mut f = line.trim_end_matches(['\r', '\n']).splitn(5, SEP);
    let id = known(f.next()?)?;
    let duration = f.next()?;
    let title = known(f.next()?).unwrap_or(id);
    let channel = f.next().and_then(known).unwrap_or("");
    let page = f.next().and_then(known);
    if !id
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return None;
    }
    let url = match (source, page) {
        (_, Some(p)) if p.starts_with("https://") || p.starts_with("http://") => p.to_owned(),
        (Source::YouTube, _) if id.len() == 11 => format!("https://www.youtube.com/watch?v={id}"),
        // a SoundCloud id alone is no link.
        _ => return None,
    };
    Some(Hit {
        id: id.into(),
        title: title.into(),
        channel: channel.into(),
        // YouTube gives whole seconds (sometimes `212.0`), SoundCloud
        // fractions (`355.776`).
        duration: duration
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|d| d.is_finite() && *d >= 0.0)
            .map(|d| d.round() as u64),
        url,
    })
}

/// runs the search to the end; a kill through `running` ends it with an
/// error.
pub fn search(
    ytdlp: &Path,
    source: Source,
    query: &str,
    count: usize,
    extra_args: &[String],
    running: &Running,
) -> Result<Vec<Hit>, String> {
    let lines = ytdlp::run_lines(ytdlp, build_args(source, query, count, extra_args), running)?;
    Ok(lines.iter().filter_map(|l| parse_line(source, l)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(fmt_duration(Some(19)), "0:19");
        assert_eq!(fmt_duration(Some(185)), "3:05");
        assert_eq!(fmt_duration(Some(3723)), "1:02:03");
        assert_eq!(fmt_duration(None), "--:--");
    }

    #[test]
    fn youtube_lines() {
        let yt = Source::YouTube;
        let h = parse_line(yt, "jNQXAC9IVRw\u{1f}19\u{1f}Me at the zoo\u{1f}jawed\u{1f}https://www.youtube.com/watch?v=jNQXAC9IVRw\n")
            .unwrap();
        assert_eq!(
            h,
            Hit {
                id: "jNQXAC9IVRw".into(),
                title: "Me at the zoo".into(),
                channel: "jawed".into(),
                duration: Some(19),
                url: "https://www.youtube.com/watch?v=jNQXAC9IVRw".into(),
            }
        );
        // no page link: built from the id.
        let live = parse_line(yt, "abcdefghijk\u{1f}NA\u{1f}Live\u{1f}NA\u{1f}NA").unwrap();
        assert_eq!((live.duration, live.channel.as_str()), (None, ""));
        assert_eq!(live.url, "https://www.youtube.com/watch?v=abcdefghijk");
        assert_eq!(
            parse_line(yt, "abcdefghijk\u{1f}212.0\u{1f}T\u{1f}")
                .unwrap()
                .duration,
            Some(212)
        );
        assert_eq!(parse_line(yt, "WARNING: something"), None);
        assert_eq!(parse_line(yt, "bad id\u{1f}1\u{1f}t\u{1f}c\u{1f}"), None);
    }

    #[test]
    fn soundcloud_lines() {
        let sc = Source::SoundCloud;
        let h = parse_line(
            sc,
            "1250206453\u{1f}355.776\u{1f}Tu Hi Haqeeqat\u{1f}Indian Lofi Beats\u{1f}https://soundcloud.com/lofi-beats-1/tu-hi",
        )
        .unwrap();
        assert_eq!(h.id, "1250206453");
        assert_eq!(h.duration, Some(356));
        assert_eq!(h.channel, "Indian Lofi Beats");
        assert_eq!(h.url, "https://soundcloud.com/lofi-beats-1/tu-hi");
        // without a link there's nothing to download.
        assert_eq!(
            parse_line(sc, "1250206453\u{1f}1\u{1f}t\u{1f}c\u{1f}NA"),
            None
        );
    }

    #[test]
    fn args() {
        let a = |src| -> Vec<String> {
            build_args(
                src,
                "lofi\nbeats",
                10,
                &["--cookies".into(), "c.txt".into()],
            )
            .into_iter()
            .map(|s| s.into_string().unwrap())
            .collect()
        };
        let yt = a(Source::YouTube);
        assert_eq!(
            &yt[yt.len() - 4..],
            ["--cookies", "c.txt", "--", "ytsearch10:lofi beats"]
        );
        assert!(yt.contains(&"--flat-playlist".to_owned()));
        assert_eq!(
            a(Source::SoundCloud).last().unwrap(),
            "scsearch10:lofi beats"
        );
    }

    #[test]
    fn sources_roundtrip() {
        for s in Source::ALL {
            assert_eq!(Source::from_key(s.key()), Some(s));
            assert_eq!(Source::from_index(s.index() as u64), Some(s));
        }
        assert_eq!(Source::from_index(2), None);
    }
}
