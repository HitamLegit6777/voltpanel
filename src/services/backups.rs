//! Backup service: create/restore/download backups of server directories.
use crate::config::Config;
use crate::db::Db;
use crate::models;
use anyhow::{Context, Result};
use chrono::Utc;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::PathBuf;

/// Create a zip backup of the server dir. Returns (backup_id, size, checksum).
pub async fn create(db: &Db, cfg: &Config, server_id: i64, name: &str) -> Result<(i64, u64, String)> {
    let server = models::get_server(db, server_id)?;
    let uuid = uuid::Uuid::new_v4().to_string();
    let fname = format!("{uuid}.zip");
    let out = cfg.paths.backups_dir.join(&fname);
    fs::create_dir_all(&cfg.paths.backups_dir)?;
    let size = crate::services::files::zip_dir(cfg, &server, ".", &out)?;
    let checksum = checksum_file(&out)?;
    let id = models::create_backup(db, &uuid, server_id, name, &out.to_string_lossy(), size as i64, &checksum, "zip")?;
    Ok((id, size, checksum))
}

/// Restore a backup: replace server dir contents.
pub async fn restore(db: &Db, cfg: &Config, backup_id: i64) -> Result<()> {
    let backup = models::get_backup(db, backup_id)?;
    let server = models::get_server(db, backup.server_id)?;
    let dir = cfg.paths.servers_dir.join(&server.uuid);
    // wipe current contents (except install)
    if dir.exists() {
        fs::remove_dir_all(&dir)?;
    }
    fs::create_dir_all(&dir)?;
    let archive = PathBuf::from(&backup.path);
    if backup.format == "zip" {
        // extract zip into dir (safe_join blocks zip-slip entries)
        let f = fs::File::open(&archive)?;
        let mut zip = zip::ZipArchive::new(f)?;
        for i in 0..zip.len() {
            let mut entry = zip.by_index(i)?;
            let out_path = crate::services::files::safe_join(&dir, &entry.name())?;
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
    } else {
        return Err(anyhow::anyhow!("unsupported backup format"));
    }
    Ok(())
}

/// Download backup bytes.
pub fn download(db: &Db, backup_id: i64) -> Result<(String, Vec<u8>)> {
    let backup = models::get_backup(db, backup_id)?;
    let bytes = fs::read(&backup.path).context("backup file missing on disk")?;
    let name = format!("{}.zip", backup.name);
    Ok((name, bytes))
}

pub fn delete(db: &Db, backup_id: i64) -> Result<()> {
    let backup = models::get_backup(db, backup_id)?;
    let _ = fs::remove_file(&backup.path);
    models::delete_backup(db, backup_id)
}

pub fn checksum_file(path: &std::path::Path) -> Result<String> {
    let bytes = fs::read(path)?;
    let mut h = Sha256::new();
    h.update(&bytes);
    Ok(hex::encode(h.finalize()))
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
    let backups = models::list_backups(db, server_id)?;
    let mut removed = 0usize;
    for (i, b) in backups.iter().enumerate() {
        if i as i64 >= keep {
            let _ = fs::remove_file(&b.path);
            models::delete_backup(db, b.id)?;
            removed += 1;
        }
    }
    Ok(removed)
}

#[allow(dead_code)]
pub fn now_stamp() -> String {
    Utc::now().format("%Y%m%d-%H%M%S").to_string()
}
