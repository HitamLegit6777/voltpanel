//! File manager: list/read/write/upload/download, create, rename, move,
//! copy, delete, chmod, archive zip/tar.gz, size.
use crate::config::Config;
use crate::models::Server;
use anyhow::{anyhow, bail, Context, Result};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::Uri;
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::client::legacy::connect::dns::Name;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use std::ffi::{CString, OsStr};
use std::fs;
use std::io::{Read, Write};
use std::net::SocketAddr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};
use tower_service::Service;

/// Hard limits applied while extracting archives into a server directory.
const MAX_ARCHIVE_ENTRIES: usize = 10_000;
const MAX_EXTRACT_FILE_BYTES: u64 = 512 * 1024 * 1024;
const MAX_EXTRACT_TOTAL_BYTES: u64 = 2 * 1024 * 1024 * 1024;

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
///
/// Paths are always contained: the legacy `allow_cross_server_dir` flag is
/// ignored and any symlinked component that escapes the server root is
/// rejected via canonicalization of the deepest existing ancestor.
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

/// Open the server root directory as a descriptor. The root itself is
/// admin-created, so it may be a symlink, but everything below it must be
/// walked with `O_NOFOLLOW`.
fn open_root_dir(root: &Path) -> Result<OwnedFd> {
    let c = CString::new(root.as_os_str().as_bytes()).map_err(|_| anyhow!("invalid root path"))?;
    let fd = unsafe {
        libc::open(
            c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("cannot open {}", root.display()));
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Walk `rel` below `root` with `openat` + `O_NOFOLLOW`, so no component is
/// ever resolved through a symlink and there is no check-then-use window.
/// Intermediate components must be directories; when `create_parents` is set
/// missing ones are created (still `O_NOFOLLOW`). `flags` (plus `mode`) apply
/// to the final component only.
fn open_relative(
    root: &OwnedFd,
    rel: &str,
    create_parents: bool,
    flags: i32,
    mode: libc::mode_t,
) -> Result<OwnedFd> {
    let rel = rel.trim_start_matches('/');
    let mut comps: Vec<&OsStr> = Vec::new();
    for c in Path::new(rel).components() {
        match c {
            Component::Normal(c) => comps.push(c),
            Component::CurDir => {}
            Component::ParentDir => bail!("path traversal not allowed"),
            Component::RootDir | Component::Prefix(_) => bail!("absolute path not allowed"),
        }
    }
    let mut holder = root.try_clone()?;
    if comps.is_empty() {
        return Ok(holder);
    }
    let mut cur = holder.as_raw_fd();
    let n = comps.len();
    for (i, name) in comps.iter().enumerate() {
        let last = i + 1 == n;
        let c = CString::new(name.as_bytes()).map_err(|_| anyhow!("invalid path component"))?;
        let oflags = if last {
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC
        } else {
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC
        };
        let f = unsafe { libc::openat(cur, c.as_ptr(), oflags, mode) };
        if f >= 0 {
            holder = unsafe { OwnedFd::from_raw_fd(f) };
            cur = holder.as_raw_fd();
            continue;
        }
        let err = std::io::Error::last_os_error();
        if !last && create_parents && err.kind() == std::io::ErrorKind::NotFound {
            // create the missing parent dir; O_CREAT|O_DIRECTORY is EINVAL,
            // so a directory must be made with mkdirat
            let mk = unsafe { libc::mkdirat(cur, c.as_ptr(), 0o755) };
            if mk != 0 {
                let mkerr = std::io::Error::last_os_error();
                if mkerr.kind() != std::io::ErrorKind::AlreadyExists {
                    return Err(mkerr)
                        .with_context(|| format!("cannot create {}", name.to_string_lossy()));
                }
            }
            let f2 = unsafe {
                libc::openat(
                    cur,
                    c.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if f2 < 0 {
                return Err(std::io::Error::last_os_error())
                    .with_context(|| format!("cannot open {}", name.to_string_lossy()));
            }
            holder = unsafe { OwnedFd::from_raw_fd(f2) };
            cur = holder.as_raw_fd();
            continue;
        }
        return Err(err).with_context(|| format!("cannot open {}", name.to_string_lossy()));
    }
    Ok(holder)
}

/// Canonicalize `p`, or the deepest existing ancestor when the tail does not
/// exist yet (same fallback logic as `resolve`).
fn canonical_maybe(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| {
        p.parent()
            .and_then(|par| par.canonicalize().ok())
            .map(|par| par.join(p.file_name().unwrap_or_default()))
            .unwrap_or_else(|| p.to_path_buf())
    })
}


pub fn list_dir(cfg: &Config, server: &Server, rel: &str) -> Result<Vec<FileEntry>> {
    let path = resolve(cfg, server, rel)?;
    let mut out = Vec::new();
    let rd = fs::read_dir(&path).with_context(|| format!("cannot read {}", path.display()))?;
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        // lstat: never follow symlinks, so no external metadata is observed
        let meta = fs::symlink_metadata(entry.path())?;
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
    fs::symlink_metadata(p)
        .map(|m| m.permissions().mode())
        .unwrap_or(0)
}

pub fn read_file(
    cfg: &Config,
    server: &Server,
    rel: &str,
    max_bytes: usize,
) -> Result<(Vec<u8>, String)> {
    let root = server_root(cfg, server);
    let rootfd = open_root_dir(&root)?;
    let fd = open_relative(&rootfd, rel, false, libc::O_RDONLY, 0)?;
    let file = fs::File::from(fd);
    let meta = file.metadata()?;
    if meta.is_dir() {
        bail!("is a directory");
    }
    if meta.len() > max_bytes as u64 {
        bail!("file too large to view inline");
    }
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    (&file).read_to_end(&mut bytes)?;
    let mime = mime_guess::from_path(root.join(rel))
        .first_or_octet_stream()
        .to_string();
    Ok((bytes, mime))
}

pub fn write_file(cfg: &Config, server: &Server, rel: &str, data: &[u8]) -> Result<()> {
    let root = server_root(cfg, server);
    fs::create_dir_all(&root)?;
    let rootfd = open_root_dir(&root)?;
    let fd = open_relative(
        &rootfd,
        rel,
        true,
        libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC,
        0o644,
    )?;
    let mut file = fs::File::from(fd);
    file.write_all(data)?;
    Ok(())
}

pub fn append_file(cfg: &Config, server: &Server, rel: &str, data: &[u8]) -> Result<()> {
    let root = server_root(cfg, server);
    let rootfd = open_root_dir(&root)?;
    let fd = open_relative(
        &rootfd,
        rel,
        false,
        libc::O_WRONLY | libc::O_CREAT | libc::O_APPEND,
        0o644,
    )?;
    let mut file = fs::File::from(fd);
    file.write_all(data)?;
    Ok(())
}

pub fn create_file(cfg: &Config, server: &Server, rel: &str) -> Result<()> {
    let root = server_root(cfg, server);
    let rootfd = open_root_dir(&root)?;
    let fd = open_relative(
        &rootfd,
        rel,
        false,
        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
        0o644,
    )?;
    drop(fd);
    Ok(())
}

pub fn create_dir(cfg: &Config, server: &Server, rel: &str) -> Result<()> {
    let root = server_root(cfg, server);
    let rootfd = open_root_dir(&root)?;
    let fd = open_relative(
        &rootfd,
        rel,
        true,
        libc::O_RDONLY | libc::O_CREAT | libc::O_DIRECTORY,
        0o755,
    )?;
    drop(fd);
    Ok(())
}

pub fn rename(cfg: &Config, server: &Server, from: &str, to: &str) -> Result<()> {
    // Containment pre-check, unchanged semantics (an escaping symlink in the
    // source or destination is refused here). The rename itself then runs on
    // pinned parent directory fds via `renameat2` (mirroring the
    // `download_pull_at` discipline), so a parent swapped for a symlink
    // between the check and the rename cannot redirect the move outside the
    // sandbox. `renameat2` never dereferences either entry — a symlink being
    // renamed moves as a link.
    let _ = resolve(cfg, server, from)?;
    let _ = resolve(cfg, server, to)?;
    let root = server_root(cfg, server);
    let rootfd = open_root_dir(&root)?;
    let from_parent = parent_rel(from);
    let to_parent = parent_rel(to);
    let from_name = Path::new(from.trim_start_matches('/'))
        .file_name()
        .context("source has no file name")?;
    let to_name = Path::new(to.trim_start_matches('/'))
        .file_name()
        .context("destination has no file name")?;
    if from_parent == to_parent && from_name == to_name {
        return Ok(()); // renaming a path onto itself is a no-op
    }
    let sdir =
        open_relative(&rootfd, &from_parent, false, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
    let ddir =
        open_relative(&rootfd, &to_parent, false, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
    let cfrom = CString::new(from_name.as_bytes()).map_err(|_| anyhow!("invalid source name"))?;
    let cto = CString::new(to_name.as_bytes()).map_err(|_| anyhow!("invalid destination name"))?;
    if unsafe {
        libc::renameat2(
            sdir.as_raw_fd(),
            cfrom.as_ptr(),
            ddir.as_raw_fd(),
            cto.as_ptr(),
            0,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error()).with_context(|| {
            format!(
                "cannot rename {} to {}",
                from.trim_start_matches('/'),
                to.trim_start_matches('/')
            )
        });
    }
    Ok(())
}

pub fn move_into(cfg: &Config, server: &Server, from: &str, dest_dir: &str) -> Result<()> {
    // Same discipline as `rename`: containment pre-check, then `renameat2` on
    // pinned parent fds. The destination must be an existing directory; its
    // fd pins the inode so a swapped directory can never redirect the move.
    let _ = resolve(cfg, server, from)?;
    let _ = resolve(cfg, server, dest_dir)?;
    let root = server_root(cfg, server);
    let rootfd = open_root_dir(&root)?;
    let from_parent = parent_rel(from);
    let from_name = Path::new(from.trim_start_matches('/'))
        .file_name()
        .context("source has no file name")?;
    let sdir =
        open_relative(&rootfd, &from_parent, false, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
    let ddir = open_relative(
        &rootfd,
        dest_dir.trim_start_matches('/'),
        false,
        libc::O_RDONLY | libc::O_DIRECTORY,
        0,
    )?;
    let cfrom = CString::new(from_name.as_bytes()).map_err(|_| anyhow!("invalid source name"))?;
    let cdst = CString::new(from_name.as_bytes()).map_err(|_| anyhow!("invalid destination name"))?;
    if unsafe {
        libc::renameat2(
            sdir.as_raw_fd(),
            cfrom.as_ptr(),
            ddir.as_raw_fd(),
            cdst.as_ptr(),
            0,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error()).with_context(|| {
            format!(
                "cannot move {} into {}",
                from.trim_start_matches('/'),
                dest_dir.trim_start_matches('/')
            )
        });
    }
    Ok(())
}

pub fn copy(cfg: &Config, server: &Server, from: &str, to: &str) -> Result<()> {
    // Containment is checked on the resolved paths (traversals, and a
    // top-level source symlink whose target escapes the root are rejected
    // here), but the actual copy walks pinned directory descriptors below the
    // root with O_NOFOLLOW, so a source swapped for a symlink mid-copy can
    // never redirect the reads outside the sandbox.
    let src_abs = resolve(cfg, server, from)?;
    let dst_abs = resolve(cfg, server, to)?;
    if canonical_maybe(&dst_abs).starts_with(canonical_maybe(&src_abs)) {
        bail!("cannot copy a directory into itself");
    }
    let root = server_root(cfg, server);
    let rootfd = open_root_dir(&root)?;
    let sname = Path::new(from.trim_start_matches('/'))
        .file_name()
        .context("source has no file name")?;
    let dname = Path::new(to.trim_start_matches('/'))
        .file_name()
        .context("destination has no file name")?;
    let src_parent = parent_rel(from);
    let dst_parent = parent_rel(to);
    let sdir = open_relative(&rootfd, &src_parent, false, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
    let ddir = open_relative(&rootfd, &dst_parent, true, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
    copy_entry(&sdir, sname, &ddir, dname)
}

/// Server-relative path of the parent of `rel` ("" for the root itself).
fn parent_rel(rel: &str) -> String {
    Path::new(rel.trim_start_matches('/'))
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Copy one directory entry from a pinned source dir to a pinned destination
/// dir. Every step is fd-relative with O_NOFOLLOW: a symlink is recreated as
/// a link (its target string is copied, never dereferenced) and a swapped
/// path can never redirect the copy outside the sandbox.
fn copy_entry<S: AsRawFd, D: AsRawFd>(
    src_dir: &S,
    name: &OsStr,
    dst_dir: &D,
    dst_name: &OsStr,
) -> Result<()> {
    let cname = CString::new(name.as_bytes()).map_err(|_| anyhow!("invalid entry name"))?;
    let cdst = CString::new(dst_name.as_bytes()).map_err(|_| anyhow!("invalid entry name"))?;
    let raw = unsafe {
        libc::openat(
            src_dir.as_raw_fd(),
            cname.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0,
        )
    };
    if raw < 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ELOOP) {
            // Symlink: recreate the link, never dereference its target.
            let target = read_link_at(src_dir, &cname)?;
            let ctarget = CString::new(target).map_err(|_| anyhow!("invalid link target"))?;
            if unsafe { libc::symlinkat(ctarget.as_ptr(), dst_dir.as_raw_fd(), cdst.as_ptr()) } != 0 {
                return Err(std::io::Error::last_os_error())
                    .with_context(|| format!("cannot create link {}", dst_name.to_string_lossy()));
            }
            return Ok(());
        }
        return Err(err).with_context(|| format!("cannot open {}", name.to_string_lossy()));
    }
    let src_file = fs::File::from(unsafe { OwnedFd::from_raw_fd(raw) });
    let meta = src_file.metadata()?;
    if meta.is_dir() {
        if unsafe { libc::mkdirat(dst_dir.as_raw_fd(), cdst.as_ptr(), 0o755) } != 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() != std::io::ErrorKind::AlreadyExists {
                return Err(err)
                    .with_context(|| format!("cannot create {}", dst_name.to_string_lossy()));
            }
        }
        let ndst_raw = unsafe {
            libc::openat(
                dst_dir.as_raw_fd(),
                cdst.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0,
            )
        };
        if ndst_raw < 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("cannot open {}", dst_name.to_string_lossy()));
        }
        let ndst = unsafe { OwnedFd::from_raw_fd(ndst_raw) };
        return copy_dir_entries(&src_file, &ndst);
    }
    // Regular file: create/truncate in the pinned destination dir. O_NOFOLLOW
    // refuses an existing destination symlink instead of writing through it.
    let out_raw = unsafe {
        libc::openat(
            dst_dir.as_raw_fd(),
            cdst.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o644,
        )
    };
    if out_raw < 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("cannot create {}", dst_name.to_string_lossy()));
    }
    let mut out = fs::File::from(unsafe { OwnedFd::from_raw_fd(out_raw) });
    std::io::copy(&mut &src_file, &mut out)
        .with_context(|| format!("cannot copy {}", name.to_string_lossy()))?;
    out.set_permissions(fs::Permissions::from_mode(meta.permissions().mode() & 0o777))?;
    Ok(())
}

fn read_link_at<S: AsRawFd>(dir: &S, name: &CString) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; 4096];
    let n = unsafe {
        libc::readlinkat(
            dir.as_raw_fd(),
            name.as_ptr(),
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len(),
        )
    };
    if n < 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("cannot read link {}", name.to_string_lossy()));
    }
    buf.truncate(n as usize);
    Ok(buf)
}

