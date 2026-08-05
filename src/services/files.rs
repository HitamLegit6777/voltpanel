//! File manager: list/read/write/upload/download, create, rename, move,
//! copy, delete, chmod, archive zip/tar.gz, size.
use crate::config::Config;
use crate::models::Server;
use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, serde::Serialize, serde::Deserialize, Clone)]
pub struct FileEntry {
    pub name: String,
    pub path: String,
    pub size: u64,
    pub modified: String,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub mode: u32,
    pub mime: String,
    pub extension: String,
}

pub fn server_root(cfg: &Config, server: &Server) -> PathBuf {
    cfg.paths.servers_dir.join(&server.uuid)
}

/// Resolve a relative request path safely inside the server dir.
pub fn resolve(cfg: &Config, server: &Server, rel: &str) -> Result<PathBuf> {
    let root = server_root(cfg, server);
    let rel = rel.trim_start_matches('/');
    let p = root.join(rel);
    // forbid path traversal components
    for comp in p.components() {
        if let Component::ParentDir = comp {
            bail!("path traversal not allowed");
        }
    }
    if cfg.security.allow_cross_server_dir {
        return Ok(p);
    }
    // containment check; when the final component does not exist yet,
    // canonicalize the deepest existing ancestor (catches symlinked parents).
    let abs = p.canonicalize().unwrap_or_else(|_| {
        p.parent()
            .and_then(|par| par.canonicalize().ok())
            .map(|par| par.join(p.file_name().unwrap_or_default()))
            .unwrap_or_else(|| p.clone())
    });
    let root_abs = root.canonicalize().unwrap_or_else(|_| root.clone());
    if !abs.starts_with(&root_abs) {
        bail!("path escapes server directory");
    }
    Ok(p)
}

/// Join an archive entry name onto `dest`, lexically normalizing `.`/`..`
/// components and rejecting any escape from `dest` (zip-slip / tar-slip safe).
pub fn safe_join(dest: &Path, name: &str) -> Result<PathBuf> {
    let mut out = dest.to_path_buf();
    for comp in Path::new(name).components() {
        match comp {
            Component::Normal(c) => {
                out.push(c);
                if let Ok(meta) = fs::symlink_metadata(&out) {
                    if meta.file_type().is_symlink() {
                        bail!("archive path contains symlink")
                    }
                }
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    bail!("archive entry escapes destination")
                }
            }
            Component::RootDir | Component::Prefix(_) => {
                bail!("archive entry uses an absolute path")
            }
        }
    }
    if !out.starts_with(dest) {
        bail!("archive entry escapes destination")
    }
    Ok(out)
}

pub fn list_dir(cfg: &Config, server: &Server, rel: &str) -> Result<Vec<FileEntry>> {
    let path = resolve(cfg, server, rel)?;
    let mut out = Vec::new();
    let rd = fs::read_dir(&path).with_context(|| format!("cannot read {}", path.display()))?;
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let meta = entry.metadata()?;
        let p = entry.path();
        let rel_path = p
            .strip_prefix(server_root(cfg, server))
            .unwrap_or(&p)
            .to_string_lossy()
            .replace('\\', "/");
        let mime = mime_guess::from_path(&name)
            .first_or_octet_stream()
            .to_string();
        let ext = Path::new(&name)
            .extension()
            .map(|e| e.to_string_lossy().to_string())
            .unwrap_or_default();
        out.push(FileEntry {
            name,
            path: format!("/{}", rel_path),
            size: if meta.is_dir() { 0 } else { meta.len() },
            modified: meta
                .modified()
                .map(|m| {
                    m.duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0)
                        .to_string()
                })
                .unwrap_or_default(),
            is_dir: meta.is_dir(),
            is_symlink: meta.file_type().is_symlink(),
            mode: file_mode(&p),
            mime: if meta.is_dir() {
                "inode/directory".into()
            } else {
                mime
            },
            extension: ext,
        });
    }
    out.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(out)
}

fn file_mode(p: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    p.metadata().map(|m| m.permissions().mode()).unwrap_or(0)
}

