//! Backup service: create/restore/download backups of server directories.
use crate::config::Config;
use crate::db::{blocking, Db};
use crate::models;
use crate::services::webhooks;
use anyhow::{bail, Context, Result};
use chrono::Utc;
use sha2::{Digest, Sha256};
use serde_json::json;
use std::ffi::CString;
use std::fs;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, OnceLock};
// Run a closure on Tokio's blocking pool, passing the pool itself as the
// argument so its body can call pool-based models without capturing a `Db`
// (which would force a per-closure move). Never unwrapped: a join failure
// surfaces as a `db worker failed` error. See `crate::db::blocking`.

/// Hard limits applied while extracting a backup archive into staging,
/// matching the bounds enforced by `services::files` for archive extraction.
const MAX_ARCHIVE_ENTRIES: usize = 10_000;
const MAX_EXTRACT_FILE_BYTES: u64 = 512 * 1024 * 1024;
/// Total extracted bytes cap, shared with the remote path: a remote
/// snapshot/restore archive is materialized in RAM (base64 on the wire), so
/// the panel refuses any archive beyond this bound rather than allocate
/// unbounded multi-GB buffers. Full streaming is backlog.
pub(crate) const MAX_EXTRACT_TOTAL_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Largest base64-encoded remote archive the panel will accept (base64
/// inflates 4:3, plus padding), the wire equivalent of
/// [`MAX_EXTRACT_TOTAL_BYTES`].
pub(crate) const MAX_REMOTE_ARCHIVE_B64_CHARS: usize =
    (MAX_EXTRACT_TOTAL_BYTES as usize) * 4 / 3 + 8;

/// Per-server operation locks: serialize `create`/`restore`/`cleanup_old`/
/// `delete` (and the API-level remote branches that bypass these fns) so two
/// concurrent operations on the same server cannot interleave their
/// stop/archive/swap/remove phases and clobber each other. Entries are added
/// on first use and never removed (bounded by the servers table).
static SERVER_OP_LOCKS: LazyLock<Mutex<HashMap<i64, Arc<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Acquire the per-server operation lock for `server_id`, waiting until no
/// other create/restore/cleanup/delete is in flight for that server.
pub async fn server_op_lock(server_id: i64) -> tokio::sync::OwnedMutexGuard<()> {
    // The std mutex guard must not be held across the await (that would make
    // the future non-Send); scope it so it drops before locking the tokio mutex.
    let lock = {
        let mut map = SERVER_OP_LOCKS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        map.entry(server_id)
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    };
    lock.lock_owned().await
}

/// Enqueue a `backup.*` event for `server_id` (best-effort, fire and forget).
/// Called with the per-server operation lock still held, so an operation
/// emits at most once even when concurrent create/restore/cleanup interleave.
/// The envelope carries the server identity (id/uuid/name) and a timestamp;
/// `extra` merges event-specific fields (operation, backup identity, error).
fn emit_backup_event(db: &Db, server_id: i64, event: &str, extra: serde_json::Value) {
    let srv = models::get_server(db, server_id).ok();
    let mut payload = json!({
        "event": event,
        "server_id": server_id,
        "uuid": srv.as_ref().map(|s| s.uuid.clone()),
        "server_name": srv.as_ref().map(|s| s.name.clone()),
        "timestamp": Utc::now().to_rfc3339(),
    });
    if let (serde_json::Value::Object(extra), serde_json::Value::Object(base)) =
        (extra, &mut payload)
    {
        for (k, v) in extra {
            base.insert(k, v);
        }
    }
    webhooks::emit(db, event, Some(server_id), payload);
}
/// Create a tar.gz backup of the server dir. Returns (backup_id, size, checksum).
///
/// `ignored` is a newline-separated glob list excluded from the archive; it is
/// stored on the backup row so a restore can report what was never captured.
pub async fn create(
    db: &Db,
    cfg: &Config,
    server_id: i64,
    name: &str,
    ignored: &str,
) -> Result<(i64, u64, String)> {
    let _guard = server_op_lock(server_id).await;
    let db = db.clone();
    let db_worker = db.clone();
    let cfg = cfg.clone();
    let name = name.to_owned();
    let emit_name = name.clone();
    let ignored = ignored.to_owned();
    // Archive + checksum are heavy blocking fs work; run them on the blocking
    // pool so a large server dir never stalls tokio workers.
    let result = tokio::task::spawn_blocking(move || {
        let db = db_worker;
        let server = models::get_server(&db, server_id)?;
        let ignore = crate::services::files::IgnoreList::parse(&ignored)?;
        let uuid = uuid::Uuid::new_v4().to_string();
        let fname = format!("{uuid}.tar.gz");
        let out = cfg.paths.backups_dir.join(&fname);
        fs::create_dir_all(&cfg.paths.backups_dir)?;
        // tar.gz to match the archive format node snapshots produce, so local
        // and remote backups behave identically end to end.
        let size = crate::services::files::tar_gz_dir_excluding(&cfg, &server, ".", &out, &ignore)?;
        let checksum = checksum_file(&out)?;
        let id = models::create_backup(
            &db,
            &uuid,
            server_id,
            &name,
            &out.to_string_lossy(),
            size as i64,
            &checksum,
            "tar.gz",
            &ignored,
        )?;
        // Offsite mirror: best-effort copy + retention trim once the primary
        // archive is durable. Failures are warn-only (see mirror_archive).
        mirror_archive(&cfg, &server, &out);
        Ok((id, size, checksum))
    })
    .await
    .context("backup create worker panicked");
    let result: Result<(i64, u64, String)> = match result {
        Ok(inner) => inner,
        Err(e) => Err(anyhow::anyhow!("backup create worker panicked: {e}")),
    };
    match &result {
        Ok((id, size, checksum)) => {
            let extra = json!({
                "operation": "create",
                "backup_id": id,
                "backup_name": emit_name,
                "size": size,
                "checksum": checksum,
            });
            let _ = blocking(db.clone(), move |db| {
                emit_backup_event(&db, server_id, "backup.complete", extra);
                Ok(())
            })
            .await;
        }
        Err(e) => {
            let extra = json!({"operation": "create", "error": e.to_string()});
            let _ = blocking(db.clone(), move |db| {
                emit_backup_event(&db, server_id, "backup.failed", extra);
                Ok(())
            })
            .await;
        }
    }
    result
}

/// Restore a backup: replace server dir contents.
///
/// Refuses to extract when the on-disk archive no longer matches the checksum
/// recorded at creation (corrupted or tampered backup).
pub async fn restore(db: &Db, cfg: &Config, backup_id: i64) -> Result<()> {
    let backup = blocking(db.clone(), move |db| models::get_backup(&db, backup_id)).await?;
    let _guard = server_op_lock(backup.server_id).await;
    let db = db.clone();
    let db_worker = db.clone();
    let cfg = cfg.clone();
    // Checksum, extraction, and isolation are heavy blocking fs work; run
    // them on the blocking pool so a large restore never stalls tokio workers.
    let result = tokio::task::spawn_blocking(move || {
        let db = db_worker;
        let backup = models::get_backup(&db, backup_id)?;
        let server = models::get_server(&db, backup.server_id)?;
        let dir = cfg.paths.servers_dir.join(&server.uuid);
        let archive = PathBuf::from(&backup.path);
        if checksum_file(&archive)? != backup.checksum {
            bail!("backup checksum mismatch; refusing restore");
        }
        let parent = dir.parent().context("server dir has no parent")?;
        let staging = parent.join(format!(".restore-{}", uuid::Uuid::new_v4().simple()));
        fs::create_dir_all(&staging)?;
        let result = match backup.format.as_str() {
            "zip" => extract_zip(&archive, &staging),
            "tar.gz" => extract_tar_gz(&archive, &staging),
            other => Err(anyhow::anyhow!("unsupported backup format: {other}")),
        };
        if let Err(error) = result {
            let _ = fs::remove_dir_all(&staging);
            return Err(error);
        }
        crate::isolation::prepare_root(&staging, &server.uuid)?;
        crate::isolation::own_tree(&staging, &server.uuid)?;
        // Atomic two-phase swap. The old code renamed the live dir aside and
        // then the staging dir into place; a crash between the two renames
        // left the server directory permanently absent, and the rollback
        // rename error was swallowed. renameat2(RENAME_EXCHANGE) atomically
        // exchanges the live dir and the staging dir, so at every instant
        // exactly one of them is the server dir — no crash window can lose it.
        // The superseded content is then removed from the staging name; a
        // crash before that cleanup only leaks a `.restore-*` dir (reclaimed
        // by [`recover_stale_dirs`] at startup). Filesystems without
        // renameat2(RENAME_EXCHANGE) (NFS, FUSE, older kernels) fall back to
        // the non-atomic two-rename dance via [`exchange_dirs`].
        if dir.exists() {
            exchange_dirs(&dir, &staging)?;
            if let Err(error) = fs::remove_dir_all(&staging) {
                tracing::warn!(
                    "could not remove superseded server dir {}: {error}",
                    staging.display()
                );
            }
        } else {
            fs::rename(&staging, &dir)?;
        }
        Ok(())
    })
    .await
    .context("backup restore worker panicked");
    let result: Result<()> = match result {
        Ok(inner) => inner,
        Err(e) => Err(anyhow::anyhow!("backup restore worker panicked: {e}")),
    };
    match &result {
        Ok(()) => {
            let extra = json!({
                "operation": "restore",
                "backup_id": backup_id,
                "backup_name": backup.name,
            });
            let _ = blocking(db.clone(), move |db| {
                emit_backup_event(&db, backup.server_id, "backup.complete", extra);
                Ok(())
            })
            .await;
        }
        Err(e) => {
            let extra = json!({
                "operation": "restore",
                "backup_id": backup_id,
                "backup_name": backup.name,
                "error": e.to_string(),
            });
            let _ = blocking(db.clone(), move |db| {
                emit_backup_event(&db, backup.server_id, "backup.failed", extra);
                Ok(())
            })
            .await;
        }
    }
    result
}

/// Atomically exchange `a` and `b` (both must exist, on the same filesystem)
/// via `renameat2(RENAME_EXCHANGE)`, so a directory swap can never pass
/// through a state where the destination is absent.
fn renameat2_exchange(a: &Path, b: &Path) -> Result<()> {
    let ca = CString::new(a.as_os_str().as_bytes()).map_err(|_| anyhow::anyhow!("invalid path"))?;
    let cb = CString::new(b.as_os_str().as_bytes()).map_err(|_| anyhow::anyhow!("invalid path"))?;
    let r = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            ca.as_ptr(),
            libc::AT_FDCWD,
            cb.as_ptr(),
            libc::RENAME_EXCHANGE,
        )
    };
    if r != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("cannot exchange {} and {}", a.display(), b.display()));
    }
    Ok(())
}