/// Recursively copy every entry of the pinned directory `src_dir` into the
/// pinned `dst_dir` (fdopendir on a duplicate of the source fd, entries
/// opened with O_NOFOLLOW via `copy_entry`).
fn copy_dir_entries<S: AsRawFd>(src_dir: &S, dst_dir: &OwnedFd) -> Result<()> {
    // fdopendir takes ownership of the fd, so hand it a duplicate.
    let dup = unsafe { libc::dup(src_dir.as_raw_fd()) };
    if dup < 0 {
        return Err(std::io::Error::last_os_error()).context("cannot duplicate directory fd");
    }
    let dirp = unsafe { libc::fdopendir(dup) };
    if dirp.is_null() {
        let err = std::io::Error::last_os_error();
        unsafe { libc::close(dup) };
        return Err(err).context("cannot open directory for copy");
    }
    let result = loop {
        unsafe { *libc::__errno_location() = 0 };
        let ent = unsafe { libc::readdir(dirp) };
        if ent.is_null() {
            if unsafe { *libc::__errno_location() } != 0 {
                break Err(std::io::Error::last_os_error()).context("cannot read directory");
            }
            break Ok(());
        }
        let name = unsafe { std::ffi::CStr::from_ptr((*ent).d_name.as_ptr()) }.to_bytes();
        if name == b"." || name == b".." {
            continue;
        }
        let name = std::ffi::OsStr::from_bytes(name);
        if let Err(e) = copy_entry(src_dir, name, dst_dir, name) {
            break Err(e);
        }
    };
    unsafe { libc::closedir(dirp) };
    result
}

