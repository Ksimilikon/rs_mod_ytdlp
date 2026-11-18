//! Link cleanup: drops query parameters that don't identify the video
//! (playlist position, share tracking, timestamps, ...), so the same video
//! always gives the same link and yt-dlp downloads exactly one video.
//!
//! Only YouTube links are touched; any other link is passed through as is
//! (yt-dlp supports many sites).

use url::Url;

/// what to remove from a YouTube link. Every flag is independent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CleanFlags {
    /// `list`, `index`, `start_radio`, `playnext`: the link points to one
    /// video, not to its place in a playlist / mix. A link without a video
    /// (`/playlist?list=…`) is then rejected.
    pub playlist: bool,
    /// share / referral tracking: `si`, `feature`, `pp`, `utm_*`, ...
    pub tracking: bool,
    /// start / end time: `t`, `start`, `end`, `time_continue`, `#t=…`.
    pub time: bool,
    /// any other parameter YouTube doesn't need to find the video.
    pub other: bool,
    /// rewrite to `https://www.youtube.com/watch?v=<id>` (shorts, youtu.be,
    /// music.youtube.com, embed, live, mobile — all become one form).
    pub canonical: bool,
}

impl Default for CleanFlags {
    fn default() -> Self {
        Self {
            playlist: true,
            tracking: true,
            time: true,
            other: true,
            canonical: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CleanError {
    /// not a link at all.
    Invalid,
    /// a YouTube link without a video while playlists are stripped.
    NotSingleVideo,
}

impl std::fmt::Display for CleanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Invalid => "not a valid link",
            Self::NotSingleVideo => "the link has no single video (playlist / channel); turn off \"remove playlist\" to download it",
        })
    }
}

const PLAYLIST: &[&str] = &["list", "index", "start_radio", "playnext"];
const TRACKING: &[&str] = &[
    "si",
    "feature",
    "pp",
    "ab_channel",
    "app",
    "kw",
    "fbclid",
    "gclid",
    "igshid",
    "embeds_referring_euri",
    "embeds_referring_origin",
    "embeds_euri",
    "embeds_origin",
    "source_ve_path",
];
const TIME: &[&str] = &["t", "start", "end", "time_continue"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Playlist,
    Tracking,
    Time,
    Other,
}

fn kind(key: &str) -> Kind {
    if PLAYLIST.contains(&key) {
        Kind::Playlist
    } else if TRACKING.contains(&key) || key.starts_with("utm_") {
        Kind::Tracking
    } else if TIME.contains(&key) {
        Kind::Time
    } else {
        Kind::Other
    }
}

/// a YouTube host without `www.` / `m.`; `None` for other sites.
fn youtube_host(host: &str) -> Option<&'static str> {
    let host = host.to_ascii_lowercase();
    let host = host
        .strip_prefix("www.")
        .or_else(|| host.strip_prefix("m."))
        .unwrap_or(&host);
    Some(match host {
        "youtube.com" => "youtube.com",
        "music.youtube.com" => "music.youtube.com",
        "youtu.be" => "youtu.be",
        "youtube-nocookie.com" => "youtube-nocookie.com",
        _ => return None,
    })
}

/// video ids are 11 chars of `[A-Za-z0-9_-]`; the length is not enforced in
/// case YouTube ever changes it.
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// the video id from the path: `/shorts/ID`, `/live/ID`, `/embed/ID`, ...
fn id_from_path(url: &Url, host: &str) -> Option<String> {
    let mut segs = url.path_segments()?.filter(|s| !s.is_empty());
    let id = if host == "youtu.be" {
        segs.next()?
    } else {
        match segs.next()? {
            "shorts" | "live" | "embed" | "v" | "e" => segs.next()?,
            _ => return None,
        }
    };
    valid_id(id).then(|| id.to_owned())
}

/// cleans `input` according to `flags`. A link without a scheme
/// (`youtu.be/…`) gets `https://`.
pub fn clean(input: &str, flags: CleanFlags) -> Result<String, CleanError> {
    let input = input.trim();
    if input.is_empty() || input.chars().any(char::is_whitespace) {
        return Err(CleanError::Invalid);
    }
    let with_scheme = if input.contains("://") {
        input.to_owned()
    } else {
        format!("https://{input}")
    };
    let mut url = Url::parse(&with_scheme).map_err(|_| CleanError::Invalid)?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(CleanError::Invalid);
    }
    let Some(host) = url.host_str().and_then(youtube_host) else {
        // another site: yt-dlp knows what to do with it.
        return Ok(url.into());
    };

    let pairs: Vec<(String, String)> = url
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    let id = id_from_path(&url, host).or_else(|| {
        pairs
            .iter()
            .find(|(k, _)| k == "v")
            .map(|(_, v)| v.clone())
            .filter(|v| valid_id(v))
    });
    let kept: Vec<&(String, String)> = pairs
        .iter()
        .filter(|(k, _)| k != "v")
        .filter(|(k, _)| match kind(k) {
            Kind::Playlist => !flags.playlist,
            Kind::Tracking => !flags.tracking,
            Kind::Time => !flags.time,
            Kind::Other => !flags.other,
        })
        .collect();

    let Some(id) = id else {
        if flags.playlist {
            return Err(CleanError::NotSingleVideo);
        }
        // a playlist / channel link: only the parameters are cleaned.
        set_query(&mut url, None, &kept);
        if flags.time {
            url.set_fragment(None);
        }
        return Ok(url.into());
    };

    if flags.canonical {
        let mut url = Url::parse("https://www.youtube.com/watch").expect("static url");
        set_query(&mut url, Some(&id), &kept);
        return Ok(url.into());
    }
    // keep the link's own form; `v` stays only where it was (`/watch`).
    let had_v = pairs.iter().any(|(k, _)| k == "v");
    set_query(&mut url, had_v.then_some(id.as_str()), &kept);
    if flags.time {
        url.set_fragment(None);
    }
    Ok(url.into())
}