pub fn read_file(
    cfg: &Config,
    server: &Server,
    rel: &str,
    max_bytes: usize,
) -> Result<(Vec<u8>, String)> {
    let path = resolve(cfg, server, rel)?;
    if path.is_dir() {
        bail!("is a directory");
    }
    let meta = fs::metadata(&path)?;
    if meta.len() > max_bytes as u64 {
        bail!("file too large to view inline");
    }
    let bytes = fs::read(&path)?;
    let mime = mime_guess::from_path(&path)
        .first_or_octet_stream()
        .to_string();
    Ok((bytes, mime))
}

pub fn write_file(cfg: &Config, server: &Server, rel: &str, data: &[u8]) -> Result<()> {
    let path = resolve(cfg, server, rel)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, data)?;
    Ok(())
}

pub fn append_file(cfg: &Config, server: &Server, rel: &str, data: &[u8]) -> Result<()> {
    let path = resolve(cfg, server, rel)?;
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    f.write_all(data)?;
    Ok(())
}

pub fn create_file(cfg: &Config, server: &Server, rel: &str) -> Result<()> {
    let path = resolve(cfg, server, rel)?;
    fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&path)?;
    Ok(())
}

pub fn create_dir(cfg: &Config, server: &Server, rel: &str) -> Result<()> {
    let path = resolve(cfg, server, rel)?;
    fs::create_dir_all(&path)?;
    Ok(())
}

pub fn rename(cfg: &Config, server: &Server, from: &str, to: &str) -> Result<()> {
    let src = resolve(cfg, server, from)?;
    let dst = resolve(cfg, server, to)?;
    fs::rename(&src, &dst)?;
    Ok(())
}

pub fn move_into(cfg: &Config, server: &Server, from: &str, dest_dir: &str) -> Result<()> {
    let src = resolve(cfg, server, from)?;
    let dir = resolve(cfg, server, dest_dir)?;
    let name = src.file_name().context("no file name")?;
    let dst = dir.join(name);
    fs::rename(&src, &dst)?;
    Ok(())
}

pub fn copy(cfg: &Config, server: &Server, from: &str, to: &str) -> Result<()> {
    let src = resolve(cfg, server, from)?;
    let dst = resolve(cfg, server, to)?;
    if src.is_dir() {
        copy_dir_recursive(&src, &dst)?;
    } else {
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(&src, &dst)?;
    }
    Ok(())
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let s = entry.path();
        let d = dst.join(entry.file_name());
        if s.is_dir() {
            copy_dir_recursive(&s, &d)?;
        } else {
            fs::copy(&s, &d)?;
        }
    }
    Ok(())
}

pub fn delete(cfg: &Config, server: &Server, rel: &str) -> Result<()> {
    let path = resolve(cfg, server, rel)?;
    if !path.exists() {
        bail!("not found");
    }
    if path.is_dir() {
        fs::remove_dir_all(&path)?;
    } else {
        fs::remove_file(&path)?;
    }
    Ok(())
}

pub fn chmod(cfg: &Config, server: &Server, rel: &str, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let path = resolve(cfg, server, rel)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(mode))?;
    Ok(())
}

pub fn exists(cfg: &Config, server: &Server, rel: &str) -> bool {
    resolve(cfg, server, rel)
        .map(|p| p.exists())
        .unwrap_or(false)
}

// ---------------- Archive ----------------

pub fn zip_dir(cfg: &Config, server: &Server, rel: &str, out_abs: &Path) -> Result<u64> {
    let src = resolve(cfg, server, rel)?;
    let file = fs::File::create(out_abs)?;
    let mut zip = zip::ZipWriter::new(file);
    let options =
        zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    let base = src.clone();
    let mut total: u64 = 0;
    add_dir_to_zip(&mut zip, &src, &base, &options, &mut total)?;
    zip.finish()?;
    Ok(total)
}