/// Recursively delete `name` inside the pinned directory `dirfd`. Every step
/// is fd-relative with O_NOFOLLOW, mirroring the `copy_entry` /
/// `copy_dir_entries` discipline: the entry is classified with
/// `fstatat(AT_SYMLINK_NOFOLLOW)`, a symlink is unlinked without ever being
/// dereferenced, and a directory is pinned with `O_NOFOLLOW` before its
/// contents are removed fd-relative, so a swapped path can never redirect
/// the delete outside the sandbox.
fn delete_entry_recursive(dirfd: &OwnedFd, name: &CString) -> Result<()> {
    let mut stbuf: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe {
        libc::fstatat(
            dirfd.as_raw_fd(),
            name.as_ptr(),
            &mut stbuf,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    if (stbuf.st_mode & libc::S_IFMT) != libc::S_IFDIR {
        if unsafe { libc::unlinkat(dirfd.as_raw_fd(), name.as_ptr(), 0) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        return Ok(());
    }
    // Directory: pin it (O_NOFOLLOW; a swapped symlink fails with ELOOP),
    // delete the contents fd-relative, then remove the now-empty directory.
    let sub_raw = unsafe {
        libc::openat(
            dirfd.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0,
        )
    };
    if sub_raw < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let sub = unsafe { OwnedFd::from_raw_fd(sub_raw) };
    // fdopendir takes ownership of the fd, so hand it a duplicate.
    let dup = unsafe { libc::dup(sub.as_raw_fd()) };
    if dup < 0 {
        return Err(std::io::Error::last_os_error()).context("cannot duplicate directory fd");
    }
    let dirp = unsafe { libc::fdopendir(dup) };
    if dirp.is_null() {
        let err = std::io::Error::last_os_error();
        unsafe { libc::close(dup) };
        return Err(err).context("cannot open directory for delete");
    }
    let result = loop {
        unsafe { *libc::__errno_location() = 0 };
        let ent = unsafe { libc::readdir(dirp) };
        if ent.is_null() {
            if unsafe { *libc::__errno_location() } != 0 {
                break Err(std::io::Error::last_os_error()).context("cannot read directory");
            }
            break Ok(());
        }
        let entry_name = unsafe { std::ffi::CStr::from_ptr((*ent).d_name.as_ptr()) }.to_bytes();
        if entry_name == b"." || entry_name == b".." {
            continue;
        }
        let cname = CString::new(entry_name).map_err(|_| anyhow!("invalid entry name"))?;
        if let Err(e) = delete_entry_recursive(&sub, &cname) {
            break Err(e);
        }
    };
    unsafe { libc::closedir(dirp) };
    result?;
    // The directory is empty now. AT_REMOVEDIR (0x200, not exported by libc)
    // removes only directories and fails with ENOTEMPTY otherwise.
    if unsafe { libc::unlinkat(dirfd.as_raw_fd(), name.as_ptr(), 0x200) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

pub fn delete(cfg: &Config, server: &Server, rel: &str) -> Result<()> {
    // Containment pre-check, unchanged semantics (an escaping symlink is
    // refused). The delete itself then runs on pinned directory descriptors
    // via `delete_entry_recursive`, so a parent swapped for a symlink after
    // this check cannot redirect the removal outside the sandbox — replacing
    // the old `resolve`-then-`remove_dir_all` re-walk by path.
    let _ = resolve(cfg, server, rel)?;
    let trimmed = rel.trim_start_matches('/');
    // Never delete the sandbox root itself: there is no parent fd to pin
    // against, and the recursive walk would otherwise reach every file in
    // the server directory in one request.
    if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        bail!("refusing to delete the server root");
    }
    let root = server_root(cfg, server);
    let rootfd = open_root_dir(&root)?;
    let parent = parent_rel(rel);
    let name = Path::new(trimmed)
        .file_name()
        .context("path has no file name")?;
    let dirfd = open_relative(&rootfd, &parent, false, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
    let cname = CString::new(name.as_bytes()).map_err(|_| anyhow!("invalid entry name"))?;
    delete_entry_recursive(&dirfd, &cname)
        .with_context(|| format!("cannot delete {}", Path::new(trimmed).display()))
}

pub fn chmod(cfg: &Config, server: &Server, rel: &str, mode: u32) -> Result<()> {
    let root = server_root(cfg, server);
    let rootfd = open_root_dir(&root)?;
    // O_NOFOLLOW walk + fchmod on the pinned fd: a symlink (or one swapped in
    // under the path between resolve and use) is never chmodded — the open
    // fails with ELOOP instead. The mode is masked to rwx bits so setuid /
    // setgid / sticky are never granted on panel-managed files.
    let open_ro = || open_relative(&rootfd, rel, false, libc::O_RDONLY, 0);
    let fd = match open_ro() {
        Ok(fd) => fd,
        // A file without read permission can still be chmodded when writable.
        Err(e)
            if e.downcast_ref::<std::io::Error>()
                .map(std::io::Error::kind)
                == Some(std::io::ErrorKind::PermissionDenied) =>
        {
            open_relative(&rootfd, rel, false, libc::O_WRONLY, 0)?
        }
        Err(e) => return Err(e),
    };
    fs::File::from(fd).set_permissions(fs::Permissions::from_mode(mode & 0o777))?;
    Ok(())
}

pub fn exists(cfg: &Config, server: &Server, rel: &str) -> bool {
    resolve(cfg, server, rel)
        .map(|p| fs::symlink_metadata(&p).is_ok())
        .unwrap_or(false)
}

/// Open `rel` below `rootfd` as a directory, creating it — and any missing
/// parents — with `mkdirat` + `openat` O_NOFOLLOW walks. `open_relative`'s
/// `create_parents` only builds *intermediate* components; the final one must
/// be created explicitly because `O_CREAT|O_DIRECTORY` is EINVAL on Linux.
fn open_dir_create(rootfd: &OwnedFd, rel: &str) -> Result<OwnedFd> {
    let trimmed = rel.trim_start_matches('/');
    if trimmed.is_empty() || trimmed == "." {
        return open_relative(rootfd, "", false, libc::O_RDONLY | libc::O_DIRECTORY, 0);
    }
    let parent = parent_rel(trimmed);
    let name = Path::new(trimmed)
        .file_name()
        .context("path has no file name")?;
    let pfd = open_relative(rootfd, &parent, true, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
    let cname = CString::new(name.as_bytes()).map_err(|_| anyhow!("invalid path component"))?;
    // mkdirat is idempotent like create_dir_all: EEXIST is fine, but the
    // entry must then be a real directory (O_NOFOLLOW, no symlink followed).
    if unsafe { libc::mkdirat(pfd.as_raw_fd(), cname.as_ptr(), 0o755) } != 0 {
        let err = std::io::Error::last_os_error();
        if err.kind() != std::io::ErrorKind::AlreadyExists {
            return Err(err).with_context(|| format!("cannot create {}", name.to_string_lossy()));
        }
    }
    let raw = unsafe {
        libc::openat(
            pfd.as_raw_fd(),
            cname.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0,
        )
    };
    if raw < 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("cannot open {}", name.to_string_lossy()));
    }
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

// ---------------- Archive ----------------

/// (device, inode) identity of an open file, used to keep the archive walk
/// from consuming its own output file. Exact and race-free, unlike a path or
/// canonicalization comparison.
fn file_identity(file: &fs::File) -> (u64, u64) {
    let m = file.metadata().expect("fstat of open file");
    (m.dev(), m.ino())
}

pub fn zip_dir(cfg: &Config, server: &Server, rel: &str, out_abs: &Path) -> Result<u64> {
    let _ = resolve(cfg, server, rel)?;
    let root = server_root(cfg, server);
    let rootfd = open_root_dir(&root)?;
    let startfd = open_relative(&rootfd, rel, false, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
    let file = fs::File::create(out_abs)?;
    let out_id = file_identity(&file);
    let mut zip = zip::ZipWriter::new(file);
    let options =
        zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    let mut total: u64 = 0;
    add_dir_to_zip(&mut zip, &startfd, "", &out_id, &options, &mut total)?;
    zip.finish()?;
    Ok(total)
}

/// Walk a pinned directory fd into the zip archive. Every entry is opened
/// with `openat` + `O_NOFOLLOW` (mirroring `copy_dir_entries`), so a swapped
/// directory or a symlinked entry can never make the panel read outside the
/// sandbox; symlinks are skipped, never dereferenced.
fn add_dir_to_zip<S: AsRawFd>(
    zip: &mut zip::ZipWriter<fs::File>,
    dirfd: &S,
    base_rel: &str,
    out_id: &(u64, u64),
    options: &zip::write::FileOptions,
    total: &mut u64,
) -> Result<()> {
    // fdopendir takes ownership of the fd, so hand it a duplicate.
    let dup = unsafe { libc::dup(dirfd.as_raw_fd()) };
    if dup < 0 {
        return Err(std::io::Error::last_os_error()).context("cannot duplicate directory fd");
    }
    let dirp = unsafe { libc::fdopendir(dup) };
    if dirp.is_null() {
        let err = std::io::Error::last_os_error();
        unsafe { libc::close(dup) };
        return Err(err).context("cannot open directory for archive");
    }
    let result = loop {
        unsafe { *libc::__errno_location() = 0 };
        let ent = unsafe { libc::readdir(dirp) };
        if ent.is_null() {
            if unsafe { *libc::__errno_location() } != 0 {
                break Err(std::io::Error::last_os_error()).context("cannot read directory");
            }
            break Ok(());
        }
        let name = unsafe { std::ffi::CStr::from_ptr((*ent).d_name.as_ptr()) }.to_bytes();
        if name == b"." || name == b".." {
            continue;
        }
        let name = std::ffi::OsStr::from_bytes(name);
        let cname = CString::new(name.as_bytes()).map_err(|_| anyhow!("invalid entry name"))?;
        let raw = unsafe {
            libc::openat(
                dirfd.as_raw_fd(),
                cname.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0,
            )
        };
        if raw < 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::ELOOP) {
                continue; // symlink: external content is never archived
            }
            break Err(err).with_context(|| format!("cannot open {}", name.to_string_lossy()));
        }
        let f = fs::File::from(unsafe { OwnedFd::from_raw_fd(raw) });
        let meta = f.metadata()?;
        // never let the archive consume its own output file
        if file_identity(&f) == *out_id {
            continue;
        }
        let rel = if base_rel.is_empty() {
            name.to_string_lossy().into_owned()
        } else {
            format!("{base_rel}/{}", name.to_string_lossy())
        };
        if meta.is_dir() {
            if let Err(e) = zip.add_directory(format!("{rel}/"), *options) {
                break Err(e).context("cannot write zip directory entry");
            }
            if let Err(e) = add_dir_to_zip(zip, &f, &rel, out_id, options, total) {
                break Err(e);
            }
        } else if meta.is_file() {
            if let Err(e) = zip.start_file(rel, *options) {
                break Err(e).context("cannot write zip file entry");
            }
            *total = total.saturating_add(meta.len());
            if let Err(e) = std::io::copy(&mut &f, zip) {
                break Err(e).context("cannot write zip entry data");
            }
        }
    };
    unsafe { libc::closedir(dirp) };
    result
}

pub fn unzip_into(cfg: &Config, server: &Server, archive_rel: &str, dest_rel: &str) -> Result<()> {
    unzip_into_bounded(
        cfg,
        server,
        archive_rel,
        dest_rel,
        MAX_ARCHIVE_ENTRIES,
        MAX_EXTRACT_FILE_BYTES,
        MAX_EXTRACT_TOTAL_BYTES,
    )
}

fn unzip_into_bounded(
    cfg: &Config,
    server: &Server,
    archive_rel: &str,
    dest_rel: &str,
    max_entries: usize,
    max_file: u64,
    max_total: u64,
) -> Result<()> {
    let archive = resolve(cfg, server, archive_rel)?;
    let dest = resolve(cfg, server, dest_rel)?;
    let root = server_root(cfg, server);
    let rootfd = open_root_dir(&root)?;
    // Pin the extraction root with an O_NOFOLLOW walk that creates missing
    // parents via mkdirat. The old `create_dir_all` + path-based open could
    // be raced by swapping a parent for a symlink after `resolve`; here every
    // component is re-validated with `openat` O_NOFOLLOW at use time and the
    // final file is created with O_NOFOLLOW, closing the symlink-parent race.
    let destfd = open_dir_create(&rootfd, dest_rel)?;
    let f = fs::File::open(&archive)?;
    let mut zip = zip::ZipArchive::new(f)?;
    if zip.len() > max_entries {
        bail!("archive has too many entries (max {max_entries})");
    }
    let mut total: u64 = 0;
    for i in 0..zip.len() {
        let entry = zip.by_index(i)?;
        let out_path = safe_join(&dest, entry.name())?;
        let rel = out_path
            .strip_prefix(&dest)
            .expect("safe_join output starts with dest")
            .to_string_lossy()
            .replace('\\', "/");
        if entry.is_dir() {
            // ensure the directory exists (created via mkdirat if missing)
            let _ = open_dir_create(&destfd, &rel)?;
            continue;
        }
        // fail on declared size before writing anything
        let declared = entry.size();
        if declared > max_file || total.saturating_add(declared) > max_total {
            bail!("archive entry exceeds extraction size limits");
        }
        // walk to the parent with O_NOFOLLOW (creating missing dirs), then
        // create the final file without ever following a symlink
        let parent = parent_rel(&rel);
        let fname = Path::new(&rel)
            .file_name()
            .context("archive entry has no file name")?;
        let pfd = open_dir_create(&destfd, &parent)?;
        let cfname = CString::new(fname.as_bytes()).map_err(|_| anyhow!("invalid entry name"))?;
        let out_raw = unsafe {
            libc::openat(
                pfd.as_raw_fd(),
                cfname.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o644,
            )
        };
        if out_raw < 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("cannot create {}", fname.to_string_lossy()));
        }
        let mut f = fs::File::from(unsafe { OwnedFd::from_raw_fd(out_raw) });
        // hard cap actual bytes regardless of what the header claimed
        let cap = max_file
            .saturating_add(1)
            .min(max_total.saturating_sub(total).saturating_add(1));
        let copied = std::io::copy(&mut entry.take(cap), &mut f)?;
        if copied > max_file || total.saturating_add(copied) > max_total {
            bail!("archive entry exceeds extraction size limits");
        }
        total += copied;
    }
    Ok(())
}

pub fn tar_gz_dir(cfg: &Config, server: &Server, rel: &str, out_abs: &Path) -> Result<u64> {
    tar_gz_dir_excluding(cfg, server, rel, out_abs, &IgnoreList::default())
}

/// Glob patterns excluded from an archive, matched against the archive-relative
/// path. A directory that matches is pruned whole rather than walked.
#[derive(Debug, Default, Clone)]
pub struct IgnoreList {
    patterns: Vec<glob::Pattern>,
}

impl IgnoreList {
    /// Parse newline-separated patterns. Blank lines and `#` comments are
    /// skipped; an unparseable pattern is an error so a typo cannot silently
    /// archive data the operator asked to exclude.
    pub fn parse(spec: &str) -> Result<Self> {
        let mut patterns = Vec::new();
        for raw in spec.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let line = line.strip_prefix("./").unwrap_or(line);
            let line = line.strip_suffix('/').unwrap_or(line);
            patterns.push(
                glob::Pattern::new(line)
                    .with_context(|| format!("invalid ignore pattern: {line}"))?,
            );
        }
        Ok(Self { patterns })
    }

    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    /// True when `rel` (archive-relative, `/`-separated) is excluded. A pattern
    /// without a separator matches at any depth, mirroring gitignore's basename
    /// rule; anchored patterns match the full relative path only.
    fn excludes(&self, rel: &str) -> bool {
        self.patterns.iter().any(|p| {
            if p.matches(rel) {
                return true;
            }
            !p.as_str().contains('/')
                && rel.rsplit('/').next().is_some_and(|base| p.matches(base))
        })
    }
}

/// Archive `rel` into `out_abs`, skipping paths matched by `ignore`.
pub fn tar_gz_dir_excluding(
    cfg: &Config,
    server: &Server,
    rel: &str,
    out_abs: &Path,
    ignore: &IgnoreList,
) -> Result<u64> {
    let _ = resolve(cfg, server, rel)?;
    let root = server_root(cfg, server);
    let rootfd = open_root_dir(&root)?;
    let startfd = open_relative(&rootfd, rel, false, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
    let file = fs::File::create(out_abs)?;
    let out_id = file_identity(&file);
    let enc = flate2::write::GzEncoder::new(file, flate2::Compression::default());
    let mut tar = tar::Builder::new(enc);
    add_dir_to_tar(&mut tar, &startfd, "", &out_id, ignore)?;
    let enc = tar.into_inner()?;
    let file = enc.finish()?;
    let size = file.metadata()?.len();
    Ok(size)
}

/// Walk a pinned directory fd into the tar archive. Every entry is opened
/// with `openat` + `O_NOFOLLOW` (mirroring `copy_dir_entries`), so a swapped
/// directory or a symlinked entry can never make the panel read outside the
/// sandbox; symlinks are skipped, never dereferenced.
fn add_dir_to_tar<W: Write, S: AsRawFd>(
    tar: &mut tar::Builder<W>,
    dirfd: &S,
    base_rel: &str,
    out_id: &(u64, u64),
    ignore: &IgnoreList,
) -> Result<()> {
    // fdopendir takes ownership of the fd, so hand it a duplicate.
    let dup = unsafe { libc::dup(dirfd.as_raw_fd()) };
    if dup < 0 {
        return Err(std::io::Error::last_os_error()).context("cannot duplicate directory fd");
    }
    let dirp = unsafe { libc::fdopendir(dup) };
    if dirp.is_null() {
        let err = std::io::Error::last_os_error();
        unsafe { libc::close(dup) };
        return Err(err).context("cannot open directory for archive");
    }
    let result = loop {
        unsafe { *libc::__errno_location() = 0 };
        let ent = unsafe { libc::readdir(dirp) };
        if ent.is_null() {
            if unsafe { *libc::__errno_location() } != 0 {
                break Err(std::io::Error::last_os_error()).context("cannot read directory");
            }
            break Ok(());
        }
        let name = unsafe { std::ffi::CStr::from_ptr((*ent).d_name.as_ptr()) }.to_bytes();
        if name == b"." || name == b".." {
            continue;
        }
        let name = std::ffi::OsStr::from_bytes(name);
        let cname = CString::new(name.as_bytes()).map_err(|_| anyhow!("invalid entry name"))?;
        let raw = unsafe {
            libc::openat(
                dirfd.as_raw_fd(),
                cname.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0,
            )
        };
        if raw < 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::ELOOP) {
                continue; // symlink: external content is never archived
            }
            break Err(err).with_context(|| format!("cannot open {}", name.to_string_lossy()));
        }
        let f = fs::File::from(unsafe { OwnedFd::from_raw_fd(raw) });
        let meta = f.metadata()?;
        // never let the archive consume its own output file
        if file_identity(&f) == *out_id {
            continue;
        }
        let rel = if base_rel.is_empty() {
            name.to_string_lossy().into_owned()
        } else {
            format!("{base_rel}/{}", name.to_string_lossy())
        };
        if ignore.excludes(&rel) {
            continue;
        }
        if meta.is_dir() {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Directory);
            header.set_size(0);
            header.set_mode(meta.permissions().mode() & 0o777);
            if let Ok(mtime) = meta.modified() {
                if let Ok(d) = mtime.duration_since(std::time::UNIX_EPOCH) {
                    header.set_mtime(d.as_secs());
                }
            }
            header.set_path(&rel)?;
            header.set_cksum();
            if let Err(e) = tar.append(&header, std::io::empty()) {
                break Err(e).context("cannot write tar directory entry");
            }
            if let Err(e) = add_dir_to_tar(tar, &f, &rel, out_id, ignore) {
                break Err(e);
            }
        } else if meta.is_file() {
            let mut header = tar::Header::new_gnu();
            header.set_size(meta.len());
            // masked: setuid/setgid/sticky are never propagated through archives
            header.set_mode(meta.permissions().mode() & 0o777);
            if let Ok(mtime) = meta.modified() {
                if let Ok(d) = mtime.duration_since(std::time::UNIX_EPOCH) {
                    header.set_mtime(d.as_secs());
                }
            }
            header.set_path(&rel)?;
            let mut data = &f;
            if let Err(e) = tar.append_data(&mut header, &rel, &mut data) {
                break Err(e).context("cannot write tar file entry");
            }
        }
    };
    unsafe { libc::closedir(dirp) };
    result
}