/// Swap `a` and `b` so the destination is never absent mid-swap. Uses
/// `renameat2(RENAME_EXCHANGE)` where the kernel/filesystem supports it;
/// filesystems that answer EINVAL/ENOSYS (NFS, FUSE, older kernels) fall back
/// to the non-atomic two-rename dance and log once.
fn exchange_dirs(a: &Path, b: &Path) -> Result<()> {
    let parent = a.parent().unwrap_or(Path::new("."));
    if *RENAMEAT2_SUPPORTED.get_or_init(|| probe_renameat2(parent)) {
        match renameat2_exchange(a, b) {
            Ok(()) => return Ok(()),
            Err(error) if renameat2_unsupported(&error) => {
                tracing::warn!(
                    "renameat2(RENAME_EXCHANGE) unsupported on this filesystem ({error}); \
                     falling back to a non-atomic two-rename swap"
                );
            }
            Err(error) => return Err(error),
        }
    }
    exchange_dirs_fallback(a, b)
}

/// Cache for [`exchange_dirs`]: support depends on the filesystem the first
/// exchange runs on (runtime input), hence OnceLock rather than LazyLock.
static RENAMEAT2_SUPPORTED: OnceLock<bool> = OnceLock::new();

/// Probe support once by exchanging two throwaway files next to the first
/// real exchange (same filesystem, so the result is authoritative).
fn probe_renameat2(parent: &Path) -> bool {
    let t1 = parent.join(format!(".renameat2-probe-{}", uuid::Uuid::new_v4().simple()));
    let t2 = parent.join(format!(".renameat2-probe-{}", uuid::Uuid::new_v4().simple()));
    let supported = (|| {
        fs::write(&t1, b"x")?;
        fs::write(&t2, b"y")?;
        match renameat2_exchange(&t1, &t2) {
            Ok(()) => Ok(true),
            Err(error) if renameat2_unsupported(&error) => Ok(false),
            Err(error) => Err(error),
        }
    })();
    let _ = fs::remove_file(&t1);
    let _ = fs::remove_file(&t2);
    match supported {
        Ok(value) => value,
        // Unexpected probe failure (e.g. permissions): assume supported and
        // let a real exchange surface the actual error.
        Err(error) => {
            tracing::debug!("renameat2 probe failed in {}: {error}", parent.display());
            true
        }
    }
}

