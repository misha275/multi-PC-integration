//! Helpers for file transfer: what to send, and where a received file may go.

use anyhow::Result;
use std::path::{Path, PathBuf};

/// A file to send: where it is on disk, and its `/`-separated path on the receiver.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutgoingFile {
    pub path: PathBuf,
    pub rel_path: String,
    pub size: u64,
}

/// Expand files and folders into the list of files to send. A folder keeps its
/// name and inner structure on the receiving side.
pub fn collect(paths: &[PathBuf]) -> Result<Vec<OutgoingFile>> {
    let mut out = Vec::new();
    for p in paths {
        let meta = std::fs::metadata(p)?;
        let name =
            p.file_name().map(|n| n.to_string_lossy().into_owned()).ok_or_else(|| anyhow::anyhow!("{} has no file name", p.display()))?;
        if meta.is_dir() {
            walk(p, &name, &mut out)?;
        } else {
            out.push(OutgoingFile { path: p.clone(), rel_path: name, size: meta.len() });
        }
    }
    Ok(out)
}

fn walk(dir: &Path, rel: &str, out: &mut Vec<OutgoingFile>) -> Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)?.collect::<std::io::Result<_>>()?;
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let rel = format!("{rel}/{}", e.file_name().to_string_lossy());
        let ty = e.file_type()?;
        if ty.is_dir() {
            walk(&e.path(), &rel, out)?;
        } else if ty.is_file() {
            out.push(OutgoingFile { path: e.path(), rel_path: rel, size: e.metadata()?.len() });
        }
    }
    Ok(())
}

/// Turn a path received from a peer into a safe relative path, or `None` if it
/// tries to escape the download folder or is not a valid file name on Windows.
pub fn sanitize_rel_path(rel: &str) -> Option<PathBuf> {
    const RESERVED: &[&str] = &[
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4",
        "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    let mut out = PathBuf::new();
    for part in rel.split(['/', '\\']) {
        if part.is_empty() || part == "." || part == ".." {
            return None;
        }
        if part.chars().any(|c| c.is_control() || "<>:\"|?*".contains(c)) {
            return None;
        }
        if part.ends_with(['.', ' ']) {
            return None;
        }
        let stem = part.split('.').next().unwrap_or("").to_ascii_uppercase();
        if RESERVED.contains(&stem.as_str()) {
            return None;
        }
        out.push(part);
    }
    (!out.as_os_str().is_empty()).then_some(out)
}

/// `dir/rel`, or `dir/name (1).ext`, `dir/name (2).ext`, ... if that already exists.
pub fn unique_path(dir: &Path, rel: &Path) -> PathBuf {
    let first = dir.join(rel);
    if !first.exists() {
        return first;
    }
    let stem = rel.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let ext = rel.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    let parent = first.parent().map(Path::to_path_buf).unwrap_or_else(|| dir.to_path_buf());
    (1..).map(|i| parent.join(format!("{stem} ({i}){ext}"))).find(|p| !p.exists()).expect("infinite iterator")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_accepts_normal_paths() {
        assert_eq!(sanitize_rel_path("a.txt"), Some(PathBuf::from("a.txt")));
        assert_eq!(sanitize_rel_path("Фото/2024/img.jpg"), Some(["Фото", "2024", "img.jpg"].iter().collect()));
        assert_eq!(sanitize_rel_path("dir\\file"), Some(["dir", "file"].iter().collect()));
    }

    #[test]
    fn sanitize_rejects_escapes() {
        for bad in ["", "../x", "a/../../x", "/etc/passwd", "C:/Windows/x", "a//b", "con.txt", "x/NUL", "a:b", "trail."] {
            assert_eq!(sanitize_rel_path(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn collect_and_unique() {
        let tmp = std::env::temp_dir().join(format!("mpc-files-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("folder/sub")).unwrap();
        std::fs::write(tmp.join("folder/a.txt"), b"hello").unwrap();
        std::fs::write(tmp.join("folder/sub/b.bin"), [0u8; 10]).unwrap();
        std::fs::write(tmp.join("single.txt"), b"x").unwrap();

        let files = collect(&[tmp.join("folder"), tmp.join("single.txt")]).unwrap();
        let rels: Vec<_> = files.iter().map(|f| (f.rel_path.as_str(), f.size)).collect();
        assert_eq!(rels, [("folder/a.txt", 5), ("folder/sub/b.bin", 10), ("single.txt", 1)]);

        assert_eq!(unique_path(&tmp, Path::new("new.txt")), tmp.join("new.txt"));
        assert_eq!(unique_path(&tmp, Path::new("single.txt")), tmp.join("single (1).txt"));
        std::fs::write(tmp.join("single (1).txt"), b"x").unwrap();
        assert_eq!(unique_path(&tmp, Path::new("single.txt")), tmp.join("single (2).txt"));
        std::fs::remove_dir_all(&tmp).unwrap();
    }
}