pub fn extract_tar_gz_into(
    cfg: &Config,
    server: &Server,
    archive_rel: &str,
    dest_rel: &str,
) -> Result<()> {
    extract_tar_gz_bounded(
        cfg,
        server,
        archive_rel,
        dest_rel,
        MAX_ARCHIVE_ENTRIES,
        MAX_EXTRACT_FILE_BYTES,
        MAX_EXTRACT_TOTAL_BYTES,
    )
}

fn extract_tar_gz_bounded(
    cfg: &Config,
    server: &Server,
    archive_rel: &str,
    dest_rel: &str,
    max_entries: usize,
    max_file: u64,
    max_total: u64,
) -> Result<()> {
    let archive = resolve(cfg, server, archive_rel)?;
    let dest = resolve(cfg, server, dest_rel)?;
    let root = server_root(cfg, server);
    let rootfd = open_root_dir(&root)?;
    // Pin the extraction root with an O_NOFOLLOW walk that creates missing
    // parents via mkdirat. The old `create_dir_all` + path-based open could
    // be raced by swapping a parent for a symlink after `resolve`; here every
    // component is re-validated with `openat` O_NOFOLLOW at use time and the
    // final file is created with O_NOFOLLOW, closing the symlink-parent race.
    let destfd = open_dir_create(&rootfd, dest_rel)?;
    let file = fs::File::open(&archive)?;
    let dec = flate2::read::GzDecoder::new(file);
    let mut tar = tar::Archive::new(dec);
    let mut total: u64 = 0;
    let mut count: usize = 0;
    for entry in tar.entries()? {
        count += 1;
        if count > max_entries {
            bail!("archive has too many entries (max {max_entries})");
        }
        let entry = entry?;
        let kind = entry.header().entry_type();
        if kind.is_symlink() || kind.is_hard_link() {
            bail!("archive link entries are forbidden");
        }
        let rel = entry.path()?.to_string_lossy().to_string();
        let path = safe_join(&dest, &rel)?;
        let rel = path
            .strip_prefix(&dest)
            .expect("safe_join output starts with dest")
            .to_string_lossy()
            .to_string();
        if kind.is_dir() {
            // ensure the directory exists (created via mkdirat if missing)
            let _ = open_dir_create(&destfd, &rel)?;
            continue;
        }
        // fail on declared size before writing anything
        let declared = entry.size();
        if declared > max_file || total.saturating_add(declared) > max_total {
            bail!("archive entry exceeds extraction size limits");
        }
        // walk to the parent with O_NOFOLLOW (creating missing dirs), then
        // create the final file without ever following a symlink
        let parent = parent_rel(&rel);
        let fname = Path::new(&rel)
            .file_name()
            .context("archive entry has no file name")?;
        let pfd = open_dir_create(&destfd, &parent)?;
        let cfname = CString::new(fname.as_bytes()).map_err(|_| anyhow!("invalid entry name"))?;
        let out_raw = unsafe {
            libc::openat(
                pfd.as_raw_fd(),
                cfname.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o644,
            )
        };
        if out_raw < 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("cannot create {}", fname.to_string_lossy()));
        }
        let mut f = fs::File::from(unsafe { OwnedFd::from_raw_fd(out_raw) });
        // hard cap actual bytes regardless of what the header claimed
        let cap = max_file
            .saturating_add(1)
            .min(max_total.saturating_sub(total).saturating_add(1));
        let mode = entry.header().mode().ok();
        let copied = std::io::copy(&mut entry.take(cap), &mut f)?;
        if copied > max_file || total.saturating_add(copied) > max_total {
            bail!("archive entry exceeds extraction size limits");
        }
        total += copied;
        if let Some(mode) = mode {
            // masked: never honor setuid/setgid/sticky from an untrusted
            // archive header (privilege-escalation surface)
            let _ = f.set_permissions(fs::Permissions::from_mode(mode & 0o777));
        }
    }
    Ok(())
}

/// Open a sandboxed file for streaming download: O_NOFOLLOW walk from the
/// root, returning the suggested filename, the open file, and its size. The
/// caller streams the file (Content-Length from the size) so multi-GB files
/// never materialize in RAM.
pub fn download_file(cfg: &Config, server: &Server, rel: &str) -> Result<(String, fs::File, u64)> {
    let root = server_root(cfg, server);
    let rootfd = open_root_dir(&root)?;
    let fd = open_relative(&rootfd, rel, false, libc::O_RDONLY, 0)?;
    let file = fs::File::from(fd);
    let meta = file.metadata()?;
    if meta.is_dir() {
        bail!("cannot download a directory as a file");
    }
    let name = Path::new(rel)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    Ok((name, file, meta.len()))
}

pub fn base64_upload(cfg: &Config, server: &Server, rel: &str, b64: &str) -> Result<()> {
    let bytes = STANDARD.decode(b64)?;
    write_file(cfg, server, rel, &bytes)
}

// ---------------- Remote URL pull ----------------

/// Timeouts for remote pulls. The total bound is generous on purpose: the
/// size ceiling (web.max_body_mb) is the real limiter, and this only guards
/// against a stalled or never-ending response. The per-chunk idle timeout
/// keeps a connection that goes silent mid-transfer from pinning the task
/// forever.
const PULL_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const PULL_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const PULL_TOTAL_TIMEOUT: Duration = Duration::from_secs(300);
const PULL_MAX_REDIRECTS: usize = 5;
/// Finished transfers are pruned from the registry after this long, so the
/// map cannot grow without bound while a polling client still sees the final
/// status.
const PULL_TRANSFER_TTL: Duration = Duration::from_secs(3600);

/// An SSRF-guarded remote source: the validated URL plus every address the
/// host resolved to. The connector used for the transfer is pinned to exactly
/// these addresses, so a resolver that changed its answer between validation
/// and connection (classic DNS rebinding) cannot redirect the transfer
/// anywhere the guard did not approve.
#[derive(Clone)]
pub struct PullTarget {
    pub url: url::Url,
    pub addrs: Vec<SocketAddr>,
}

/// True when `ip` must never be a pull destination: loopback, link-local,
/// private (including IPv6 ULA), multicast, and reserved ranges (CGNAT,
/// benchmark, documentation, 0/8, 240/4, unspecified, broadcast). IPv4-mapped
/// IPv6 addresses are unwrapped and judged as IPv4, so `::ffff:127.0.0.1`
/// cannot dodge the IPv4 rules.
pub fn ip_is_blocked(ip: std::net::IpAddr) -> bool {
    use std::net::IpAddr;
    let v4 = match ip {
        IpAddr::V4(v4) => v4,
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return ip_is_blocked(IpAddr::V4(v4));
            }
            let seg = v6.segments();
            // ::1 loopback, :: unspecified, ff00::/8 multicast, fc00::/7 ULA,
            // fe80::/10 link-local, 2001:db8::/32 documentation.
            return v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || seg[0] & 0xfe00 == 0xfc00
                || seg[0] & 0xffc0 == 0xfe80
                || (seg[0] == 0x2001 && seg[1] == 0x0db8);
        }
    };
    let [a, b, c, _] = v4.octets();
    if v4.is_loopback()
        || v4.is_private()
        || v4.is_link_local()
        || v4.is_multicast()
        || v4.is_broadcast()
        || v4.is_unspecified()
        || v4.is_documentation()
    {
        return true;
    }
    // Ranges std does not classify as blocked but the guard must:
    // 0.0.0.0/8, 100.64.0.0/10 (CGNAT), 192.0.0.0/24 (protocol assignments),
    // 198.18.0.0/15 (benchmarking), 240.0.0.0/4 (reserved).
    match a {
        0 | 240..=255 => true,
        100 if b & 0xc0 == 0x40 => true,
        192 if b == 0 && c == 0 => true,
        198 if b & 0xfe == 0x12 => true,
        _ => false,
    }
}

