//! YouTube search through yt-dlp (`ytsearchN:<query>`): a flat listing, no
//! download, one line per video — id, duration, title, channel.

use crate::ytdlp::{self, Running};
use std::{ffi::OsString, path::Path};

/// choices of the "Search results" select, in its order.
pub const COUNTS: [usize; 4] = [5, 10, 20, 30];
/// the most results a search can give (slots of the results list).
pub const MAX: usize = COUNTS[COUNTS.len() - 1];

/// separates the fields of a result line (titles never contain it).
const SEP: char = '\u{1f}';

/// one video found.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub id: String,
    pub title: String,
    pub channel: String,
    /// seconds; `None` — unknown (live streams, premieres).
    pub duration: Option<u64>,
}

impl Hit {
    pub fn url(&self) -> String {
        format!("https://www.youtube.com/watch?v={}", self.id)
    }
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
pub fn build_args(query: &str, count: usize, extra_args: &[String]) -> Vec<OsString> {
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
    a.push(format!("%(id)s{SEP}%(duration)s{SEP}%(title)s{SEP}%(channel,uploader|)s").into());
    a.extend(extra_args.iter().map(OsString::from));
    a.push("--".into());
    // newlines would end the query early; yt-dlp treats the rest as text.
    a.push(format!("ytsearch{count}:{}", query.replace(['\n', '\r'], " ")).into());
    a
}

/// one output line → a hit; anything else (warnings, junk) → `None`.
pub fn parse_line(line: &str) -> Option<Hit> {
    let mut f = line.trim_end_matches(['\r', '\n']).splitn(4, SEP);
    let id = f.next()?.trim();
    let duration = f.next()?.trim();
    let title = f.next()?.trim();
    let channel = f.next().unwrap_or("").trim();
    // a YouTube id: 11 chars of [A-Za-z0-9_-].
    if id.len() != 11
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return None;
    }
    Some(Hit {
        id: id.into(),
        title: if title.is_empty() || title == "NA" {
            id.into()
        } else {
            title.into()
        },
        channel: if channel == "NA" {
            String::new()
        } else {
            channel.into()
        },
        // flat entries give whole seconds, sometimes as `212.0`.
        duration: duration
            .parse::<f64>()
            .ok()
            .filter(|d| d.is_finite() && *d >= 0.0)
            .map(|d| d.round() as u64),
    })
}

/// runs the search to the end; a kill through `running` ends it with an
/// error.
pub fn search(
    ytdlp: &Path,
    query: &str,
    count: usize,
    extra_args: &[String],
    running: &Running,
) -> Result<Vec<Hit>, String> {
    let lines = ytdlp::run_lines(ytdlp, build_args(query, count, extra_args), running)?;
    Ok(lines.iter().filter_map(|l| parse_line(l)).collect())
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
    fn lines() {
        let h = parse_line("jNQXAC9IVRw\u{1f}19\u{1f}Me at the zoo\u{1f}jawed\n").unwrap();
        assert_eq!(
            h,
            Hit {
                id: "jNQXAC9IVRw".into(),
                title: "Me at the zoo".into(),
                channel: "jawed".into(),
                duration: Some(19)
            }
        );
        assert_eq!(h.url(), "https://www.youtube.com/watch?v=jNQXAC9IVRw");
        let live = parse_line("abcdefghijk\u{1f}NA\u{1f}Live\u{1f}NA").unwrap();
        assert_eq!((live.duration, live.channel.as_str()), (None, ""));
        assert_eq!(
            parse_line("abcdefghijk\u{1f}212.0\u{1f}T\u{1f}")
                .unwrap()
                .duration,
            Some(212)
        );
        assert_eq!(parse_line("WARNING: something"), None);
        assert_eq!(parse_line("bad id\u{1f}1\u{1f}t\u{1f}c"), None);
    }

    #[test]
    fn args() {
        let a: Vec<String> = build_args("lofi\nbeats", 10, &["--cookies".into(), "c.txt".into()])
            .into_iter()
            .map(|s| s.into_string().unwrap())
            .collect();
        assert_eq!(
            &a[a.len() - 4..],
            ["--cookies", "c.txt", "--", "ytsearch10:lofi beats"]
        );
        assert!(a.contains(&"--flat-playlist".to_owned()));
    }
}