fn add_dir_to_zip(
    zip: &mut zip::ZipWriter<fs::File>,
    dir: &Path,
    base: &Path,
    options: &zip::write::FileOptions,
    total: &mut u64,
) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let meta = fs::symlink_metadata(&path)?;
        if meta.file_type().is_symlink() {
            continue;
        }
        let name = path
            .strip_prefix(base)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        if meta.is_dir() {
            zip.add_directory(format!("{name}/"), *options)?;
            add_dir_to_zip(zip, &path, base, options, total)?;
        } else if meta.is_file() {
            zip.start_file(name, *options)?;
            let mut file = fs::File::open(&path)?;
            *total = total.saturating_add(meta.len());
            std::io::copy(&mut file, zip)?;
        }
    }
    Ok(())
}

pub fn unzip_into(cfg: &Config, server: &Server, archive_rel: &str, dest_rel: &str) -> Result<()> {
    let archive = resolve(cfg, server, archive_rel)?;
    let dest = resolve(cfg, server, dest_rel)?;
    fs::create_dir_all(&dest)?;
    let f = fs::File::open(&archive)?;
    let mut zip = zip::ZipArchive::new(f)?;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i)?;
        let out_path = safe_join(&dest, entry.name())?;
        if entry.is_dir() {
            fs::create_dir_all(&out_path)?;
            continue;
        }
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut f = fs::File::create(&out_path)?;
        std::io::copy(&mut entry, &mut f)?;
    }
    Ok(())
}

pub fn tar_gz_dir(cfg: &Config, server: &Server, rel: &str, out_abs: &Path) -> Result<u64> {
    let src = resolve(cfg, server, rel)?;
    let file = fs::File::create(out_abs)?;
    let enc = flate2::write::GzEncoder::new(file, flate2::Compression::default());
    let mut tar = tar::Builder::new(enc);
    tar.append_dir_all(".", &src)?;
    let enc = tar.into_inner()?;
    let file = enc.finish()?;
    let size = file.metadata()?.len();
    Ok(size)
}

pub fn extract_tar_gz_into(
    cfg: &Config,
    server: &Server,
    archive_rel: &str,
    dest_rel: &str,
) -> Result<()> {
    let archive = resolve(cfg, server, archive_rel)?;
    let dest = resolve(cfg, server, dest_rel)?;
    fs::create_dir_all(&dest)?;
    let file = fs::File::open(&archive)?;
    let dec = flate2::read::GzDecoder::new(file);
    let mut tar = tar::Archive::new(dec);
    tar.set_unpack_xattrs(false);
    for entry in tar.entries()? {
        let mut entry = entry?;
        let kind = entry.header().entry_type();
        if kind.is_symlink() || kind.is_hard_link() {
            bail!("archive link entries are forbidden")
        }
        let rel = entry.path()?.to_string_lossy().to_string();
        let path = safe_join(&dest, &rel)?;
        entry.unpack(&path)?;
    }
    Ok(())
}

pub fn download_bytes(cfg: &Config, server: &Server, rel: &str) -> Result<(String, Vec<u8>)> {
    let path = resolve(cfg, server, rel)?;
    let bytes = fs::read(&path)?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    Ok((name, bytes))
}

pub fn base64_upload(cfg: &Config, server: &Server, rel: &str, b64: &str) -> Result<()> {
    let bytes = STANDARD.decode(b64)?;
    write_file(cfg, server, rel, &bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_join_normalizes_and_blocks_escape() {
        let dest = Path::new("/srv");
        assert_eq!(
            safe_join(dest, "a/b.txt").unwrap(),
            PathBuf::from("/srv/a/b.txt")
        );
        assert_eq!(
            safe_join(dest, "a/./b.txt").unwrap(),
            PathBuf::from("/srv/a/b.txt")
        );
        assert_eq!(
            safe_join(dest, "a/../b.txt").unwrap(),
            PathBuf::from("/srv/b.txt")
        );
        assert!(safe_join(dest, "../evil.txt").is_err());
        assert!(safe_join(dest, "/abs.txt").is_err());
        assert!(safe_join(dest, "a/../../evil.txt").is_err());
    }

    #[test]
    fn safe_join_rejects_existing_symlink_parent() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let dest = temp.path().join("dest");
        std::fs::create_dir_all(&dest).unwrap();
        symlink("/etc", dest.join("escape")).unwrap();
        assert!(safe_join(&dest, "escape/passwd").is_err());
    }
}