/// EINVAL/ENOSYS/EOPNOTSUPP from renameat2 mean "this filesystem has no
/// RENAME_EXCHANGE" — the fallback case, not a caller error.
fn renameat2_unsupported(error: &anyhow::Error) -> bool {
    matches!(
        error
            .root_cause()
            .downcast_ref::<std::io::Error>()
            .and_then(std::io::Error::raw_os_error),
        Some(libc::ENOSYS | libc::EINVAL | libc::EOPNOTSUPP)
    )
}

/// Non-atomic fallback: rename `a` aside, rename `b` into place, roll back on
/// failure. A crash between the renames can leave the destination absent (the
/// exact failure the exchange path avoids) — acceptable only on filesystems
/// without `RENAME_EXCHANGE`; the deterministic `.previous-<name>` aside name
/// lets [`recover_stale_dirs`] tell a superseded leftover (safe to reclaim)
/// from the only surviving copy of a crashed swap (must be restored).
fn exchange_dirs_fallback(a: &Path, b: &Path) -> Result<()> {
    let parent = a.parent().unwrap_or(Path::new("."));
    let name = a
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .context("cannot derive aside name for dir swap")?;
    let aside = parent.join(format!(".previous-{name}"));
    // The live dir `a` still existing proves any pre-existing `.previous-<name>`
    // is a superseded leftover from an earlier completed swap whose cleanup
    // crashed — never the only copy — so it is safe to reclaim before reusing
    // the deterministic aside name.
    if aside.exists() {
        if let Err(error) = fs::remove_dir_all(&aside) {
            tracing::warn!(
                "removing superseded leftover {} before swap: {error}",
                aside.display()
            );
        }
    }
    fs::rename(a, &aside)?;
    if let Err(error) = fs::rename(b, a) {
        if let Err(rollback) = fs::rename(&aside, a) {
            tracing::error!(
                "rollback of superseded dir {} to {} failed: {rollback}",
                aside.display(),
                a.display()
            );
        }
        return Err(error).with_context(|| format!("cannot swap {} and {}", a.display(), b.display()));
    }
    if let Err(error) = fs::remove_dir_all(&aside) {
        tracing::warn!(
            "could not remove superseded dir {}: {error}",
            aside.display()
        );
    }
    Ok(())
}
/// Startup recovery for crash leftovers in the servers dir. Runs in two
/// passes:
///
/// 1. **Restore** any `.previous-<name>` whose live dir `<name>` is missing:
///    that is the crash window between the two renames of the non-atomic
///    fallback swap, where the aside holds the ONLY copy of the server's data
///    (NFS/FUSE filesystems without `RENAME_EXCHANGE`). Deleting it would
///    destroy the last surviving copy.
/// 2. **Reclaim** everything provably safe to drop: `.restore-*` staging dirs
///    (their content was freshly extracted from an archive that still exists,
///    so they are never the only copy), `.previous-*` dirs whose live dir is
///    present (a completed swap left them superseded), and stray
///    `.renameat2-probe-*` files from a crashed support probe.
///
/// Old-style `.previous-<random>` aside names (no dash, from versions before
/// the deterministic naming) cannot be mapped to a server dir from the
/// filesystem alone; they are left in place with a warning rather than risk
/// deleting the only copy of a crashed swap.
///
/// MUST be called exactly once at process startup, after
/// `cfg.ensure_dirs()` and before any request is served (called from main.rs
/// boot, next to the database open) — never concurrently with a restore.
pub fn recover_stale_dirs(cfg: &Config) -> Result<usize> {
    recover_stale_dirs_in(&cfg.paths.servers_dir)
}

/// Live server dir names are hyphenated (UUIDs, or test names); pre-change
/// aside names were `.previous-<32 hex>` with no dash. The dash is the
/// discriminator between a mappable new-style aside and an old-style leftover.
fn aside_targets_server_dir(name: &str) -> bool {
    name.contains('-')
}

fn recover_stale_dirs_in(servers_dir: &Path) -> Result<usize> {
    if !servers_dir.exists() {
        return Ok(0);
    }
    let collect = |prefix: &str| -> Result<Vec<(PathBuf, String, fs::FileType)>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(servers_dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with(prefix) {
                out.push((entry.path(), name, entry.file_type()?));
            }
        }
        Ok(out)
    };
    let mut actions = 0usize;
    // Pass 1 — restore before anything is deleted.
    for (path, name, file_type) in collect(".previous-")? {
        if !file_type.is_dir() {
            continue;
        }
        let target_name = &name[".previous-".len()..];
        let target = servers_dir.join(target_name);
        if target.exists() {
            continue; // completed swap; superseded — pass 2 reclaims it.
        }
        if !aside_targets_server_dir(target_name) {
            tracing::warn!(
                "recovery: {} has no matching server dir and no recoverable \
                 server name; leaving it for manual inspection",
                path.display()
            );
            actions += 1;
            continue;
        }
        match fs::rename(&path, &target) {
            Ok(()) => {
                tracing::warn!(
                    "recovery: RESTORED server dir {} from crash leftover {} \
                     (server dir was missing; the aside held the only copy)",
                    target.display(),
                    path.display()
                );
                actions += 1;
            }
            Err(error) => tracing::warn!(
                "recovery: could not restore server dir from {}: {error}",
                path.display()
            ),
        }
    }
    // Pass 2 — reclaim everything provably safe to drop.
    for (path, _name, file_type) in collect(".restore-")? {
        if file_type.is_dir() && fs::remove_dir_all(&path).is_ok() {
            tracing::warn!(
                "recovery: removed stale restore staging dir {}",
                path.display()
            );
            actions += 1;
        }
    }
    for (path, name, file_type) in collect(".previous-")? {
        let target = servers_dir.join(&name[".previous-".len()..]);
        if file_type.is_dir() && target.exists() && fs::remove_dir_all(&path).is_ok() {
            tracing::warn!(
                "recovery: removed superseded dir {} (live dir {} already in place)",
                path.display(),
                target.display()
            );
            actions += 1;
        }
    }
    for (path, _name, file_type) in collect(".renameat2-probe-")? {
        if file_type.is_file() && fs::remove_file(&path).is_ok() {
            actions += 1;
        }
    }
    Ok(actions)
}

