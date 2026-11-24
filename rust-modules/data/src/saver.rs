//! Amber: the screensaver's pictures - fanart of the films the viewer's server holds that are rated
//! M and under. The application fills the list in the background; the screensaver page reads it.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

/// One picture: where it is (a server path on server `sid`) and what it is of.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SaverImage {
    pub sid: u16,
    pub art: String,
    pub title: String,
    pub year: i64,
    /// The classification as it reads on its own ("M", "PG"): the server's text with the country
    /// before the slash dropped ("au/M"). Plex has no classification artwork to show instead.
    pub rating: String,
}

static IMAGES: Mutex<Vec<SaverImage>> = Mutex::new(Vec::new());
static GEN: AtomicU32 = AtomicU32::new(0);

pub fn set(images: Vec<SaverImage>) {
    *IMAGES.lock().unwrap_or_else(|e| e.into_inner()) = images;
    GEN.fetch_add(1, Ordering::Relaxed);
}

pub fn images() -> Vec<SaverImage> {
    IMAGES.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

pub fn count() -> usize {
    IMAGES.lock().unwrap_or_else(|e| e.into_inner()).len()
}

/// Moves whenever the list is replaced.
pub fn generation() -> u32 {
    GEN.load(Ordering::Relaxed)
}

/// "au/MA 15+" is "MA 15+": the text after the country and its slash.
pub fn rating_label(content_rating: &str) -> String {
    content_rating.rsplit('/').next().unwrap_or("").trim().to_string()
}

/// Is a film or show with this content rating one the screensaver may show? Ratings M and under: G, PG and
/// M (and TV-Y to TV-PG), however the server spells them ("M", "au/M", "gb/PG", "G"). Anything else - PG-13, R, MA15+,
/// R18+, an unrated film - stays out.
pub fn rating_allowed(content_rating: &str) -> bool {
    let tail = content_rating.rsplit('/').next().unwrap_or("").trim();
    // the TV scale's equivalents ride along: TV-Y, TV-Y7, TV-G and TV-PG are at or under PG
    matches!(tail.to_ascii_uppercase().as_str(), "G" | "PG" | "M" | "TV-Y" | "TV-Y7" | "TV-Y7-FV" | "TV-G" | "TV-PG")
}

#[cfg(test)]
mod tests {
    use super::{rating_allowed, rating_label};
    #[test]
    fn the_country_is_dropped() {
        assert_eq!(rating_label("au/MA 15+"), "MA 15+");
        assert_eq!(rating_label("au/M"), "M");
        assert_eq!(rating_label("PG"), "PG");
        assert_eq!(rating_label(""), "");
    }
    #[test]
    fn m_and_under_only() {
        for ok in ["G", "PG", "M", "au/M", "au/PG", "gb/G", " m ", "pg", "TV-PG", "TV-G", "TV-Y7"] {
            assert!(rating_allowed(ok), "{ok}");
        }
        for no in ["", "PG-13", "R", "MA15+", "R18+", "au/MA15+", "NC-17", "X18+", "NR", "Not Rated", "TV-MA", "TV-14", "au/M15+", "au/Revoked", "15", "18"] {
            assert!(!rating_allowed(no), "{no}");
        }
    }
}
