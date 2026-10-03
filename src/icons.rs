//! Vanilla Minecraft item icons, served from `{data_dir}/item-icons/`.
//!
//! The icons are extracted from the official Mojang client JAR (see
//! `scripts/extract-icons.py` — run once per Minecraft version). The panel
//! serves them at `/static/items/<name>.png`; no external icon API needed.

use std::path::{Path, PathBuf};

/// Directory holding `<item>.png` files.
pub fn icons_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("item-icons")
}

