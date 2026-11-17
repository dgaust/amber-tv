//! Amber: the viewer's own arrangement of the sidebar and of Home's rows - their order and which
//! are hidden - kept in one small JSON file beside the app's other persistent files, so it
//! survives a reboot and a reinstall. Identities, not positions: a sidebar entry is `search`,
//! `home`, `watchlist`, `livetv` or `lib:<section key>`; a Home row is its hub identifier. An
//! entry the file does not name keeps its default place after the named ones.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Layout {
    pub sidebar_order: Vec<String>,
    pub sidebar_hidden: Vec<String>,
    pub home_order: Vec<String>,
    pub home_hidden: Vec<String>,
}

fn path() -> PathBuf {
    let id = plx_base::paths::app_id();
    let preferred = PathBuf::from(format!("/media/developer/{id}-layout.json"));
    if cfg!(feature = "hostsim") || !std::path::Path::new("/media/developer").is_dir() {
        return plx_base::paths::runtime_dir().join("amber-layout.json");
    }
    preferred
}

impl Layout {
    pub(crate) fn load() -> Self {
        std::fs::read(path()).ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    pub(crate) fn save(&self) {
        let Ok(bytes) = serde_json::to_vec_pretty(self) else { return };
        let target = path();
        let tmp = target.with_extension("json.tmp");
        if std::fs::write(&tmp, bytes).is_ok() {
            let _ = std::fs::rename(&tmp, &target);
        }
    }

    /// `ids` in the viewer's order: the ones the layout names first, in its order, then the rest
    /// in their given order.
    pub(crate) fn arrange<'a>(order: &[String], ids: &'a [String]) -> Vec<&'a String> {
        let mut out: Vec<&String> = ids.iter().collect();
        out.sort_by_key(|id| order.iter().position(|o| o == *id).unwrap_or(usize::MAX));
        out
    }

    /// Move `id` one place up (`-1`) or down (`+1`) within `visible`, the current arrangement,
    /// and keep the result as the whole order.
    pub(crate) fn step(order: &mut Vec<String>, visible: &[String], id: &str, dir: i32) {
        let mut all: Vec<String> = visible.to_vec();
        let Some(i) = all.iter().position(|x| x == id) else { return };
        let j = i as i32 + dir;
        if j < 0 || j as usize >= all.len() { return; }
        all.swap(i, j as usize);
        *order = all;
    }

    pub(crate) fn toggle(hidden: &mut Vec<String>, id: &str) {
        if let Some(i) = hidden.iter().position(|x| x == id) { hidden.remove(i); } else { hidden.push(id.to_owned()); }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn arrange_puts_named_first_and_keeps_the_rest_in_order() {
        let ids: Vec<String> = ["search", "home", "lib:1", "lib:2", "watchlist"].iter().map(|s| s.to_string()).collect();
        let order = vec!["lib:2".to_string(), "home".to_string()];
        let got: Vec<&str> = Layout::arrange(&order, &ids).into_iter().map(|s| s.as_str()).collect();
        assert_eq!(got, ["lib:2", "home", "search", "lib:1", "watchlist"]);
    }
    #[test]
    fn step_moves_within_the_arrangement() {
        let visible: Vec<String> = ["a", "b", "c"].iter().map(|s| s.to_string()).collect();
        let mut order = Vec::new();
        Layout::step(&mut order, &visible, "c", -1);
        assert_eq!(order, ["a", "c", "b"]);
    }
}