fn extract_zip(archive: &std::path::Path, staging: &std::path::Path) -> Result<()> {
    let f = fs::File::open(archive)?;
    let mut zip = zip::ZipArchive::new(f)?;
    if zip.len() > MAX_ARCHIVE_ENTRIES {
        bail!("archive has too many entries (max {MAX_ARCHIVE_ENTRIES})");
    }
    let mut total: u64 = 0;
    for i in 0..zip.len() {
        let entry = zip.by_index(i)?;
        let out = crate::services::files::safe_join(staging, entry.name())?;
        if entry.is_dir() {
            fs::create_dir_all(&out)?;
            continue;
        }
        // Fail on the declared size before writing anything, then hard-cap
        // the bytes actually copied so a lying header cannot overflow.
        let declared = entry.size();
        if declared > MAX_EXTRACT_FILE_BYTES
            || total.saturating_add(declared) > MAX_EXTRACT_TOTAL_BYTES
        {
            bail!("archive entry exceeds extraction size limits");
        }
        if let Some(p) = out.parent() {
            fs::create_dir_all(p)?;
        }
        let mut file = fs::File::create(&out)?;
        let cap = MAX_EXTRACT_FILE_BYTES.saturating_add(1).min(
            MAX_EXTRACT_TOTAL_BYTES
                .saturating_sub(total)
                .saturating_add(1),
        );
        let copied = std::io::copy(&mut entry.take(cap), &mut file)?;
        if copied > MAX_EXTRACT_FILE_BYTES || total.saturating_add(copied) > MAX_EXTRACT_TOTAL_BYTES
        {
            bail!("archive entry exceeds extraction size limits");
        }
        total += copied;
    }
    Ok(())
}
/// Extract a tar.gz archive into `staging`, tar-slip safe and with no link
/// entries (matching `services::files::extract_tar_gz_into`).
fn extract_tar_gz(archive: &std::path::Path, staging: &std::path::Path) -> Result<()> {
    let file = fs::File::open(archive)?;
    let dec = flate2::read::GzDecoder::new(file);
    let mut tar = tar::Archive::new(dec);
    tar.set_unpack_xattrs(false);
    let mut total: u64 = 0;
    let mut count: usize = 0;
    for entry in tar.entries()? {
        count += 1;
        if count > MAX_ARCHIVE_ENTRIES {
            bail!("archive has too many entries (max {MAX_ARCHIVE_ENTRIES})");
        }
        let entry = entry?;
        let kind = entry.header().entry_type();
        if kind.is_symlink() || kind.is_hard_link() {
            bail!("archive link entries are forbidden");
        }
        let rel = entry.path()?.to_string_lossy().to_string();
        let path = crate::services::files::safe_join(staging, &rel)?;
        if kind.is_dir() {
            fs::create_dir_all(&path)?;
            continue;
        }
        // Fail on the declared size before writing anything, then hard-cap
        // the bytes actually copied so a lying header cannot overflow.
        let declared = entry.size();
        if declared > MAX_EXTRACT_FILE_BYTES
            || total.saturating_add(declared) > MAX_EXTRACT_TOTAL_BYTES
        {
            bail!("archive entry exceeds extraction size limits");
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)?;
        let cap = MAX_EXTRACT_FILE_BYTES.saturating_add(1).min(
            MAX_EXTRACT_TOTAL_BYTES
                .saturating_sub(total)
                .saturating_add(1),
        );
        let mode = entry.header().mode().ok();
        let copied = std::io::copy(&mut entry.take(cap), &mut f)?;
        if copied > MAX_EXTRACT_FILE_BYTES || total.saturating_add(copied) > MAX_EXTRACT_TOTAL_BYTES
        {
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

/// Open a backup archive for streaming download. Returns the suggested
/// filename, the open file, and its size so the caller can stream the body
/// with a Content-Length instead of loading the archive into memory.
pub fn download(db: &Db, backup_id: i64) -> Result<(String, std::fs::File, u64)> {
    let backup = models::get_backup(db, backup_id)?;
    let file = fs::File::open(&backup.path).context("backup file missing on disk")?;
    let size = file.metadata()?.len();
    let ext = if backup.format == "tar.gz" {
        "tar.gz"
    } else {
        "zip"
    };
    let name = format!("{}.{}", backup.name, ext);
    Ok((name, file, size))
}

/// Drop the row first: `delete_backup` refuses while the backup is locked, and
/// unlinking ahead of that check would destroy the archive of a pinned backup
/// and leave the surviving row pointing at a missing file.
pub async fn delete(db: &Db, backup_id: i64) -> Result<()> {
    let backup = blocking(db.clone(), move |db| models::get_backup(&db, backup_id)).await?;
    let _guard = server_op_lock(backup.server_id).await;
    // Re-fetch under the lock: the row may have changed while we waited.
    blocking(db.clone(), move |db| {
        let backup = models::get_backup(&db, backup_id)?;
        models::delete_backup(&db, backup_id)?;
        let _ = fs::remove_file(&backup.path);
        Ok(())
    })
    .await?;
    Ok(())
}

pub fn checksum_file(path: &std::path::Path) -> Result<String> {
    let file = fs::File::open(path)?;
    // Stream the digest in 64 KiB chunks: a multi-GB archive must never be
    // read into memory whole just to checksum it.
    let mut reader = std::io::BufReader::with_capacity(64 * 1024, file);
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Verify checksum of a backup on disk.
pub fn verify(db: &Db, backup_id: i64) -> Result<bool> {
    let backup = models::get_backup(db, backup_id)?;
    if !std::path::Path::new(&backup.path).exists() {
        return Ok(false);
    }
    Ok(checksum_file(std::path::Path::new(&backup.path))? == backup.checksum)
}
pub async fn cleanup_old(db: &Db, _cfg: &Config, server_id: i64, keep: i64) -> Result<usize> {
    let _guard = server_op_lock(server_id).await;
    let keep = keep.max(0) as usize;
    let removed = blocking(db.clone(), move |db| {
        let mut removed = 0usize;
        for b in models::rotation_candidates(&db, server_id, keep)? {
            let _ = fs::remove_file(&b.path);
            models::delete_backup(&db, b.id)?;
            removed += 1;
        }
        Ok(removed)
    })
    .await?;
    Ok(removed)
}

// ---------------- Offsite mirror ----------------
/// Consecutive mirror-copy/maintenance failures this process has seen. A
/// fully successful mirror operation resets it to zero; [`mirror_status`]
/// maps zero to `ok` and non-zero to `degraded`. Process-lifetime only: a
/// restart clears it (the next create or an admin re-sync re-probes the
/// mirror).
static MIRROR_FAILURES: AtomicU64 = AtomicU64::new(0);

/// Mirror health surfaced in the backups list response: `disabled` (mirror
/// not enabled in config), `ok`, or `degraded` (enabled, but a mirror
/// operation has failed this process lifetime).
pub fn mirror_status(cfg: &Config) -> &'static str {
    if !cfg.backups.mirror.enabled {
        "disabled"
    } else if MIRROR_FAILURES.load(Ordering::Relaxed) == 0 {
        "ok"
    } else {
        "degraded"
    }
}

/// Mirror subdir for a server: `<mirror.path>/<server-uuid>`.
fn mirror_server_dir(cfg: &Config, server: &models::Server) -> Option<PathBuf> {
    let m = &cfg.backups.mirror;
    if !m.enabled {
        return None;
    }
    Some(m.path.as_ref()?.join(&server.uuid))
}

/// Copy `src` into the mirror for `server`. Returns `Ok(true)` when the
/// archive was copied, `Ok(false)` when it was already present (idempotent
/// re-sync). COPY by design, never hardlink: the mirror exists to survive
/// loss of the primary store, so it must be an independent file — a hardlink
/// shares the primary inode (dying with the primary disk) and fails outright
/// across mounts, which is the usual mirror setup. The copy preserves the
/// source mtime so mirror retention trims by backup age, not copy time.
fn mirror_copy_one(cfg: &Config, server: &models::Server, src: &Path) -> Result<bool> {
    let dir = mirror_server_dir(cfg, server).context("mirror disabled")?;
    let fname = src
        .file_name()
        .context("backup archive path has no filename")?;
    let dest = dir.join(fname);
    if dest.exists() {
        return Ok(false);
    }
    fs::create_dir_all(&dir)?;
    let mut src_f = fs::File::open(src)?;
    let meta = src_f.metadata()?;
    let mut dst_f = fs::File::create(&dest)?;
    std::io::copy(&mut src_f, &mut dst_f)?;
    dst_f.set_modified(meta.modified()?)?;
    Ok(true)
}

/// Enforce `mirror.keep` for one server's mirror subdir: remove the oldest
/// archives (by mtime, preserved from the primary at copy time) beyond the
/// keep count. Only files under the mirror path are touched — the primary
/// backup store is never affected. Best-effort per file: an individual
/// removal failure is skipped, not fatal. Returns archives removed.
fn mirror_trim(cfg: &Config, server: &models::Server) -> Result<usize> {
    let dir = mirror_server_dir(cfg, server).context("mirror disabled")?;
    let keep = cfg.backups.mirror.keep as usize;
    let mut entries: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    for entry in fs::read_dir(&dir)? {
        let Ok(entry) = entry else { continue };
        // `Path::extension()` yields only the last component ("gz"), so match
        // the full ".tar.gz" suffix on the file name instead.
        if !entry
            .path()
            .file_name()
            .map(|n| n.to_string_lossy().ends_with(".tar.gz"))
            .unwrap_or(false)
        {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let Ok(mtime) = meta.modified() else { continue };
        entries.push((mtime, entry.path()));
    }
    entries.sort_by_key(|(mtime, _)| *mtime); // oldest first
    let excess = entries.len().saturating_sub(keep);
    let mut removed = 0usize;
    for (_, path) in entries.into_iter().take(excess) {
        if fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    Ok(removed)
}

/// Mirror a freshly created archive and enforce mirror retention. Best-effort
/// by contract: any failure is logged and bumps the degraded counter but
/// never fails or alters the primary backup. A fully successful operation
/// resets the degraded counter. Called with the per-server op lock held.
pub(crate) fn mirror_archive(cfg: &Config, server: &models::Server, archive: &Path) {
    if !cfg.backups.mirror.enabled {
        return;
    }
    match mirror_copy_one(cfg, server, archive).and_then(|_| mirror_trim(cfg, server)) {
        Ok(removed) => {
            MIRROR_FAILURES.store(0, Ordering::Relaxed);
            tracing::info!(
                server_id = server.id,
                "mirror: archived {}, trimmed {removed} old",
                archive.display()
            );
        }
        Err(e) => {
            MIRROR_FAILURES.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(
                server_id = server.id,
                "mirror: {e} - degraded; primary backup unaffected"
            );
        }
    }
}

/// Outcome of [`mirror_sync`].
pub struct MirrorSyncReport {
    /// Servers scanned.
    pub servers: usize,
    /// Archives actually copied into the mirror (missing ones only).
    pub copied: usize,
    /// Archives trimmed for mirror retention.
    pub removed: usize,
    /// Archives whose mirror copy failed (primary archive missing or
    /// unreadable).
    pub failed: usize,
}

/// Re-sync the mirror from the primary store: copy every archive the DB
/// records that is missing from the mirror, then enforce per-server mirror
/// retention. Idempotent — an intact mirror makes a second run copy and trim
/// nothing. Lock-free by design: it only reads DB rows and performs idempotent
/// file operations; `create` holds the per-server op lock for its own mirror
/// step, and a concurrent duplicate copy writes identical bytes.
pub async fn mirror_sync(db: &Db, cfg: &Config) -> Result<MirrorSyncReport> {
    let db = db.clone();
    let cfg = cfg.clone();
    tokio::task::spawn_blocking(move || {
        let mut report = MirrorSyncReport {
            servers: 0,
            copied: 0,
            removed: 0,
            failed: 0,
        };
        if !cfg.backups.mirror.enabled {
            return Ok(report);
        }
        let mut any_failure = false;
        for server in models::list_servers(&db, None, false)? {
            report.servers += 1;
            for b in models::list_backups(&db, server.id)? {
                match mirror_copy_one(&cfg, &server, Path::new(&b.path)) {
                    Ok(true) => report.copied += 1,
                    Ok(false) => {}
                    Err(e) => {
                        report.failed += 1;
                        any_failure = true;
                        tracing::warn!(server_id = server.id, "mirror sync: {} - {e}", b.name);
                    }
                }
            }
            match mirror_trim(&cfg, &server) {
                Ok(n) => report.removed += n,
                Err(e) => {
                    any_failure = true;
                    tracing::warn!(server_id = server.id, "mirror sync trim: {e}");
                }
            }
        }
        if any_failure {
            MIRROR_FAILURES.fetch_add(1, Ordering::Relaxed);
        } else {
            MIRROR_FAILURES.store(0, Ordering::Relaxed);
        }
        Ok(report)
    })
    .await
    .context("mirror sync worker panicked")?
}

#[allow(dead_code)]
pub fn now_stamp() -> String {
    Utc::now().format("%Y%m%d-%H%M%S").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;

    fn test_env() -> (Db, Config, i64, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let db = crate::db::open(tmp.path().join("t.db").to_str().unwrap()).unwrap();
        let mut cfg = Config::default();
        cfg.paths.servers_dir = tmp.path().join("servers");
        cfg.paths.backups_dir = tmp.path().join("backups");
        std::fs::create_dir_all(&cfg.paths.servers_dir).unwrap();
        std::fs::create_dir_all(&cfg.paths.backups_dir).unwrap();
        let conn = db.get().unwrap();
        conn.execute(
            "INSERT INTO users(username,email,password_hash,created_at,updated_at)
             VALUES('u','u@t','x','now','now')",
            [],
        )
        .unwrap();
        let uid = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO blueprints(uuid,name,created_at,updated_at)
             VALUES('b','b','now','now')",
            [],
        )
        .unwrap();
        let bid = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO servers(uuid,name,user_id,blueprint_id,created_at,updated_at)
             VALUES('srv-uuid','s',?1,?2,'now','now')",
            params![uid, bid],
        )
        .unwrap();
        let sid = conn.last_insert_rowid();
        drop(conn);
        (db, cfg, sid, tmp)
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Runtime::new().unwrap()
    }

    #[test]
    fn restore_refuses_checksum_mismatch() {
        let (db, cfg, sid, _tmp) = test_env();
        let archive = cfg.paths.backups_dir.join("b.tar.gz");
        fs::write(&archive, b"original").unwrap();
        let checksum = checksum_file(&archive).unwrap();
        let id = models::create_backup(
            &db,
            "u1",
            sid,
            "b",
            &archive.to_string_lossy(),
            8,
            &checksum,
            "tar.gz",
            "",
        )
        .unwrap();
        // Corrupt the archive on disk after the backup row was recorded.
        fs::write(&archive, b"tampered!").unwrap();
        let err = rt().block_on(restore(&db, &cfg, id)).unwrap_err();
        assert!(err.to_string().contains("checksum mismatch"));
        // Server dir must be untouched.
        let srv_dir = cfg.paths.servers_dir.join("srv-uuid");
        assert!(!srv_dir.exists());
    }

    #[test]
    fn restore_tar_gz_roundtrip_replaces_contents() {
        let (db, cfg, sid, _tmp) = test_env();
        let srv_dir = cfg.paths.servers_dir.join("srv-uuid");
        fs::create_dir_all(&srv_dir).unwrap();
        fs::write(srv_dir.join("hello.txt"), b"one").unwrap();
        let server = models::get_server(&db, sid).unwrap();
        let archive = cfg.paths.backups_dir.join("b.tar.gz");
        let size = crate::services::files::tar_gz_dir(&cfg, &server, ".", &archive).unwrap();
        let checksum = checksum_file(&archive).unwrap();
        // Mutate the live dir after the backup was taken; restore must
        // roll it back to the archived state.
        fs::write(srv_dir.join("hello.txt"), b"two").unwrap();
        let id = models::create_backup(
            &db,
            "u2",
            sid,
            "b",
            &archive.to_string_lossy(),
            size as i64,
            &checksum,
            "tar.gz",
            "",
        )
        .unwrap();
        rt().block_on(restore(&db, &cfg, id)).unwrap();
        assert_eq!(
            fs::read_to_string(srv_dir.join("hello.txt")).unwrap(),
            "one"
        );
    }

    #[test]
    fn download_name_follows_stored_format() {
        let (db, cfg, sid, _tmp) = test_env();
        let zip = cfg.paths.backups_dir.join("z.zip");
        fs::write(&zip, b"z").unwrap();
        let zid = models::create_backup(
            &db,
            "u3",
            sid,
            "z",
            &zip.to_string_lossy(),
            1,
            &checksum_file(&zip).unwrap(),
            "zip",
            "",
        )
        .unwrap();
        let tar = cfg.paths.backups_dir.join("t.tar.gz");
        fs::write(&tar, b"t").unwrap();
        let tid = models::create_backup(
            &db,
            "u4",
            sid,
            "t",
            &tar.to_string_lossy(),
            1,
            &checksum_file(&tar).unwrap(),
            "tar.gz",
            "",
        )
        .unwrap();
        let (zip_name, _, _) = download(&db, zid).unwrap();
        let (tar_name, _, _) = download(&db, tid).unwrap();
        assert!(zip_name.ends_with(".zip"));
        assert!(tar_name.ends_with(".tar.gz"));
    }
    #[test]
    fn extract_zip_rejects_slip_entries() {
        use std::io::Write;
        let tmp = tempfile::tempdir().unwrap();
        let staging = tmp.path().join("staging");
        fs::create_dir_all(&staging).unwrap();
        let archive = tmp.path().join("evil.zip");
        {
            let f = fs::File::create(&archive).unwrap();
            let mut w = zip::ZipWriter::new(f);
            let opts = zip::write::FileOptions::default();
            // A relative escape: safe_join must refuse it.
            w.start_file("../evil.txt", opts).unwrap();
            w.write_all(b"x").unwrap();
            w.finish().unwrap();
        }
        let err = extract_zip(&archive, &staging).unwrap_err();
        assert!(err.to_string().contains("escape"), "got: {err}");
        assert!(!tmp.path().join("evil.txt").exists());
    }

    #[test]
    fn extract_zip_rejects_too_many_entries() {
        use std::io::Write;
        let tmp = tempfile::tempdir().unwrap();
        let staging = tmp.path().join("staging");
        fs::create_dir_all(&staging).unwrap();
        let archive = tmp.path().join("many.zip");
        {
            let f = fs::File::create(&archive).unwrap();
            let mut w = zip::ZipWriter::new(f);
            let opts = zip::write::FileOptions::default();
            for i in 0..=MAX_ARCHIVE_ENTRIES {
                w.start_file(format!("f{i}"), opts).unwrap();
                w.write_all(b"x").unwrap();
            }
            w.finish().unwrap();
        }
        let err = extract_zip(&archive, &staging).unwrap_err();
        assert!(err.to_string().contains("too many entries"), "got: {err}");
    }

    #[test]
    fn extract_tar_gz_rejects_link_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let staging = tmp.path().join("staging");
        fs::create_dir_all(&staging).unwrap();
        let archive = tmp.path().join("link.tar.gz");
        {
            let f = fs::File::create(&archive).unwrap();
            let enc = flate2::write::GzEncoder::new(f, flate2::Compression::default());
            let mut w = tar::Builder::new(enc);
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(tar::EntryType::Symlink);
            h.set_size(0);
            h.set_path("link").unwrap();
            h.set_link_name("/etc/passwd").unwrap();
            w.append(&h, std::io::empty()).unwrap();
            w.into_inner().unwrap().finish().unwrap();
        }
        let err = extract_tar_gz(&archive, &staging).unwrap_err();
        assert!(err.to_string().contains("link"), "got: {err}");
        assert!(!tmp.path().join("link").exists());
    }

    #[test]
    fn delete_keeps_archive_and_row_while_locked() {
        let (db, cfg, sid, _tmp) = test_env();
        let archive = cfg.paths.backups_dir.join("pinned.tar.gz");
        fs::write(&archive, b"pinned").unwrap();
        let checksum = checksum_file(&archive).unwrap();
        let id = models::create_backup(
            &db,
            "u-lock",
            sid,
            "pinned",
            &archive.to_string_lossy(),
            6,
            &checksum,
            "tar.gz",
            "",
        )
        .unwrap();
        models::set_backup_locked(&db, id, true).unwrap();

        let err = rt().block_on(delete(&db, id)).unwrap_err();
        assert!(err.to_string().contains("locked"), "got: {err}");
        // The archive a locked row points at must survive the refused delete.
        assert!(archive.exists());
        assert!(models::get_backup(&db, id).is_ok());

        models::set_backup_locked(&db, id, false).unwrap();
        rt().block_on(delete(&db, id)).unwrap();
        assert!(!archive.exists());
        assert!(models::get_backup(&db, id).is_err());
    }

    #[test]
    fn recovery_restores_only_copy_in_previous_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let servers = tmp.path().join("servers");
        fs::create_dir_all(&servers).unwrap();
        // Crash window of the non-atomic fallback swap: the server dir was
        // renamed aside and the process died before the staging rename landed —
        // the aside is the ONLY surviving copy and must be restored, not swept.
        let aside = servers.join(".previous-srv-uuid");
        fs::create_dir_all(&aside).unwrap();
        fs::write(aside.join("data.txt"), b"live-data").unwrap();
        // A superseded leftover (live dir present): safe to reclaim.
        let superseded = servers.join(".previous-other-srv");
        fs::create_dir_all(&superseded).unwrap();
        fs::write(superseded.join("old.txt"), b"old").unwrap();
        fs::create_dir_all(servers.join("other-srv")).unwrap();
        // Staging dir: never the only copy, reclaimed.
        let staging = servers.join(".restore-abc");
        fs::create_dir_all(&staging).unwrap();
        fs::write(staging.join("stage.txt"), b"stage").unwrap();
        // Old-style unparseable leftover (no dash): must NOT be deleted.
        let old = servers.join(".previous-0123456789abcdef0123456789abcdef");
        fs::create_dir_all(&old).unwrap();
        fs::write(old.join("maybe-live.txt"), b"keep-me").unwrap();

        let actions = recover_stale_dirs_in(&servers).unwrap();
        // restore(1) + superseded removal(1) + staging removal(1) + old-style
        // leftover left for manual review(1).
        assert_eq!(actions, 4);
        // The only copy is back where it belongs.
        assert!(servers.join("srv-uuid").is_dir());
        assert_eq!(
            fs::read_to_string(servers.join("srv-uuid/data.txt")).unwrap(),
            "live-data"
        );
        assert!(!aside.exists());
        // Superseded leftover reclaimed; live dir untouched.
        assert!(!superseded.exists());
        assert!(servers.join("other-srv").is_dir());
        // Staging reclaimed; old-style leftover preserved for manual review.
        assert!(!staging.exists());
        assert!(old.exists());
        assert_eq!(
            fs::read_to_string(old.join("maybe-live.txt")).unwrap(),
            "keep-me"
        );
    }

    #[test]
    fn fallback_swap_reuses_deterministic_aside_name() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("srv-uuid");
        let b = tmp.path().join("staging");
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        fs::write(a.join("old.txt"), b"old").unwrap();
        fs::write(b.join("new.txt"), b"new").unwrap();
        // Leftover from an earlier completed swap whose cleanup crashed. The
        // live dir `a` still existing proves it superseded, so the fallback
        // reclaims it before reusing the deterministic aside name.
        let leftover = tmp.path().join(".previous-srv-uuid");
        fs::create_dir_all(&leftover).unwrap();
        fs::write(leftover.join("x.txt"), b"superseded").unwrap();

        exchange_dirs_fallback(&a, &b).unwrap();
        assert!(!leftover.exists());
        assert!(a.join("new.txt").exists());
        assert!(!a.join("old.txt").exists());
        assert!(!b.exists()); // staging was renamed into place at `a`
    }

    #[test]
    fn create_enqueues_backup_complete_webhook_delivery() {
        let (db, cfg, sid, _tmp) = test_env();
        let srv_dir = cfg.paths.servers_dir.join("srv-uuid");
        fs::create_dir_all(&srv_dir).unwrap();
        fs::write(srv_dir.join("f.txt"), b"data").unwrap();
        // Subscribe a webhook to backup.* scoped to the server: the emit path
        // (create -> emit_backup_event -> webhooks::emit -> deliveries) must
        // land exactly one delivery with the backup identity.
        {
            let conn = db.get().unwrap();
            conn.execute(
                "INSERT INTO webhooks(uuid,name,url,secret,events,server_id,enabled,created_at,updated_at)
                 VALUES('wh-uuid','wh','https://hooks.example/x','0123456789abcdef','[\"backup.*\"]',?1,1,'now','now')",
                [sid],
            )
            .unwrap();
        }
        let (id, size, checksum) = rt().block_on(create(&db, &cfg, sid, "b", "")).unwrap();
        assert!(size > 0);

        let conn = db.get().unwrap();
        let (event, payload): (String, String) = conn
            .query_row(
                "SELECT event, payload FROM webhook_deliveries",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(event, "backup.complete");
        let v: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(v["server_id"], sid);
        assert_eq!(v["uuid"], "srv-uuid");
        assert_eq!(v["operation"], "create");
        assert_eq!(v["backup_id"], id);
        assert_eq!(v["backup_name"], "b");
        assert_eq!(v["size"], size);
        assert_eq!(v["checksum"], checksum);
    }

    #[test]
    fn restore_failure_enqueues_backup_failed_webhook_delivery() {
        let (db, cfg, sid, _tmp) = test_env();
        let archive = cfg.paths.backups_dir.join("b.tar.gz");
        fs::write(&archive, b"original").unwrap();
        let checksum = checksum_file(&archive).unwrap();
        let id = models::create_backup(
            &db,
            "u1",
            sid,
            "b",
            &archive.to_string_lossy(),
            8,
            &checksum,
            "tar.gz",
            "",
        )
        .unwrap();
        // Corrupt the archive on disk after the backup row was recorded.
        fs::write(&archive, b"tampered!").unwrap();
        {
            let conn = db.get().unwrap();
            conn.execute(
                "INSERT INTO webhooks(uuid,name,url,secret,events,server_id,enabled,created_at,updated_at)
                 VALUES('wh-uuid','wh','https://hooks.example/x','0123456789abcdef','[\"backup.failed\"]',?1,1,'now','now')",
                [sid],
            )
            .unwrap();
        }
        let err = rt().block_on(restore(&db, &cfg, id)).unwrap_err();
        assert!(err.to_string().contains("checksum mismatch"));

        let conn = db.get().unwrap();
        let (event, payload): (String, String) = conn
            .query_row(
                "SELECT event, payload FROM webhook_deliveries",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(event, "backup.failed");
        let v: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(v["server_id"], sid);
        assert_eq!(v["operation"], "restore");
        assert_eq!(v["backup_id"], id);
        assert_eq!(v["backup_name"], "b");
        assert!(v["error"].as_str().unwrap().contains("checksum mismatch"));
    }

    /// Mirror tests share the process-global degraded counter and the mirror
    /// dir; serialize them so parallel test threads cannot interleave.
    static MIRROR_TESTS: std::sync::LazyLock<parking_lot::Mutex<()>> =
        std::sync::LazyLock::new(|| parking_lot::Mutex::new(()));

    #[test]
    fn mirror_copies_archives_and_trims_oldest_per_server() {
        let _g = MIRROR_TESTS.lock();
        let (db, mut cfg, sid, tmp) = test_env();
        cfg.backups.mirror.enabled = true;
        cfg.backups.mirror.path = Some(tmp.path().join("mirror"));
        cfg.backups.mirror.keep = 2;
        let srv_dir = cfg.paths.servers_dir.join("srv-uuid");
        fs::create_dir_all(&srv_dir).unwrap();
        for i in 0..4 {
            fs::write(srv_dir.join("f.txt"), format!("data-{i}")).unwrap();
            rt().block_on(create(&db, &cfg, sid, &format!("b{i}"), "")).unwrap();
        }
        // Mirror retains exactly `keep` archives, per server.
        let mirror_dir = tmp.path().join("mirror").join("srv-uuid");
        let count = fs::read_dir(&mirror_dir).unwrap().count();
        assert_eq!(count, 2, "mirror retention must keep exactly keep archives");
        // Primary store is untouched: all 4 archives still present.
        let primary_count = fs::read_dir(&cfg.paths.backups_dir).unwrap().count();
        assert_eq!(primary_count, 4);
        assert_eq!(mirror_status(&cfg), "ok");
    }

    #[test]
    fn mirror_degraded_on_failure_and_recovers_on_success() {
        let _g = MIRROR_TESTS.lock();
        let (db, mut cfg, sid, tmp) = test_env();
        cfg.backups.mirror.enabled = true;
        // Mirror path blocked by a regular file: create_dir_all fails.
        let blocker = tmp.path().join("blocker");
        fs::write(&blocker, b"x").unwrap();
        cfg.backups.mirror.path = Some(blocker.join("mirror"));
        cfg.backups.mirror.keep = 2;
        let srv_dir = cfg.paths.servers_dir.join("srv-uuid");
        fs::create_dir_all(&srv_dir).unwrap();
        fs::write(srv_dir.join("f.txt"), b"data").unwrap();
        let (id, _, _) = rt().block_on(create(&db, &cfg, sid, "b", "")).unwrap();
        // Primary backup succeeded despite the mirror failure...
        assert_eq!(models::get_backup(&db, id).unwrap().id, id);
        assert_eq!(mirror_status(&cfg), "degraded");
        // ...fix the mirror path; the next create recovers the status.
        fs::remove_file(&blocker).unwrap();
        let (id2, _, _) = rt().block_on(create(&db, &cfg, sid, "b2", "")).unwrap();
        assert_eq!(models::get_backup(&db, id2).unwrap().id, id2);
        assert_eq!(mirror_status(&cfg), "ok");
        assert!(tmp.path().join("blocker").join("mirror").join("srv-uuid").is_dir());
    }

    #[test]
    fn mirror_sync_repairs_missing_archives_and_is_idempotent() {
        let _g = MIRROR_TESTS.lock();
        let (db, mut cfg, sid, tmp) = test_env();
        cfg.backups.mirror.enabled = true;
        cfg.backups.mirror.path = Some(tmp.path().join("mirror"));
        cfg.backups.mirror.keep = 10;
        let srv_dir = cfg.paths.servers_dir.join("srv-uuid");
        fs::create_dir_all(&srv_dir).unwrap();
        for i in 0..3 {
            fs::write(srv_dir.join("f.txt"), format!("data-{i}")).unwrap();
            rt().block_on(create(&db, &cfg, sid, &format!("b{i}"), "")).unwrap();
        }
        let mirror_dir = tmp.path().join("mirror").join("srv-uuid");
        assert_eq!(fs::read_dir(&mirror_dir).unwrap().count(), 3);
        // Simulate mirror loss: wipe it, then repair via re-sync.
        fs::remove_dir_all(&mirror_dir).unwrap();
        let report = rt().block_on(mirror_sync(&db, &cfg)).unwrap();
        assert_eq!(report.servers, 1);
        assert_eq!(report.copied, 3);
        assert_eq!(report.failed, 0);
        assert_eq!(fs::read_dir(&mirror_dir).unwrap().count(), 3);
        // Idempotent: a second sync copies and trims nothing.
        let report = rt().block_on(mirror_sync(&db, &cfg)).unwrap();
        assert_eq!(report.copied, 0);
        assert_eq!(report.removed, 0);
        assert_eq!(report.failed, 0);
        assert_eq!(mirror_status(&cfg), "ok");
    }
}