fn set_query(url: &mut Url, id: Option<&str>, kept: &[&(String, String)]) {
    if id.is_none() && kept.is_empty() {
        url.set_query(None);
        return;
    }
    let mut q = url.query_pairs_mut();
    q.clear();
    if let Some(id) = id {
        q.append_pair("v", id);
    }
    for (k, v) in kept {
        q.append_pair(k, v);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: CleanFlags = CleanFlags {
        playlist: true,
        tracking: true,
        time: true,
        other: true,
        canonical: true,
    };
    const NONE: CleanFlags = CleanFlags {
        playlist: false,
        tracking: false,
        time: false,
        other: false,
        canonical: false,
    };
    const W: &str = "https://www.youtube.com/watch?v=dQw4w9WgXcQ";

    #[test]
    fn every_form_becomes_canonical() {
        for link in [
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ&list=PL123&index=4&si=abc&t=42s",
            "https://youtu.be/dQw4w9WgXcQ?si=xyz&t=10",
            "youtu.be/dQw4w9WgXcQ",
            "https://m.youtube.com/watch?feature=share&v=dQw4w9WgXcQ",
            "https://music.youtube.com/watch?v=dQw4w9WgXcQ&list=RDAMVM&pp=ygUE",
            "https://www.youtube.com/shorts/dQw4w9WgXcQ?feature=share",
            "https://www.youtube.com/live/dQw4w9WgXcQ?si=1",
            "https://www.youtube-nocookie.com/embed/dQw4w9WgXcQ?start=5",
            "  https://YOUTUBE.com/watch?v=dQw4w9WgXcQ&ab_channel=Rick#t=3  ",
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ&utm_source=x&foo=bar",
        ] {
            assert_eq!(clean(link, ALL).unwrap(), W, "{link}");
        }
    }

    #[test]
    fn flags_are_independent() {
        let link = "https://www.youtube.com/watch?v=dQw4w9WgXcQ&list=PL1&si=s&t=9&foo=1";
        let only = |f: CleanFlags| clean(link, f).unwrap();
        assert_eq!(
            only(CleanFlags {
                playlist: true,
                ..NONE
            }),
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ&si=s&t=9&foo=1"
        );
        assert_eq!(
            only(CleanFlags {
                tracking: true,
                ..NONE
            }),
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ&list=PL1&t=9&foo=1"
        );
        assert_eq!(
            only(CleanFlags { time: true, ..NONE }),
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ&list=PL1&si=s&foo=1"
        );
        assert_eq!(
            only(CleanFlags {
                other: true,
                ..NONE
            }),
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ&list=PL1&si=s&t=9"
        );
        assert_eq!(only(NONE), link);
    }

    #[test]
    fn non_canonical_keeps_the_form() {
        let f = CleanFlags {
            canonical: false,
            ..ALL
        };
        assert_eq!(
            clean("https://youtu.be/dQw4w9WgXcQ?si=xyz", f).unwrap(),
            "https://youtu.be/dQw4w9WgXcQ"
        );
        assert_eq!(
            clean(
                "https://www.youtube.com/shorts/dQw4w9WgXcQ?feature=share",
                f
            )
            .unwrap(),
            "https://www.youtube.com/shorts/dQw4w9WgXcQ"
        );
        assert_eq!(
            clean("https://music.youtube.com/watch?v=dQw4w9WgXcQ&list=RD", f).unwrap(),
            "https://music.youtube.com/watch?v=dQw4w9WgXcQ"
        );
    }

    #[test]
    fn playlists() {
        let pl = "https://www.youtube.com/playlist?list=PLabc&si=x";
        assert_eq!(clean(pl, ALL), Err(CleanError::NotSingleVideo));
        let f = CleanFlags {
            playlist: false,
            ..ALL
        };
        assert_eq!(
            clean(pl, f).unwrap(),
            "https://www.youtube.com/playlist?list=PLabc"
        );
        // a video in a playlist with playlists allowed keeps `list`.
        assert_eq!(
            clean("https://youtu.be/dQw4w9WgXcQ?list=PLabc&index=2", f).unwrap(),
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ&list=PLabc&index=2"
        );
    }

    #[test]
    fn other_sites_and_garbage() {
        assert_eq!(
            clean("https://soundcloud.com/a/b?si=1", ALL).unwrap(),
            "https://soundcloud.com/a/b?si=1"
        );
        assert_eq!(clean("", ALL), Err(CleanError::Invalid));
        assert_eq!(clean("not a link", ALL), Err(CleanError::Invalid));
        assert_eq!(
            clean("ftp://youtube.com/watch?v=dQw4w9WgXcQ", ALL),
            Err(CleanError::Invalid)
        );
        // never turns into a yt-dlp option.
        assert!(clean("-o/etc/passwd", ALL).map_or(true, |u| u.starts_with("https://")));
        // a broken id is not trusted: there is no video.
        assert_eq!(
            clean("https://www.youtube.com/watch?v=<script>", ALL),
            Err(CleanError::NotSingleVideo)
        );
    }
}