/// Parse a pull URL and reject anything that is not http(s) with a host; a
/// literal IP host is checked immediately, a hostname is checked after
/// resolution in [`check_resolved_addrs`].
pub fn parse_pull_url(raw: &str) -> Result<url::Url> {
    let url = url::Url::parse(raw).map_err(|e| anyhow!("invalid pull URL: {e}"))?;
    match url.scheme() {
        "http" | "https" => {}
        other => bail!("pull URL scheme '{other}' is not allowed (http/https only)"),
    }
    let host = url
        .host_str()
        .ok_or_else(|| anyhow!("pull URL has no host"))?;
    // Literal IP hosts are checked immediately (`host_str()` keeps the IPv6
    // brackets, so the typed `host()` is used instead of a string parse); a
    // hostname is checked after resolution in `check_resolved_addrs`.
    if let Some(ip) = url.host().and_then(|h| match h {
        url::Host::Ipv4(ip) => Some(std::net::IpAddr::V4(ip)),
        url::Host::Ipv6(ip) => Some(std::net::IpAddr::V6(ip)),
        url::Host::Domain(_) => None,
    }) {
        if ip_is_blocked(ip) {
            bail!("pull URL host {host} is a blocked address");
        }
    }
    Ok(url)
}

/// Reject a resolution when ANY resolved address is blocked: an attacker who
/// can answer DNS for the host must not be able to sneak one private address
/// past the guard by interleaving it with public ones.
pub fn check_resolved_addrs(u: &url::Url, addrs: &[SocketAddr]) -> Result<()> {
    for a in addrs {
        if ip_is_blocked(a.ip()) {
            bail!(
                "pull URL host '{}' resolves to a blocked address {}",
                u.host_str().unwrap_or("?"),
                a.ip()
            );
        }
    }
    Ok(())
}

/// Validate `raw` through the SSRF guard and resolve the host now, so the
/// caller can pin the connection to the validated addresses.
pub async fn prepare_pull(raw: &str) -> Result<PullTarget> {
    let url = parse_pull_url(raw)?;
    let port = url.port_or_known_default().unwrap_or(80);
    let host = url.host_str().context("pull URL has no host")?.to_string();
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), port))
        .await
        .with_context(|| format!("pull URL host '{host}' could not be resolved"))?
        .collect();
    if addrs.is_empty() {
        bail!("pull URL host '{host}' resolved to no addresses");
    }
    check_resolved_addrs(&url, &addrs)?;
    Ok(PullTarget { url, addrs })
}

/// Last path segment of a pull URL, percent-decoded, for the default
/// destination filename. Falls back to `download` when there is none.
pub fn url_basename(raw: &str) -> String {
    url::Url::parse(raw)
        .ok()
        .and_then(|u| u.path_segments().and_then(|mut s| s.next_back()).map(str::to_string))
        .map(|s| percent_encoding::percent_decode_str(&s).decode_utf8_lossy().into_owned())
        .filter(|s| !s.is_empty() && s != "." && s != "..")
        .unwrap_or_else(|| "download".to_string())
}

/// Resolver that answers hyper's DNS queries with the pre-validated address
/// set only. The request URL keeps the original hostname, so the Host header
/// and TLS SNI/certificate validation stay correct, but the socket can only
/// ever connect to an address the SSRF guard approved.
#[derive(Clone)]
struct PinnedResolver {
    addrs: Arc<Vec<SocketAddr>>,
}

impl Service<Name> for PinnedResolver {
    type Response = std::vec::IntoIter<SocketAddr>;
    type Error = std::io::Error;
    type Future = std::future::Ready<std::io::Result<Self::Response>>;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, _name: Name) -> Self::Future {
        std::future::ready(Ok((*self.addrs).clone().into_iter()))
    }
}

fn pinned_client(
    addrs: &[SocketAddr],
) -> Result<Client<HttpsConnector<HttpConnector<PinnedResolver>>, Full<Bytes>>> {
    let mut http = HttpConnector::new_with_resolver(PinnedResolver {
        addrs: Arc::new(addrs.to_vec()),
    });
    http.enforce_http(false);
    http.set_connect_timeout(Some(PULL_CONNECT_TIMEOUT));
    let https = HttpsConnectorBuilder::new()
        .with_webpki_roots()
        .https_or_http()
        .enable_http1()
        .enable_http2()
        .wrap_connector(http);
    Ok(Client::builder(TokioExecutor::new()).build(https))
}

fn check_cancel(st: &Mutex<PullState>) -> Result<()> {
    if st.lock().cancel.load(Ordering::Relaxed) {
        bail!("transfer cancelled");
    }
    Ok(())
}

/// Stream a validated pull target through the chunk callback, enforcing the
/// size ceiling, connect/idle/total timeouts, cooperative cancellation, and
/// a full SSRF re-validation on every redirect hop. Returns total bytes.
async fn pull_stream<F>(target: &PullTarget, cap: u64, st: &Mutex<PullState>, mut on_chunk: F) -> Result<u64>
where
    F: FnMut(&[u8]) -> std::io::Result<()>,
{
    let mut current = target.clone();
    let mut received: u64 = 0;
    for _ in 0..=PULL_MAX_REDIRECTS {
        check_cancel(st)?;
        let client = pinned_client(&current.addrs)?;
        let uri: Uri = current.url.as_str().parse()?;
        let response = tokio::time::timeout(PULL_TOTAL_TIMEOUT, client.get(uri))
            .await
            .map_err(|_| anyhow!("pull timed out before a response was received"))??;
        let status = response.status();
        if status.is_redirection() {
            let location = response
                .headers()
                .get(hyper::header::LOCATION)
                .ok_or_else(|| anyhow!("pull redirect (HTTP {status}) without a Location header"))?
                .to_str()?;
            let next = current
                .url
                .join(location)
                .map_err(|e| anyhow!("pull redirect to an invalid URL: {e}"))?;
            // Re-run the entire guard on the redirect target: a redirect into
            // a private range is just as dangerous as a direct one.
            current = prepare_pull(next.as_str()).await?;
            continue;
        }
        if !status.is_success() {
            bail!("pull failed: HTTP {status}");
        }
        let length = response
            .headers()
            .get(hyper::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok());
        if let Some(length) = length {
            if length > cap {
                bail!("remote file exceeds the {} MiB pull limit", cap / (1024 * 1024));
            }
            st.lock().total = Some(length);
        }
        let mut body = response.into_body();
        while let Some(frame) = tokio::time::timeout(PULL_IDLE_TIMEOUT, body.frame())
            .await
            .map_err(|_| anyhow!("pull stalled: no data received for 30s"))?
        {
            let frame = frame.map_err(anyhow::Error::from)?;
            let chunk = frame
                .into_data()
                .map_err(|_| anyhow!("pull response ended with trailers instead of data"))?;
            check_cancel(st)?;
            received = received.saturating_add(chunk.len() as u64);
            if received > cap {
                bail!("remote file exceeds the {} MiB pull limit", cap / (1024 * 1024));
            }
            on_chunk(&chunk)?;
            st.lock().received = received;
        }
        return Ok(received);
    }
    bail!("pull exceeded {PULL_MAX_REDIRECTS} redirects");
}

/// Open a directory path (deepest existing ancestor canonicalized) and pin
/// it as a descriptor for fd-relative work.
fn open_dir_pinned(path: &Path) -> Result<OwnedFd> {
    let canon = canonical_maybe(path);
    let c =
        CString::new(canon.as_os_str().as_bytes()).map_err(|_| anyhow!("invalid directory path"))?;
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("cannot open {}", canon.display()));
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Stream a validated pull into `name` below a pinned directory descriptor.
/// Bytes land in a unique temp sibling (same filesystem), created with
/// `O_EXCL | O_NOFOLLOW` via `openat`, and are renamed into place with
/// `renameat` only after the stream fully succeeds, so a failed or cancelled
/// transfer never truncates or deletes a pre-existing destination. Holding
/// the directory fd for the whole transfer pins the parent inode, so a
/// parent swapped for a symlink mid-download cannot redirect the write
/// outside the sandbox; the final `renameat` never follows a destination
/// symlink (it replaces the link entry itself). The temp file is removed on
/// every failure path.
async fn download_pull_at(
    target: &PullTarget,
    dirfd: &OwnedFd,
    name: &OsStr,
    cap: u64,
    st: &Mutex<PullState>,
) -> Result<u64> {
    let cname = CString::new(name.as_bytes()).map_err(|_| anyhow!("invalid destination name"))?;
    // Refuse a pre-existing symlink destination outright.
    let mut stbuf: libc::stat = unsafe { std::mem::zeroed() };
    let r = unsafe {
        libc::fstatat(
            dirfd.as_raw_fd(),
            cname.as_ptr(),
            &mut stbuf,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if r == 0 && (stbuf.st_mode & libc::S_IFMT) == libc::S_IFLNK {
        bail!("pull destination is a symlink");
    }
    let tmp_name = format!(
        ".{}-voltpanel-pull-{}.part",
        name.to_string_lossy(),
        uuid::Uuid::new_v4().simple()
    );
    let ctmp = CString::new(tmp_name.as_bytes()).map_err(|_| anyhow!("invalid temp name"))?;
    // O_CREAT|O_EXCL: a planted symlink at the temp path is never followed,
    // and no stale temp is ever reused.
    let tfd = unsafe {
        libc::openat(
            dirfd.as_raw_fd(),
            ctmp.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if tfd < 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("cannot create pull temp for {}", name.to_string_lossy()));
    }
    let mut tmp = fs::File::from(unsafe { OwnedFd::from_raw_fd(tfd) });
    let result = pull_stream(target, cap, st, |chunk| tmp.write_all(chunk)).await;
    match result {
        Ok(size) => {
            let commit = (|| -> Result<()> {
                tmp.sync_all()
                    .with_context(|| format!("cannot sync pull temp for {}", name.to_string_lossy()))?;
                if unsafe {
                    libc::renameat(
                        dirfd.as_raw_fd(),
                        ctmp.as_ptr(),
                        dirfd.as_raw_fd(),
                        cname.as_ptr(),
                    )
                } != 0
                {
                    return Err(std::io::Error::last_os_error()).with_context(|| {
                        format!("cannot rename pull temp to {}", name.to_string_lossy())
                    });
                }
                Ok(())
            })();
            if let Err(e) = commit {
                let _ = unsafe { libc::unlinkat(dirfd.as_raw_fd(), ctmp.as_ptr(), 0) };
                return Err(e);
            }
            Ok(size)
        }
        Err(error) => {
            let _ = unsafe { libc::unlinkat(dirfd.as_raw_fd(), ctmp.as_ptr(), 0) };
            Err(error)
        }
    }
}

/// Stream a validated pull into `dest_abs` atomically. The destination's
/// parent is pinned as a directory descriptor up front (see
/// [`download_pull_at`]) so the whole transfer — including the final
/// rename — operates on the pinned inode, never through a path that could be
/// swapped for a symlink. A failed or cancelled transfer never truncates or
/// deletes a pre-existing destination; an existing destination symlink is
/// refused outright.
#[allow(dead_code)] // absolute-path convenience wrapper exercised by the pull tests
pub async fn download_pull(
    target: &PullTarget,
    dest_abs: &Path,
    cap: u64,
    st: &Mutex<PullState>,
) -> Result<u64> {
    let parent = dest_abs
        .parent()
        .context("pull destination has no parent directory")?;
    let name = dest_abs
        .file_name()
        .context("pull destination has no file name")?;
    fs::create_dir_all(parent)?;
    let dirfd = open_dir_pinned(parent)?;
    download_pull_at(target, &dirfd, name, cap, st).await
}

/// Same stream, buffered in memory (bounded by `cap`), for pushing the bytes
/// through the node protocol.
pub async fn download_pull_buf(target: &PullTarget, cap: u64, st: &Mutex<PullState>) -> Result<Vec<u8>> {
    let mut buf: Vec<u8> = Vec::new();
    pull_stream(target, cap, st, |chunk| {
        buf.extend_from_slice(chunk);
        Ok(())
    })
    .await?;
    Ok(buf)
}

/// Mutable state of one background pull, shared between the API handlers
/// (status/cancel) and the spawned task doing the transfer.
pub struct PullState {
    pub status: String, // running | done | error | cancelled
    pub phase: String,  // resolving | downloading | pushing | ""
    pub received: u64,
    pub total: Option<u64>,
    pub error: Option<String>,
    pub cancel: AtomicBool,
}

pub struct PullHandle {
    pub id: String,
    pub server_id: i64,
    pub url: String,
    /// Workspace-relative destination (`/dir/name`).
    pub dest: String,
    pub node: String,
    pub created: String,
    pub state: Mutex<PullState>,
    pub finished: Mutex<Option<Instant>>,
}

#[derive(serde::Serialize)]
pub struct PullStatus {
    pub id: String,
    pub server_id: i64,
    pub url: String,
    pub dest: String,
    pub node: String,
    pub created: String,
    pub status: String,
    pub phase: String,
    pub received: u64,
    pub total: Option<u64>,
    pub error: Option<String>,
}

type PullRegistry = Mutex<std::collections::HashMap<String, Arc<PullHandle>>>;
static PULLS: LazyLock<PullRegistry> = LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));

