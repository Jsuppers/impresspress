//! Asset-overlay logic shared by the four flow quadrants. Reads
//! `[[assets.overlay]]` entries from `impresspress.toml` and copies each
//! `from` (relative to repo root) to `to` (relative to the dist dir). An
//! entry may name either a file or a directory — a directory is copied
//! recursively, which is what the dev-sandbox example uses to overlay its
//! whole `seed/` tree onto `dist/seed/` in one entry.

use std::path::Path;

use anyhow::{anyhow, Result};

use crate::cli::config::Config;

pub fn apply_overlays(cfg: &Config, repo_root: &Path, dist_dir: &Path) -> Result<()> {
    for overlay in &cfg.assets.overlay {
        let src = repo_root.join(&overlay.from);
        let dst = dist_dir.join(&overlay.to);
        if src.is_dir() {
            copy_dir_all(&src, &dst)?;
        } else {
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| anyhow!("create dir {parent:?}: {e}"))?;
            }
            std::fs::copy(&src, &dst).map_err(|e| anyhow!("overlay {src:?} → {dst:?}: {e}"))?;
        }
    }
    Ok(())
}

/// Recursively copy every entry under `src` into `dst`, creating directories
/// as needed. The directory half of [`apply_overlays`] — a plain `fs::copy`
/// only ever handles the file-to-file case.
fn copy_dir_all(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst).map_err(|e| anyhow!("create dir {dst:?}: {e}"))?;
    for entry in std::fs::read_dir(src).map_err(|e| anyhow!("read dir {src:?}: {e}"))? {
        let entry = entry.map_err(|e| anyhow!("read dir entry under {src:?}: {e}"))?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if src_path.is_dir() {
            copy_dir_all(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path)
                .map_err(|e| anyhow!("overlay {src_path:?} → {dst_path:?}: {e}"))?;
        }
    }
    Ok(())
}

pub use crate::cli::config::OverlayEntry as Overlay;

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    /// A minimal `Config` whose `[[assets.overlay]]` entries are exactly
    /// `entries`.
    fn overlay_cfg(entries: &[(&str, &str)]) -> Config {
        let mut toml =
            String::from("[app]\nname = \"x\"\ntitle = \"X\"\nboot_redirect = \"/\"\n\n[assets]\n");
        for (from, to) in entries {
            toml.push_str("[[assets.overlay]]\n");
            toml.push_str(&format!("from = \"{from}\"\n"));
            toml.push_str(&format!("to = \"{to}\"\n"));
        }
        crate::cli::config::parse(&toml).unwrap()
    }

    #[test]
    fn directory_overlay_copies_recursively() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("seed/a")).unwrap();
        fs::write(root.join("seed/a/b.txt"), b"hi").unwrap();

        let cfg = overlay_cfg(&[("seed", "seed")]);
        let dist = root.join("dist");
        fs::create_dir_all(&dist).unwrap();
        apply_overlays(&cfg, root, &dist).unwrap();

        assert_eq!(fs::read_to_string(dist.join("seed/a/b.txt")).unwrap(), "hi");
    }

    #[test]
    fn file_overlay_still_works() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(root.join("index.html"), b"<h1>hi</h1>").unwrap();

        let cfg = overlay_cfg(&[("index.html", "index.html")]);
        let dist = root.join("dist");
        fs::create_dir_all(&dist).unwrap();
        apply_overlays(&cfg, root, &dist).unwrap();

        assert_eq!(
            fs::read_to_string(dist.join("index.html")).unwrap(),
            "<h1>hi</h1>"
        );
    }
}