pub fn get_pull(id: &str) -> Option<Arc<PullHandle>> {
    PULLS.lock().get(id).cloned()
}

/// Register a transfer and hand back the handle; the caller spawns the
/// background task. Terminal transfers older than `PULL_TRANSFER_TTL` are
/// pruned on every registration so the map stays bounded.
pub fn start_pull(server_id: i64, url: &str, dest: &str, node: &str) -> Arc<PullHandle> {
    let handle = Arc::new(PullHandle {
        id: uuid::Uuid::new_v4().simple().to_string(),
        server_id,
        url: url.to_string(),
        dest: dest.to_string(),
        node: node.to_string(),
        created: chrono::Utc::now().to_rfc3339(),
        state: Mutex::new(PullState {
            status: "running".into(),
            phase: "queued".into(),
            received: 0,
            total: None,
            error: None,
            cancel: AtomicBool::new(false),
        }),
        finished: Mutex::new(None),
    });
    let mut registry = PULLS.lock();
    registry.retain(|_, h| {
        if h.state.lock().status == "running" {
            return true;
        }
        h.finished
            .lock()
            .is_none_or(|t| t.elapsed() <= PULL_TRANSFER_TTL)
    });
    registry.insert(handle.id.clone(), handle.clone());
    handle
}

/// Record the terminal outcome of a transfer. Cancellation wins over any
/// concurrent result so a cancel that lands during the final write still
/// reports `cancelled`.
pub fn finish_pull(h: &Arc<PullHandle>, result: Result<u64>) {
    let mut st = h.state.lock();
    st.phase = String::new();
    if st.cancel.load(Ordering::Relaxed) {
        st.status = "cancelled".into();
    } else {
        match result {
            Ok(size) => {
                st.status = "done".into();
                st.received = size;
                st.total = Some(st.total.unwrap_or(size));
            }
            Err(error) => {
                st.status = "error".into();
                st.error = Some(format!("{error:#}"));
            }
        }
    }
    *h.finished.lock() = Some(Instant::now());
}

/// Ask a running transfer to stop; the download loop notices between chunks.
/// Returns false when the transfer already finished.
pub fn cancel_pull(h: &Arc<PullHandle>) -> bool {
    let st = h.state.lock();
    if st.status == "running" {
        st.cancel.store(true, Ordering::Relaxed);
        true
    } else {
        false
    }
}

pub fn pull_status(h: &PullHandle) -> PullStatus {
    let st = h.state.lock();
    PullStatus {
        id: h.id.clone(),
        server_id: h.server_id,
        url: h.url.clone(),
        dest: h.dest.clone(),
        node: h.node.clone(),
        created: h.created.clone(),
        status: st.status.clone(),
        phase: st.phase.clone(),
        received: st.received,
        total: st.total,
        error: st.error.clone(),
    }
}

/// Background pull into a local workspace. The SSRF guard runs again here,
/// immediately before connecting — the handler's check only fails fast; this
/// fresh resolution and validation is the security boundary. The destination
/// parent is walked from the server root with O_NOFOLLOW and pinned as a
/// descriptor for the whole transfer (see [`download_pull_at`]), so a parent
/// swapped for a symlink mid-download cannot redirect the write outside the
/// sandbox.
pub async fn local_pull(
    cfg: &Config,
    server: &Server,
    rel: &str,
    url: &str,
    cap: u64,
    st: &Mutex<PullState>,
) -> Result<u64> {
    st.lock().phase = "resolving".into();
    let target = prepare_pull(url).await?;
    st.lock().phase = "downloading".into();
    let root = server_root(cfg, server);
    let rootfd = open_root_dir(&root)?;
    let rel_path = Path::new(rel.trim_start_matches('/'));
    let name = rel_path
        .file_name()
        .context("pull destination has no file name")?;
    let parent_rel = parent_rel(rel);
    let parentfd = open_relative(&rootfd, &parent_rel, true, libc::O_RDONLY | libc::O_DIRECTORY, 0)
        .with_context(|| format!("cannot open destination directory for {rel}"))?;
    download_pull_at(&target, &parentfd, name, cap, st).await
}

/// Background pull that lands on a remote node: download (SSRF-guarded,
/// capped) then push through the existing node protocol write path, matching
/// how `write` and multipart upload already deliver bytes to nodes.
pub async fn remote_pull(
    node_client: &crate::services::node::NodeClient,
    node: &crate::nodes::Node,
    uuid: &str,
    rel: &str,
    url: &str,
    cap: u64,
    st: &Mutex<PullState>,
) -> Result<u64> {
    st.lock().phase = "resolving".into();
    let target = prepare_pull(url).await?;
    st.lock().phase = "downloading".into();
    let bytes = download_pull_buf(&target, cap, st).await?;
    st.lock().phase = "pushing".into();
    let request = crate::node_protocol::FileWriteRequest {
        path: rel.to_string(),
        content_b64: STANDARD.encode(&bytes),
        append: false,
    };
    node_client.write_file(node, uuid, &request).await?;
    Ok(bytes.len() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Backups, Config, Features, General, Limits, Paths, Security, Web};
    use std::io::Write;
    use std::os::unix::fs::symlink;

    fn test_server(uuid: &str) -> Server {
        Server {
            id: 1,
            uuid: uuid.into(),
            name: "test".into(),
            user_id: 1,
            blueprint_id: 0,
            description: String::new(),
            status: "running".into(),
            runtime_hint: String::new(),
            startup: String::new(),
            node: "local".into(),
            port: None,
            memory_mb: 1024,
            disk_mb: 1024,
            cpu_percent: 100,
            suspended: false,
            auto_restart: false,
            restart_count: 0,
            crash_detect_clean_exit: false,
            crash_restart_budget: 5,
            crash_restarts: 0,
            crash_window_start: String::new(),
            crash_reason: String::new(),
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    fn test_config(tmp: &Path) -> Config {
        // allow_cross_server_dir enabled on purpose: the code must ignore it
        Config {
            general: General {
                instance_name: "test".into(),
                locale: "en".into(),
                data_dir: tmp.join("data"),
                log_level: "info".into(),
                audit_retention_days: 90,
            },
            web: Web {
                listen: "127.0.0.1:8080".parse().unwrap(),
                session_ttl_hours: 24,
                max_body_mb: 64,
                tls_self_signed: false,
                tls_extra_sans: Vec::new(),
                trusted_proxies: Vec::new(),
                allowed_origins: Vec::new(),
                hostnames: Vec::new(),
            },
            paths: Paths {
                servers_dir: tmp.join("servers"),
                backups_dir: tmp.join("backups"),
                blueprints_dir: tmp.join("blueprints"),
                logs_dir: tmp.join("logs"),
                website_dir: tmp.join("websites"),
                datalab_dir: tmp.join("datalab"),
            },
            limits: Limits {
                default_memory_mb: 1024,
                default_disk_mb: 8192,
                default_cpu_percent: 100,
                max_memory_mb: 16384,
                max_servers_per_user: 16,
            },
            security: Security {
                argon2_cost: 3,
                argon2_mem_kib: 65536,
                rate_limit_per_min: 120,
                password_min_len: 8,
                allow_cross_server_dir: true,
                webhook_master_key: String::new(),
                require_signed_node_responses: false,
            },
            features: Features {
                enable_backups: true,
                enable_databases: true,
                enable_schedules: true,
                enable_api_keys: true,
                enable_2fa: true,
                enable_websites: true,
                enable_audit_log: true,
            },
            backups: Backups::default(),
        }
    }

    /// Build a single-entry ustar archive with an arbitrary (possibly
    /// malicious) entry name, bypassing the Builder API's own validation.
    fn raw_tar_archive(name: &[u8], data: &[u8]) -> Vec<u8> {
        let mut h = [0u8; 512];
        let n = name.len().min(100);
        h[..n].copy_from_slice(&name[..n]);
        // size: 11 octal digits + NUL at offset 124
        let size = format!("{:011o}", data.len());
        h[124..135].copy_from_slice(size.as_bytes());
        h[156] = b'0'; // regular file typeflag
                       // POSIX checksum: sum of header bytes with the checksum field as spaces
        let sum: u32 = h.iter().map(|&b| b as u32).sum::<u32>() + 8 * 0x20;
        let ck = format!("{:06o}", sum);
        h[148..154].copy_from_slice(ck.as_bytes());
        h[154] = 0;
        h[155] = b' ';
        let mut out = h.to_vec();
        out.extend_from_slice(data);
        let pad = (512 - data.len() % 512) % 512;
        out.extend(std::iter::repeat_n(0u8, pad));
        out.extend_from_slice(&[0u8; 1024]); // two end-of-archive blocks
        out
    }

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

    #[test]
    fn resolve_always_contains_even_when_flag_enabled() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_config(tmp.path());
        let srv = test_server("s1");
        let root = server_root(&cfg, &srv);
        fs::create_dir_all(&root).unwrap();
        let secret = tmp.path().join("secret.txt");
        fs::write(&secret, "secret").unwrap();
        symlink(&secret, root.join("link")).unwrap();
        // symlink escaping the root is rejected
        assert!(resolve(&cfg, &srv, "link").is_err());
        // traversal is rejected even though the flag is on
        assert!(resolve(&cfg, &srv, "../secret.txt").is_err());
        assert!(resolve(&cfg, &srv, "a/../../x").is_err());
        // normal path resolves
        assert_eq!(
            resolve(&cfg, &srv, "sub/file.txt").unwrap(),
            root.join("sub/file.txt")
        );
    }

    #[test]
    fn read_file_never_follows_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_config(tmp.path());
        let srv = test_server("s1");
        let root = server_root(&cfg, &srv);
        fs::create_dir_all(&root).unwrap();
        let secret = tmp.path().join("secret.txt");
        fs::write(&secret, "secret").unwrap();
        symlink(&secret, root.join("link")).unwrap();
        fs::write(root.join("ok.txt"), "hi").unwrap();
        assert!(read_file(&cfg, &srv, "link", 1024).is_err());
        let (data, _) = read_file(&cfg, &srv, "ok.txt", 1024).unwrap();
        assert_eq!(data, b"hi");
    }

    #[test]
    fn write_rejects_symlinked_parent_descriptor_safe() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_config(tmp.path());
        let srv = test_server("s1");
        let root = server_root(&cfg, &srv);
        fs::create_dir_all(&root).unwrap();
        let external = tmp.path().join("external");
        fs::create_dir_all(&external).unwrap();
        symlink(&external, root.join("a")).unwrap();
        assert!(write_file(&cfg, &srv, "a/x.txt", b"pwn").is_err());
        assert!(!external.join("x.txt").exists());
        // nested parents are created normally (fresh, real dirs)
        write_file(&cfg, &srv, "n/m/c.txt", b"ok").unwrap();
        assert_eq!(fs::read_to_string(root.join("n/m/c.txt")).unwrap(), "ok");
    }

    #[test]
    fn copy_never_dereferences_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_config(tmp.path());
        let srv = test_server("s1");
        let root = server_root(&cfg, &srv);
        fs::create_dir_all(root.join("d")).unwrap();
        fs::write(root.join("d/real.txt"), "real").unwrap();
        let secret = tmp.path().join("secret.txt");
        fs::write(&secret, "secret").unwrap();
        symlink(&secret, root.join("d/link")).unwrap();
        copy(&cfg, &srv, "d", "d2").unwrap();
        // copied symlink is still a link, not dereferenced content
        let meta = fs::symlink_metadata(root.join("d2/link")).unwrap();
        assert!(meta.file_type().is_symlink());
        assert_eq!(
            fs::read_to_string(root.join("d2/real.txt")).unwrap(),
            "real"
        );
        // single-file copy of an in-root symlink preserves it as a link
        symlink(root.join("d/real.txt"), root.join("inlink")).unwrap();
        copy(&cfg, &srv, "inlink", "copied_link").unwrap();
        assert!(fs::symlink_metadata(root.join("copied_link"))
            .unwrap()
            .file_type()
            .is_symlink());
        // a single-file symlink whose target escapes the root is rejected
        assert!(copy(&cfg, &srv, "d/link", "esc_copy").is_err());
    }

    #[test]
    fn copy_dir_into_itself_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_config(tmp.path());
        let srv = test_server("s1");
        let root = server_root(&cfg, &srv);
        fs::create_dir_all(root.join("d")).unwrap();
        fs::write(root.join("d/x.txt"), "x").unwrap();
        assert!(copy(&cfg, &srv, "d", "d/inner").is_err());
    }

    #[test]
    fn delete_removes_trees_and_never_follows_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_config(tmp.path());
        let srv = test_server("s1");
        let root = server_root(&cfg, &srv);
        fs::create_dir_all(root.join("d/sub")).unwrap();
        fs::write(root.join("d/sub/x.txt"), "x").unwrap();
        fs::write(root.join("d/top.txt"), "t").unwrap();
        let secret = tmp.path().join("secret.txt");
        fs::write(&secret, "secret").unwrap();
        // a symlink whose target escapes the root, buried inside the tree:
        // the recursive delete must unlink it, never dereference it
        symlink(&secret, root.join("d/link")).unwrap();
        delete(&cfg, &srv, "d").unwrap();
        assert!(!root.join("d").exists());
        assert_eq!(fs::read_to_string(&secret).unwrap(), "secret");
        // single file
        fs::write(root.join("f.txt"), "f").unwrap();
        delete(&cfg, &srv, "f.txt").unwrap();
        assert!(!root.join("f.txt").exists());
        // a top-level symlink is removed as a link, target untouched
        fs::write(root.join("target.txt"), "target").unwrap();
        symlink(root.join("target.txt"), root.join("lnk")).unwrap();
        delete(&cfg, &srv, "lnk").unwrap();
        assert!(fs::symlink_metadata(root.join("lnk")).is_err());
        assert_eq!(fs::read_to_string(root.join("target.txt")).unwrap(), "target");
        // the sandbox root itself is never deletable
        assert!(delete(&cfg, &srv, "/").is_err());
        assert!(delete(&cfg, &srv, ".").is_err());
        // missing paths error instead of silently succeeding
        assert!(delete(&cfg, &srv, "nope").is_err());
    }

    #[test]
    fn zip_archive_skips_symlinks_and_output_file() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_config(tmp.path());
        let srv = test_server("s1");
        let root = server_root(&cfg, &srv);
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("a.txt"), "hi").unwrap();
        let secret = tmp.path().join("secret.txt");
        fs::write(&secret, "secret").unwrap();
        symlink(&secret, root.join("link.txt")).unwrap();
        let out_abs = resolve(&cfg, &srv, "out.zip").unwrap();
        zip_dir(&cfg, &srv, ".", &out_abs).unwrap();
        let f = fs::File::open(&out_abs).unwrap();
        let mut z = zip::ZipArchive::new(f).unwrap();
        let names: Vec<String> = (0..z.len())
            .map(|i| z.by_index(i).unwrap().name().to_string())
            .collect();
        assert!(names.contains(&"a.txt".to_string()));
        assert!(!names.iter().any(|n| n.contains("link") || n == "out.zip"));
        unzip_into(&cfg, &srv, "out.zip", "x").unwrap();
        assert_eq!(fs::read_to_string(root.join("x/a.txt")).unwrap(), "hi");
        assert!(!root.join("x/link.txt").exists());
    }

    #[test]
    fn unzip_rejects_traversal_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_config(tmp.path());
        let srv = test_server("s1");
        let root = server_root(&cfg, &srv);
        fs::create_dir_all(&root).unwrap();
        let file = fs::File::create(root.join("evil.zip")).unwrap();
        let mut z = zip::ZipWriter::new(file);
        let opt =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Stored);
        z.start_file("../evil.txt", opt).unwrap();
        z.write_all(b"pwn").unwrap();
        z.start_file("/abs.txt", opt).unwrap();
        z.write_all(b"pwn").unwrap();
        z.finish().unwrap();
        assert!(unzip_into(&cfg, &srv, "evil.zip", "dest").is_err());
        assert!(!tmp.path().join("evil.txt").exists());
        assert!(!root.join("abs.txt").exists());
    }

    #[test]
    fn unzip_bounds_entry_count_and_sizes() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_config(tmp.path());
        let srv = test_server("s1");
        let root = server_root(&cfg, &srv);
        fs::create_dir_all(&root).unwrap();
        let file = fs::File::create(root.join("multi.zip")).unwrap();
        let mut z = zip::ZipWriter::new(file);
        let opt =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Stored);
        for name in ["f0", "f1", "f2"] {
            z.start_file(name, opt).unwrap();
            z.write_all(name.as_bytes()).unwrap();
        }
        z.finish().unwrap();
        // entry count cap
        assert!(unzip_into_bounded(&cfg, &srv, "multi.zip", "d1", 2, 1 << 20, 1 << 20).is_err());
        // per-file size cap
        assert!(unzip_into_bounded(&cfg, &srv, "multi.zip", "d2", 10, 1, 1 << 20).is_err());
        // total size cap
        assert!(unzip_into_bounded(&cfg, &srv, "multi.zip", "d3", 10, 1 << 20, 4).is_err());
        // within limits: extracts fully
        unzip_into_bounded(&cfg, &srv, "multi.zip", "d4", 10, 1 << 20, 1 << 20).unwrap();
        assert_eq!(fs::read_to_string(root.join("d4/f2")).unwrap(), "f2");
    }

    #[test]
    fn tar_archive_skips_symlinks_and_output() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_config(tmp.path());
        let srv = test_server("s1");
        let root = server_root(&cfg, &srv);
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("b.txt"), "hi").unwrap();
        let secret = tmp.path().join("secret.txt");
        fs::write(&secret, "secret").unwrap();
        symlink(&secret, root.join("link2.txt")).unwrap();
        let out_abs = resolve(&cfg, &srv, "out.tar.gz").unwrap();
        tar_gz_dir(&cfg, &srv, ".", &out_abs).unwrap();
        extract_tar_gz_into(&cfg, &srv, "out.tar.gz", "t").unwrap();
        assert_eq!(fs::read_to_string(root.join("t/b.txt")).unwrap(), "hi");
        assert!(!root.join("t/link2.txt").exists());
        assert!(!root.join("t/out.tar.gz").exists());
    }

    #[test]
    fn tar_extract_rejects_links_and_traversal_and_bounds() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_config(tmp.path());
        let srv = test_server("s1");
        let root = server_root(&cfg, &srv);
        fs::create_dir_all(&root).unwrap();

        // symlink entry -> forbidden
        let out = root.join("links.tar.gz");
        let file = fs::File::create(&out).unwrap();
        let enc = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        let mut tar = tar::Builder::new(enc);
        let mut h = tar::Header::new_gnu();
        h.set_path("lnk").unwrap();
        tar.append_link(&mut h, "lnk", "secret").unwrap();
        let enc = tar.into_inner().unwrap();
        enc.finish().unwrap();
        assert!(extract_tar_gz_into(&cfg, &srv, "links.tar.gz", "t1").is_err());

        // ../ entry -> escape rejected (crafted as raw tar; the Builder API
        // itself refuses `..` paths, so the malicious header is built by hand)
        let out = root.join("slip.tar.gz");
        let raw = raw_tar_archive(b"../evil.txt", b"pwn");
        let file = fs::File::create(&out).unwrap();
        let mut enc = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        std::io::Write::write_all(&mut enc, &raw).unwrap();
        enc.finish().unwrap();
        assert!(extract_tar_gz_into(&cfg, &srv, "slip.tar.gz", "t2").is_err());
        assert!(!tmp.path().join("evil.txt").exists());
        assert!(!root.join("evil.txt").exists());

        // two-file archive for bound checks
        let out = root.join("two.tar.gz");
        let file = fs::File::create(&out).unwrap();
        let enc = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        let mut tar = tar::Builder::new(enc);
        for name in ["a", "b"] {
            let mut h = tar::Header::new_gnu();
            h.set_size(1);
            h.set_mode(0o644);
            h.set_path(name).unwrap();
            tar.append_data(&mut h, name, &mut std::io::Cursor::new(b"x"))
                .unwrap();
        }
        let enc = tar.into_inner().unwrap();
        enc.finish().unwrap();
        // entry count cap
        assert!(
            extract_tar_gz_bounded(&cfg, &srv, "two.tar.gz", "t3", 1, 1 << 20, 1 << 20).is_err()
        );
        // per-file size cap
        assert!(extract_tar_gz_bounded(&cfg, &srv, "two.tar.gz", "t4", 10, 0, 1 << 20).is_err());
        // total size cap
        assert!(extract_tar_gz_bounded(&cfg, &srv, "two.tar.gz", "t5", 10, 1 << 20, 1).is_err());
        // within limits
        extract_tar_gz_bounded(&cfg, &srv, "two.tar.gz", "t6", 10, 1 << 20, 1 << 20).unwrap();
        assert_eq!(fs::read_to_string(root.join("t6/b")).unwrap(), "x");
    }

    /// End-to-end smoke: pull a real HTTP source we control and watch it land
    /// inside the workspace root. The SSRF guard blocks every private and
    /// loopback range, so the source is bound to a *public* alias added on
    /// loopback for the duration of the run:
    ///
    /// ```sh
    /// ip addr add 52.0.0.1/32 dev lo
    /// python3 -m http.server 8000 --bind 52.0.0.1 --directory /tmp/pullsrc &
    /// # plus a slow server on :8001 for the cancel leg (see report)
    /// cargo test --bin voltpanel --lib services::files::tests::pull_smoke -- --ignored --nocapture
    /// ip addr del 52.0.0.1/32 dev lo
    /// ```
    #[test]
    #[ignore = "needs the 52.0.0.1 loopback alias plus HTTP servers on :8000/:8001"]
    fn pull_smoke_real_http_source() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let tmp = tempfile::tempdir().unwrap();
            let cfg = test_config(tmp.path());
            let srv = test_server("smoke-srv");
            let root = server_root(&cfg, &srv);
            fs::create_dir_all(&root).unwrap();
            let cap = 64u64 * 1024 * 1024;

            // 1. happy path: the pulled file lands inside the workspace root
            let st = Mutex::new(PullState {
                status: "running".into(),
                phase: String::new(),
                received: 0,
                total: None,
                error: None,
                cancel: AtomicBool::new(false),
            });
            let size = local_pull(
                &cfg,
                &srv,
                "sub/hello.txt",
                "http://52.0.0.1:8000/hello.txt",
                cap,
                &st,
            )
            .await
            .unwrap();
            let written = fs::read_to_string(root.join("sub/hello.txt")).unwrap();
            assert_eq!(written, "hello from the smoke server\n");
            assert!(size as usize == written.len());
            // contained: nothing of ours appeared outside the root
            assert!(!tmp.path().join("hello.txt").exists());
            assert!(!tmp.path().join("sub").exists());
            println!(
                "SMOKE 1 ok: pulled {} bytes -> {}",
                size,
                root.join("sub/hello.txt").display()
            );

            // 2. SSRF live: a loopback source is rejected even though it is
            // fully reachable on this host
            let st = Mutex::new(PullState {
                status: "running".into(),
                phase: String::new(),
                received: 0,
                total: None,
                error: None,
                cancel: AtomicBool::new(false),
            });
            let err = local_pull(&cfg, &srv, "x.txt", "http://127.0.0.1:8000/hello.txt", cap, &st)
                .await
                .unwrap_err();
            assert!(err.to_string().contains("blocked"), "got: {err}");
            assert!(!root.join("x.txt").exists());
            println!("SMOKE 2 ok: loopback pull rejected: {err:#}");

            // 3. cancel mid-transfer against the slow server on :8001
            let st = Arc::new(Mutex::new(PullState {
                status: "running".into(),
                phase: String::new(),
                received: 0,
                total: None,
                error: None,
                cancel: AtomicBool::new(false),
            }));
            let st_task = st.clone();
            let cfg_task = cfg.clone();
            let srv_task = srv.clone();
            let task = tokio::spawn(async move {
                local_pull(
                    &cfg_task,
                    &srv_task,
                    "big.bin",
                    "http://52.0.0.1:8001/big",
                    cap,
                    &st_task,
                )
                .await
            });
            tokio::time::sleep(Duration::from_millis(300)).await;
            st.lock().cancel.store(true, Ordering::Relaxed);
            let result = task.await.unwrap();
            assert!(result.is_err());
            let msg = format!("{:#}", result.unwrap_err());
            assert!(msg.contains("cancelled"), "got: {msg}");
            // the partial file was cleaned up
            assert!(!root.join("big.bin").exists());
            println!("SMOKE 3 ok: cancelled mid-transfer ({msg})");
        });
    }

    #[test]
    fn ip_is_blocked_covers_forbidden_classes() {
        let blocked = [
            // loopback
            "127.0.0.1", "127.8.8.8",
            // link-local (cloud metadata)
            "169.254.169.254",
            // private
            "10.0.0.1", "172.16.0.1", "172.31.255.255", "192.168.1.1",
            // unspecified / CGNAT
            "0.0.0.0", "100.64.0.1", "100.127.255.254",
            // IETF protocol assignments
            "192.0.0.1",
            // benchmarking
            "198.18.0.1", "198.19.255.255",
            // documentation
            "192.0.2.1", "198.51.100.1", "203.0.113.1",
            // multicast / reserved / broadcast
            "224.0.0.1", "239.255.255.255", "240.0.0.1", "255.255.255.255",
            // IPv6 loopback / unspecified / ULA / link-local / multicast / docs
            "::1", "::", "fc00::1", "fd12:3456::1", "fe80::1", "ff02::1", "2001:db8::1",
            // IPv4-mapped IPv6 must be judged as IPv4
            "::ffff:127.0.0.1", "::ffff:10.0.0.1",
        ];
        for ip in blocked {
            assert!(ip_is_blocked(ip.parse().unwrap()), "{ip} should be blocked");
        }
        for ip in ["8.8.8.8", "93.184.216.34", "1.1.1.1", "2606:4700:4700::1111"] {
            assert!(!ip_is_blocked(ip.parse().unwrap()), "{ip} should be allowed");
        }
    }

    #[test]
    fn parse_pull_url_rejects_blocked_literals_and_bad_schemes() {
        for bad in [
            "http://127.0.0.1/secret",
            "http://127.0.0.1:8080/secret",
            "http://169.254.169.254/latest/meta-data",
            "http://10.1.2.3/x",
            "http://172.16.0.1/x",
            "http://192.168.1.1/x",
            "http://[::1]/x",
            "http://[fe80::1]/x",
            "http://[::ffff:127.0.0.1]/x",
            "ftp://example.com/x",
            "file:///etc/passwd",
            "gopher://example.com/x",
            "not a url",
        ] {
            assert!(parse_pull_url(bad).is_err(), "should reject {bad}");
        }
        assert!(parse_pull_url("http://93.184.216.34/file.txt").is_ok());
        assert!(parse_pull_url("https://example.com/download.zip").is_ok());
        assert!(parse_pull_url("https://example.com:8443/x").is_ok());
    }

    #[test]
    fn ssrf_guard_rejects_hostname_resolving_to_blocked_ip() {
        let u = parse_pull_url("http://attacker.example/x").unwrap();
        for octets in [[127, 0, 0, 1], [169, 254, 169, 254], [10, 0, 0, 1], [192, 168, 1, 1]] {
            let addr = std::net::SocketAddr::from((octets, 80));
            assert!(
                check_resolved_addrs(&u, &[addr]).is_err(),
                "should reject {octets:?}"
            );
        }
        // One blocked address poisons the whole resolution set.
        let mixed = [
            std::net::SocketAddr::from(([93, 184, 216, 34], 80)),
            std::net::SocketAddr::from(([192, 168, 1, 1], 80)),
        ];
        assert!(check_resolved_addrs(&u, &mixed).is_err());
        // An all-public resolution is accepted.
        let public = [
            std::net::SocketAddr::from(([93, 184, 216, 34], 80)),
            std::net::SocketAddr::from(([93, 184, 216, 35], 80)),
        ];
        assert!(check_resolved_addrs(&u, &public).is_ok());
    }

    #[test]
    fn prepare_pull_public_literal_resolves_without_dns() {
        // A literal IP host needs no DNS, so the async guard is fully hermetic.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let target = rt
            .block_on(prepare_pull("http://93.184.216.34:8080/a/b.txt"))
            .unwrap();
        assert_eq!(target.addrs.len(), 1);
        assert_eq!(target.addrs[0], "93.184.216.34:8080".parse().unwrap());
    }

    #[test]
    fn pull_dest_containment_rejects_traversal_and_symlink_escape() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_config(tmp.path());
        let srv = test_server("s1");
        let root = server_root(&cfg, &srv);
        fs::create_dir_all(&root).unwrap();
        let secret = tmp.path().join("secret.txt");
        fs::write(&secret, "secret").unwrap();
        symlink(&secret, root.join("link")).unwrap();
        let external = tmp.path().join("external");
        fs::create_dir_all(&external).unwrap();
        symlink(&external, root.join("dirlink")).unwrap();
        // traversal in the destination
        assert!(resolve(&cfg, &srv, "../secret.txt").is_err());
        assert!(resolve(&cfg, &srv, "a/../../x").is_err());
        // symlinked file parent escapes the root
        assert!(resolve(&cfg, &srv, "link/evil.txt").is_err());
        // symlinked directory escapes the root
        assert!(resolve(&cfg, &srv, "dirlink").is_err());
        assert!(resolve(&cfg, &srv, "dirlink/pwn.txt").is_err());
        // a fresh nested destination stays contained inside the root
        let abs = resolve(&cfg, &srv, "sub/deep/file.txt").unwrap();
        assert!(abs.starts_with(&root));
    }

    #[test]
    fn pull_url_basename_uses_last_segment() {
        assert_eq!(url_basename("http://x/a/b/file.zip"), "file.zip");
        assert_eq!(url_basename("http://x/a%20b.txt"), "a b.txt");
        assert_eq!(url_basename("http://x/"), "download");
        assert_eq!(url_basename("http://x/a/../b"), "b");
        assert_eq!(url_basename("garbage"), "download");
    }

    /// Serve a single HTTP/1.1 response then close, so a pull's success path
    /// can be exercised against a fully local listener. The request head is
    /// drained first (as a real server would): hyper treats bytes arriving on
    /// a fresh connection before its request has been sent as an unexpected
    /// message, so an unsolicited early response fails the pull.
    async fn one_shot_http_server(body: &'static [u8]) -> std::net::SocketAddr {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0u8; 4096];
                loop {
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    if n == 0 || buf[..n].windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(body).await;
            }
        });
        addr
    }

    #[test]
    fn pull_download_is_atomic_and_preserves_existing_destination() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let tmp = tempfile::tempdir().unwrap();
            let dest = tmp.path().join("out.txt");
            fs::write(&dest, "precious original").unwrap();

            let state = || {
                Mutex::new(PullState {
                    status: "running".into(),
                    phase: String::new(),
                    received: 0,
                    total: None,
                    error: None,
                    cancel: AtomicBool::new(false),
                })
            };

            // 1. Failure (connection refused) leaves the pre-existing
            //    destination byte-for-byte intact and drops no temp files.
            let closed = {
                let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                l.local_addr().unwrap()
            };
            let target = PullTarget {
                url: url::Url::parse(&format!("http://{closed}/file.txt")).unwrap(),
                addrs: vec![closed],
            };
            assert!(download_pull(&target, &dest, 1024, &state()).await.is_err());
            assert_eq!(fs::read_to_string(&dest).unwrap(), "precious original");
            assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 1);

            // 2. Success replaces the destination atomically, only after the
            //    full body has landed.
            let body: &'static [u8] = b"downloaded content";
            let addr = one_shot_http_server(body).await;
            let target = PullTarget {
                url: url::Url::parse(&format!("http://{addr}/file.txt")).unwrap(),
                addrs: vec![addr],
            };
            let size = download_pull(&target, &dest, 1024, &state()).await.unwrap();
            assert_eq!(size, body.len() as u64);
            assert_eq!(fs::read(&dest).unwrap(), body);
            assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 1);

            // 3. A destination symlink is refused: never followed (write-out)
            //    and never silently replaced.
            let victim = tmp.path().join("victim.txt");
            fs::write(&victim, "outside").unwrap();
            symlink(&victim, tmp.path().join("lnk.txt")).unwrap();
            let addr2 = one_shot_http_server(body).await;
            let target = PullTarget {
                url: url::Url::parse(&format!("http://{addr2}/file.txt")).unwrap(),
                addrs: vec![addr2],
            };
            let err = download_pull(&target, &tmp.path().join("lnk.txt"), 1024, &state())
                .await
                .unwrap_err();
            assert!(err.to_string().contains("symlink"), "got: {err}");
            assert_eq!(fs::read_to_string(&victim).unwrap(), "outside");
            assert!(
                fs::symlink_metadata(tmp.path().join("lnk.txt"))
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
        });
    }

}