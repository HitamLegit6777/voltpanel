//! Database-backed data models + CRUD helpers.
use crate::db::Db;
use anyhow::{bail, Context, Result};
use chrono::Utc;
use rusqlite::{params, OptionalExtension, Row};
use serde::{Deserialize, Serialize};

fn now() -> String {
    Utc::now().to_rfc3339()
}

// ---------------- User ----------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub id: i64,
    pub username: String,
    pub email: String,
    pub avatar: String,
    pub language: String,
    pub theme: String,
    pub root_admin: bool,
    pub active: bool,
    pub twofa_secret: Option<String>,
    pub about: String,
    pub created_at: String,
    pub updated_at: String,
}

pub fn user_from_row(r: &Row) -> rusqlite::Result<User> {
    Ok(User {
        id: r.get(0)?,
        username: r.get(1)?,
        email: r.get(2)?,
        avatar: r.get(3)?,
        language: r.get(4)?,
        theme: r.get(5)?,
        root_admin: r.get::<_, i64>(6)? != 0,
        active: r.get::<_, i64>(7)? != 0,
        twofa_secret: r.get(8)?,
        about: r.get(9)?,
        created_at: r.get(10)?,
        updated_at: r.get(11)?,
    })
}

pub fn get_user(db: &Db, id: i64) -> Result<User> {
    let conn = db.lock();
    conn.query_row(
        "SELECT id,username,email,avatar,language,theme,root_admin,active,twofa_secret,about,created_at,updated_at FROM users WHERE id=?1",
        [id],
        user_from_row,
    )
    .context("user not found")
}

pub fn get_user_by_name(db: &Db, username: &str) -> Result<User> {
    let conn = db.lock();
    conn.query_row(
        "SELECT id,username,email,avatar,language,theme,root_admin,active,twofa_secret,about,created_at,updated_at FROM users WHERE username=?1",
        [username],
        user_from_row,
    )
    .context("user not found")
}

pub fn get_user_by_email(db: &Db, email: &str) -> Result<User> {
    let conn = db.lock();
    conn.query_row(
        "SELECT id,username,email,avatar,language,theme,root_admin,active,twofa_secret,about,created_at,updated_at FROM users WHERE email=?1",
        [email],
        user_from_row,
    )
    .context("user not found")
}

pub fn list_users(db: &Db) -> Result<Vec<User>> {
    let conn = db.lock();
    let mut stmt = conn.prepare(
        "SELECT id,username,email,avatar,language,theme,root_admin,active,twofa_secret,about,created_at,updated_at FROM users ORDER BY id",
    )?;
    let rows = stmt.query_map([], user_from_row)?;
    let mut out = Vec::new();
    for u in rows {
        out.push(u?);
    }
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
pub fn create_user(
    db: &Db,
    username: &str,
    email: &str,
    password_hash: &str,
    root_admin: bool,
    language: &str,
    theme: &str,
) -> Result<i64> {
    let conn = db.lock();
    let t = now();
    conn.execute(
        "INSERT INTO users(username,email,password_hash,root_admin,language,theme,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?7)",
        params![username, email, password_hash, root_admin as i64, language, theme, t],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn update_user(db: &Db, u: &User) -> Result<()> {
    let conn = db.lock();
    conn.execute(
        "UPDATE users SET email=?1, avatar=?2, language=?3, theme=?4, root_admin=?5, active=?6, about=?7, updated_at=?8 WHERE id=?9",
        params![
            u.email,
            u.avatar,
            u.language,
            u.theme,
            u.root_admin as i64,
            u.active as i64,
            u.about,
            now(),
            u.id
        ],
    )?;
    Ok(())
}

pub fn set_password(db: &Db, user_id: i64, hash: &str) -> Result<()> {
    let conn = db.lock();
    conn.execute(
        "UPDATE users SET password_hash=?1, updated_at=?2 WHERE id=?3",
        params![hash, now(), user_id],
    )?;
    Ok(())
}

pub fn set_twofa_secret(db: &Db, user_id: i64, secret: Option<&str>) -> Result<()> {
    let conn = db.lock();
    conn.execute(
        "UPDATE users SET twofa_secret=?1, updated_at=?2 WHERE id=?3",
        params![secret, now(), user_id],
    )?;
    Ok(())
}

pub fn delete_user(db: &Db, id: i64) -> Result<()> {
    let conn = db.lock();
    conn.execute("DELETE FROM users WHERE id=?1", [id])?;
    Ok(())
}

// ---------------- Egg ----------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EggVariable {
    pub name: String,
    pub description: String,
    pub env_var: String,
    pub default_value: String,
    pub user_viewable: bool,
    pub user_editable: bool,
    pub rules: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Egg {
    pub id: i64,
    pub uuid: String,
    pub name: String,
    pub description: String,
    pub author: String,
    pub category: String,
    pub docker_image: String,
    pub startup: String,
    pub default_config: Option<String>,
    pub config_files: Option<String>,
    pub install_script: Option<String>,
    pub variables: Vec<EggVariable>,
    pub stop_command: String,
    pub created_at: String,
    pub updated_at: String,
}

fn egg_from_row(r: &Row) -> rusqlite::Result<Egg> {
    Ok(Egg {
        id: r.get(0)?,
        uuid: r.get(1)?,
        name: r.get(2)?,
        description: r.get(3)?,
        author: r.get(4)?,
        category: r.get(5)?,
        docker_image: r.get(6)?,
        startup: r.get(7)?,
        default_config: r.get(8)?,
        config_files: r.get(9)?,
        install_script: r.get(10)?,
        variables: serde_json::from_str(&r.get::<_, String>(11)?).unwrap_or_default(),
        stop_command: r.get(12)?,
        created_at: r.get(13)?,
        updated_at: r.get(14)?,
    })
}

pub fn get_egg(db: &Db, id: i64) -> Result<Egg> {
    let conn = db.lock();
    conn.query_row(
        "SELECT id,uuid,name,description,author,category,docker_image,startup,default_config,config_files,install_script,variables,stop_command,created_at,updated_at FROM egss WHERE id=?1",
        [id],
        egg_from_row,
    )
    .context("egg not found")
}

pub fn list_eggs(db: &Db) -> Result<Vec<Egg>> {
    let conn = db.lock();
    let mut stmt = conn.prepare(
        "SELECT id,uuid,name,description,author,category,docker_image,startup,default_config,config_files,install_script,variables,stop_command,created_at,updated_at FROM egss ORDER BY name",
    )?;
    let rows = stmt.query_map([], egg_from_row)?;
    let mut out = Vec::new();
    for e in rows {
        out.push(e?);
    }
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
pub fn create_egg(
    db: &Db,
    uuid: &str,
    name: &str,
    description: &str,
    author: &str,
    category: &str,
    docker_image: &str,
    startup: &str,
    default_config: Option<&str>,
    config_files: Option<&str>,
    install_script: Option<&str>,
    variables: &[EggVariable],
    stop_command: &str,
) -> Result<i64> {
    let conn = db.lock();
    let t = now();
    conn.execute(
        "INSERT INTO egss(uuid,name,description,author,category,docker_image,startup,default_config,config_files,install_script,variables,stop_command,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?13)",
        params![
            uuid,
            name,
            description,
            author,
            category,
            docker_image,
            startup,
            default_config,
            config_files,
            install_script,
            serde_json::to_string(variables)?,
            stop_command,
            t
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn update_egg(db: &Db, e: &Egg) -> Result<()> {
    let conn = db.lock();
    conn.execute(
        "UPDATE egss SET name=?1, description=?2, author=?3, category=?4, docker_image=?5, startup=?6, default_config=?7, config_files=?8, install_script=?9, variables=?10, stop_command=?11, updated_at=?12 WHERE id=?13",
        params![
            e.name,
            e.description,
            e.author,
            e.category,
            e.docker_image,
            e.startup,
            e.default_config,
            e.config_files,
            e.install_script,
            serde_json::to_string(&e.variables)?,
            e.stop_command,
            now(),
            e.id
        ],
    )?;
    Ok(())
}

pub fn delete_egg(db: &Db, id: i64) -> Result<()> {
    let conn = db.lock();
    let used: i64 = conn.query_row(
        "SELECT COUNT(*) FROM servers WHERE egg_id=?1 AND deleted=0",
        [id],
        |r| r.get(0),
    )?;
    if used > 0 {
        bail!("egg is used by {used} server(s)");
    }
    conn.execute("DELETE FROM egss WHERE id=?1", [id])?;
    Ok(())
}

// ---------------- Server ----------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Server {
    pub id: i64,
    pub uuid: String,
    pub name: String,
    pub user_id: i64,
    pub egg_id: i64,
    pub description: String,
    pub status: String,
    pub docker_image: String,
    pub startup: String,
    pub node: String,
    pub port: Option<i64>,
    pub memory_mb: i64,
    pub disk_mb: i64,
    pub cpu_percent: i64,
    pub threads: String,
    pub suspended: bool,
    pub auto_restart: bool,
    pub restart_count: i64,
    pub ignore_oom: bool,
    pub created_at: String,
    pub updated_at: String,
}

fn server_from_row(r: &Row) -> rusqlite::Result<Server> {
    Ok(Server {
        id: r.get(0)?,
        uuid: r.get(1)?,
        name: r.get(2)?,
        user_id: r.get(3)?,
        egg_id: r.get(4)?,
        description: r.get(5)?,
        status: r.get(6)?,
        docker_image: r.get(7)?,
        startup: r.get(8)?,
        node: r.get(9)?,
        port: r.get(10)?,
        memory_mb: r.get(11)?,
        disk_mb: r.get(12)?,
        cpu_percent: r.get(13)?,
        threads: r.get(14)?,
        suspended: r.get::<_, i64>(15)? != 0,
        auto_restart: r.get::<_, i64>(16)? != 0,
        restart_count: r.get(17)?,
        ignore_oom: r.get::<_, i64>(18)? != 0,
        created_at: r.get(19)?,
        updated_at: r.get(20)?,
    })
}

const SERVER_COLS: &str =
    "id,uuid,name,user_id,egg_id,description,status,docker_image,startup,node,port,memory_mb,disk_mb,cpu_percent,threads,suspended,auto_restart,restart_count,ignore_oom,created_at,updated_at";

pub fn get_server(db: &Db, id: i64) -> Result<Server> {
    let conn = db.lock();
    conn.query_row(
        &format!("SELECT {SERVER_COLS} FROM servers WHERE id=?1 AND deleted=0"),
        [id],
        server_from_row,
    )
    .context("server not found")
}

pub fn set_server_node(db: &Db, id: i64, node: &str) -> Result<()> {
    let mut conn = db.lock();
    let tx = conn.transaction()?;
    tx.execute("UPDATE allocations SET node=?1 WHERE server_id=?2", params![node,id])?;
    tx.execute("UPDATE servers SET node=?1,updated_at=?2 WHERE id=?3", params![node,now(),id])?;
    tx.commit()?;
    Ok(())
}

pub fn servers_on_node(db: &Db, node: &str) -> Result<Vec<Server>> {
    let conn = db.lock();
    let mut stmt = conn.prepare(&format!("SELECT {SERVER_COLS} FROM servers WHERE node=?1 AND deleted=0 ORDER BY name"))?;
    let rows = stmt.query_map([node], server_from_row)?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}
pub fn get_server_by_uuid(db: &Db, uuid: &str) -> Result<Server> {
    let conn = db.lock();
    conn.query_row(
        &format!("SELECT {SERVER_COLS} FROM servers WHERE uuid=?1 AND deleted=0"),
        [uuid],
        server_from_row,
    )
    .context("server not found")
}

pub fn list_servers(db: &Db, user_id: Option<i64>, include_deleted: bool) -> Result<Vec<Server>> {
    let conn = db.lock();
    let (sql, extra) = match user_id {
        Some(uid) => (
            format!(
                "SELECT {SERVER_COLS} FROM servers WHERE user_id=?1{} ORDER BY name",
                if include_deleted { "" } else { " AND deleted=0" }
            ),
            vec![uid.to_string()],
        ),
        None => (
            format!(
                "SELECT {SERVER_COLS} FROM servers{} ORDER BY name",
                if include_deleted { "" } else { " WHERE deleted=0" }
            ),
            vec![],
        ),
    };
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(extra.iter()), server_from_row)?;
    let mut out = Vec::new();
    for s in rows {
        out.push(s?);
    }
    Ok(out)
}

pub fn count_servers(db: &Db) -> Result<i64> {
    let conn = db.lock();
    Ok(conn.query_row("SELECT COUNT(*) FROM servers WHERE deleted=0", [], |r| r.get(0))?)
}

pub fn count_servers_by_user(db: &Db, user_id: i64) -> Result<i64> {
    let conn = db.lock();
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM servers WHERE user_id=?1 AND deleted=0",
        [user_id],
        |r| r.get(0),
    )?)
}

#[allow(clippy::too_many_arguments)]
pub fn create_server(
    db: &Db,
    uuid: &str,
    name: &str,
    user_id: i64,
    egg_id: i64,
    docker_image: &str,
    startup: &str,
    memory_mb: i64,
    disk_mb: i64,
    cpu_percent: i64,
    threads: &str,
) -> Result<i64> {
    let conn = db.lock();
    let t = now();
    conn.execute(
        "INSERT INTO servers(uuid,name,user_id,egg_id,docker_image,startup,memory_mb,disk_mb,cpu_percent,threads,status,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,'offline',?11,?11)",
        params![uuid, name, user_id, egg_id, docker_image, startup, memory_mb, disk_mb, cpu_percent, threads, t],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn update_server(db: &Db, s: &Server) -> Result<()> {
    let conn = db.lock();
    conn.execute(
        "UPDATE servers SET name=?1, description=?2, docker_image=?3, startup=?4, memory_mb=?5, disk_mb=?6, cpu_percent=?7, threads=?8, auto_restart=?9, ignore_oom=?10, updated_at=?11 WHERE id=?12",
        params![
            s.name,
            s.description,
            s.docker_image,
            s.startup,
            s.memory_mb,
            s.disk_mb,
            s.cpu_percent,
            s.threads,
            s.auto_restart as i64,
            s.ignore_oom as i64,
            now(),
            s.id
        ],
    )?;
    Ok(())
}

pub fn set_server_status(db: &Db, id: i64, status: &str) -> Result<()> {
    let conn = db.lock();
    conn.execute(
        "UPDATE servers SET status=?1, updated_at=?2 WHERE id=?3",
        params![status, now(), id],
    )?;
    Ok(())
}

pub fn set_server_suspended(db: &Db, id: i64, suspended: bool) -> Result<()> {
    let conn = db.lock();
    conn.execute(
        "UPDATE servers SET suspended=?1, updated_at=?2 WHERE id=?3",
        params![suspended as i64, now(), id],
    )?;
    Ok(())
}

pub fn bump_restart_count(db: &Db, id: i64) -> Result<()> {
    let conn = db.lock();
    conn.execute(
        "UPDATE servers SET restart_count=restart_count+1, updated_at=?1 WHERE id=?2",
        params![now(), id],
    )?;
    Ok(())
}

pub fn delete_server(db: &Db, id: i64) -> Result<()> {
    let mut conn=db.lock(); let tx=conn.transaction()?;
    tx.execute("DELETE FROM allocations WHERE server_id=?1",[id])?;
    tx.execute("UPDATE servers SET deleted=1,port=NULL,updated_at=?1 WHERE id=?2",params![now(),id])?;
    tx.commit()?; Ok(())
}

pub fn purge_server(db: &Db, id: i64) -> Result<()> {
    let conn = db.lock();
    conn.execute("DELETE FROM servers WHERE id=?1", [id])?;
    Ok(())
}

// ---------------- Server variables ----------------

pub fn get_server_vars(db: &Db, server_id: i64) -> Result<Vec<(String, String)>> {
    let conn = db.lock();
    let mut stmt = conn.prepare("SELECT key,value FROM server_variables WHERE server_id=?1")?;
    let rows = stmt.query_map([server_id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    let mut out = Vec::new();
    for v in rows {
        out.push(v?);
    }
    Ok(out)
}

pub fn set_server_var(db: &Db, server_id: i64, egg_id: i64, key: &str, value: &str) -> Result<()> {
    let conn = db.lock();
    conn.execute(
        "INSERT INTO server_variables(server_id,egg_id,key,value) VALUES(?1,?2,?3,?4) ON CONFLICT(server_id,key) DO UPDATE SET value=?4",
        params![server_id, egg_id, key, value],
    )?;
    Ok(())
}

pub fn delete_server_vars(db: &Db, server_id: i64) -> Result<()> {
    let conn = db.lock();
    conn.execute("DELETE FROM server_variables WHERE server_id=?1", [server_id])?;
    Ok(())
}

// ---------------- Subusers ----------------

pub fn list_subusers(db: &Db, server_id: i64) -> Result<Vec<(User, Vec<String>)>> {
    let conn = db.lock();
    let mut stmt = conn.prepare(
        "SELECT u.id,u.username,u.email,u.avatar,u.language,u.theme,u.root_admin,u.active,u.twofa_secret,u.about,u.created_at,u.updated_at,s.permissions FROM subusers s JOIN users u ON u.id=s.user_id WHERE s.server_id=?1",
    )?;
    let rows = stmt.query_map([server_id], |r| {
        let perms: Vec<String> = serde_json::from_str(&r.get::<_, String>(12)?).unwrap_or_default();
        Ok((user_from_row(r)?, perms))
    })?;
    let mut out = Vec::new();
    for v in rows {
        out.push(v?);
    }
    Ok(out)
}

pub fn add_subuser(db: &Db, server_id: i64, user_id: i64, perms: &[String]) -> Result<()> {
    let conn = db.lock();
    conn.execute(
        "INSERT OR REPLACE INTO subusers(server_id,user_id,permissions) VALUES(?1,?2,?3)",
        params![server_id, user_id, serde_json::to_string(perms)?],
    )?;
    Ok(())
}

pub fn remove_subuser(db: &Db, server_id: i64, user_id: i64) -> Result<()> {
    let conn = db.lock();
    conn.execute("DELETE FROM subusers WHERE server_id=?1 AND user_id=?2", params![server_id, user_id])?;
    Ok(())
}

pub fn user_has_server_access(db: &Db, user: &User, server_id: i64) -> Result<bool> {
    if user.root_admin {
        return Ok(true);
    }
    let conn = db.lock();
    let mine: i64 = conn.query_row(
        "SELECT COUNT(*) FROM servers WHERE id=?1 AND user_id=?2 AND deleted=0",
        params![server_id, user.id],
        |r| r.get(0),
    )?;
    if mine > 0 {
        return Ok(true);
    }
    let sub: i64 = conn.query_row(
        "SELECT COUNT(*) FROM subusers WHERE server_id=?1 AND user_id=?2",
        params![server_id, user.id],
        |r| r.get(0),
    )?;
    Ok(sub > 0)
}

pub fn user_has_server_permission(db: &Db, user: &User, server_id: i64, permission: &str) -> Result<bool> {
    if user.root_admin { return Ok(true); }
    let conn = db.lock();
    let owner: i64 = conn.query_row("SELECT COUNT(*) FROM servers WHERE id=?1 AND user_id=?2 AND deleted=0", params![server_id,user.id], |r|r.get(0))?;
    if owner > 0 { return Ok(true); }
    let raw: Option<String> = conn.query_row("SELECT permissions FROM subusers WHERE server_id=?1 AND user_id=?2",params![server_id,user.id],|r|r.get(0)).optional()?;
    let Some(raw)=raw else{return Ok(false)};
    let perms:Vec<String>=serde_json::from_str(&raw).unwrap_or_default();
    let category=permission.split('.').next().unwrap_or(permission);
    Ok(perms.iter().any(|p| p=="*"||p==permission||p==category||
        (permission.starts_with("control.")&&matches!(p.as_str(),"start"|"stop"|"restart"|"power"))||
        (permission.starts_with("console.")&&p=="console")||(permission.starts_with("files.")&&p=="files")))
}

// ---------------- Backup ----------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Backup {
    pub id: i64,
    pub uuid: String,
    pub server_id: i64,
    pub name: String,
    pub path: String,
    pub size_bytes: i64,
    pub checksum: String,
    pub format: String,
    pub created_at: String,
}

fn backup_from_row(r: &Row) -> rusqlite::Result<Backup> {
    Ok(Backup {
        id: r.get(0)?,
        uuid: r.get(1)?,
        server_id: r.get(2)?,
        name: r.get(3)?,
        path: r.get(4)?,
        size_bytes: r.get(5)?,
        checksum: r.get(6)?,
        format: r.get(7)?,
        created_at: r.get(8)?,
    })
}

pub fn list_backups(db: &Db, server_id: i64) -> Result<Vec<Backup>> {
    let conn = db.lock();
    let mut stmt = conn.prepare("SELECT id,uuid,server_id,name,path,size_bytes,checksum,format,created_at FROM backups WHERE server_id=?1 ORDER BY created_at DESC")?;
    let rows = stmt.query_map([server_id], backup_from_row)?;
    let mut out = Vec::new();
    for b in rows {
        out.push(b?);
    }
    Ok(out)
}

pub fn get_backup(db: &Db, id: i64) -> Result<Backup> {
    let conn = db.lock();
    conn.query_row(
        "SELECT id,uuid,server_id,name,path,size_bytes,checksum,format,created_at FROM backups WHERE id=?1",
        [id],
        backup_from_row,
    )
    .context("backup not found")
}

pub fn create_backup(db: &Db, uuid: &str, server_id: i64, name: &str, path: &str, size_bytes: i64, checksum: &str, format: &str) -> Result<i64> {
    let conn = db.lock();
    conn.execute(
        "INSERT INTO backups(uuid,server_id,name,path,size_bytes,checksum,format,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
        params![uuid, server_id, name, path, size_bytes, checksum, format, now()],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn delete_backup(db: &Db, id: i64) -> Result<()> {
    let conn = db.lock();
    conn.execute("DELETE FROM backups WHERE id=?1", [id])?;
    Ok(())
}

// ---------------- Database ----------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Database {
    pub id: i64,
    pub server_id: i64,
    pub name: String,
    pub db_type: String,
    pub host: String,
    pub port: i64,
    pub db_name: String,
    pub username: String,
    pub password: String,
    pub max_conns: i64,
    pub created_at: String,
}

fn database_from_row(r: &Row) -> rusqlite::Result<Database> {
    Ok(Database {
        id: r.get(0)?,
        server_id: r.get(1)?,
        name: r.get(2)?,
        db_type: r.get(3)?,
        host: r.get(4)?,
        port: r.get(5)?,
        db_name: r.get(6)?,
        username: r.get(7)?,
        password: r.get(8)?,
        max_conns: r.get(9)?,
        created_at: r.get(10)?,
    })
}

pub fn list_databases(db: &Db, server_id: i64) -> Result<Vec<Database>> {
    let conn = db.lock();
    let mut stmt = conn.prepare("SELECT id,server_id,name,db_type,host,port,db_name,username,password,max_conns,created_at FROM databases WHERE server_id=?1 ORDER BY name")?;
    let rows = stmt.query_map([server_id], database_from_row)?;
    let mut out = Vec::new();
    for d in rows {
        out.push(d?);
    }
    Ok(out)
}

pub fn create_database(db: &Db, server_id: i64, name: &str, db_type: &str, host: &str, port: i64, db_name: &str, username: &str, password: &str, max_conns: i64) -> Result<i64> {
    let conn = db.lock();
    conn.execute(
        "INSERT INTO databases(server_id,name,db_type,host,port,db_name,username,password,max_conns,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
        params![server_id, name, db_type, host, port, db_name, username, password, max_conns, now()],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn delete_database(db: &Db, id: i64) -> Result<()> {
    let conn = db.lock();
    conn.execute("DELETE FROM databases WHERE id=?1", [id])?;
    Ok(())
}

// ---------------- Schedule ----------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduleTask {
    pub id: i64,
    pub schedule_id: i64,
    pub action: String,
    pub payload: String,
    pub sequence: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Schedule {
    pub id: i64,
    pub server_id: i64,
    pub name: String,
    pub cron_expr: String,
    pub enabled: bool,
    pub last_run_at: Option<String>,
    pub next_run_at: Option<String>,
    pub created_at: String,
    #[serde(default)]
    pub tasks: Vec<ScheduleTask>,
}

fn schedule_from_row(r: &Row) -> rusqlite::Result<Schedule> {
    Ok(Schedule {
        id: r.get(0)?,
        server_id: r.get(1)?,
        name: r.get(2)?,
        cron_expr: r.get(3)?,
        enabled: r.get::<_, i64>(4)? != 0,
        last_run_at: r.get(5)?,
        next_run_at: r.get(6)?,
        created_at: r.get(7)?,
        tasks: vec![],
    })
}

pub fn list_schedules(db: &Db, server_id: i64) -> Result<Vec<Schedule>> {
    let conn = db.lock();
    let mut stmt = conn.prepare("SELECT id,server_id,name,cron_expr,enabled,last_run_at,next_run_at,created_at FROM schedules WHERE server_id=?1 ORDER BY id")?;
    let rows = stmt.query_map([server_id], schedule_from_row)?;
    let mut out = Vec::new();
    for s in rows {
        let mut s = s?;
        s.tasks = list_schedule_tasks_conn(&conn, s.id)?;
        out.push(s);
    }
    Ok(out)
}

pub fn get_schedule(db: &Db, id: i64) -> Result<Schedule> {
    let conn = db.lock();
    let mut s = conn
        .query_row(
            "SELECT id,server_id,name,cron_expr,enabled,last_run_at,next_run_at,created_at FROM schedules WHERE id=?1",
            [id],
            schedule_from_row,
        )
        .context("schedule not found")?;
    s.tasks = list_schedule_tasks_conn(&conn, s.id)?;
    Ok(s)
}

fn list_schedule_tasks_conn(conn: &rusqlite::Connection, schedule_id: i64) -> Result<Vec<ScheduleTask>> {
    let mut stmt = conn.prepare("SELECT id,schedule_id,action,payload,sequence FROM schedule_tasks WHERE schedule_id=?1 ORDER BY sequence")?;
    let rows = stmt.query_map([schedule_id], |r| {
        Ok(ScheduleTask {
            id: r.get(0)?,
            schedule_id: r.get(1)?,
            action: r.get(2)?,
            payload: r.get(3)?,
            sequence: r.get(4)?,
        })
    })?;
    let mut out = Vec::new();
    for t in rows {
        out.push(t?);
    }
    Ok(out)
}

pub fn create_schedule(db: &Db, server_id: i64, name: &str, cron_expr: &str, enabled: bool) -> Result<i64> {
    let conn = db.lock();
    conn.execute(
        "INSERT INTO schedules(server_id,name,cron_expr,enabled,created_at) VALUES(?1,?2,?3,?4,?5)",
        params![server_id, name, cron_expr, enabled as i64, now()],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn update_schedule(db: &Db, s: &Schedule) -> Result<()> {
    let conn = db.lock();
    conn.execute(
        "UPDATE schedules SET name=?1, cron_expr=?2, enabled=?3 WHERE id=?4",
        params![s.name, s.cron_expr, s.enabled as i64, s.id],
    )?;
    Ok(())
}

pub fn set_schedule_next(db: &Db, id: i64, next: Option<&str>) -> Result<()> {
    let conn = db.lock();
    conn.execute("UPDATE schedules SET next_run_at=?1 WHERE id=?2", params![next, id])?;
    Ok(())
}

pub fn set_schedule_last(db: &Db, id: i64, last: Option<&str>) -> Result<()> {
    let conn = db.lock();
    conn.execute("UPDATE schedules SET last_run_at=?1 WHERE id=?2", params![last, id])?;
    Ok(())
}

pub fn delete_schedule(db: &Db, id: i64) -> Result<()> {
    let conn = db.lock();
    conn.execute("DELETE FROM schedules WHERE id=?1", [id])?;
    Ok(())
}

pub fn add_schedule_task(db: &Db, schedule_id: i64, action: &str, payload: &str, sequence: i64) -> Result<i64> {
    let conn = db.lock();
    conn.execute(
        "INSERT INTO schedule_tasks(schedule_id,action,payload,sequence) VALUES(?1,?2,?3,?4)",
        params![schedule_id, action, payload, sequence],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn delete_schedule_task(db: &Db, id: i64) -> Result<()> {
    let conn = db.lock();
    conn.execute("DELETE FROM schedule_tasks WHERE id=?1", [id])?;
    Ok(())
}

// ---------------- API key ----------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKey {
    pub id: i64,
    pub user_id: i64,
    pub token: String,
    pub name: String,
    pub created_at: String,
    pub last_used: Option<String>,
    pub scopes: String,
}

fn apikey_from_row(r: &Row) -> rusqlite::Result<ApiKey> {
    Ok(ApiKey {
        id: r.get(0)?,
        user_id: r.get(1)?,
        token: r.get(2)?,
        name: r.get(3)?,
        created_at: r.get(4)?,
        last_used: r.get(5)?,
        scopes: r.get(6)?,
    })
}

pub fn list_api_keys(db: &Db, user_id: i64) -> Result<Vec<ApiKey>> {
    let conn = db.lock();
    let mut stmt = conn.prepare("SELECT id,user_id,token,name,created_at,last_used,scopes FROM api_keys WHERE user_id=?1 ORDER BY id DESC")?;
    let rows = stmt.query_map([user_id], apikey_from_row)?;
    let mut out = Vec::new();
    for k in rows {
        out.push(k?);
    }
    Ok(out)
}

pub fn create_api_key(db: &Db, user_id: i64, token: &str, name: &str, scopes: &str) -> Result<i64> {
    let conn = db.lock();
    conn.execute(
        "INSERT INTO api_keys(user_id,token,name,scopes,created_at) VALUES(?1,?2,?3,?4,?5)",
        params![user_id, token, name, scopes, now()],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn touch_api_key(db: &Db, id: i64) -> Result<()> {
    let conn = db.lock();
    conn.execute("UPDATE api_keys SET last_used=?1 WHERE id=?2", params![now(), id])?;
    Ok(())
}

pub fn get_api_key_by_token(db: &Db, token_hash: &str) -> Result<Option<ApiKey>> {
    let conn=db.lock();
    conn.query_row("SELECT id,user_id,token,name,created_at,last_used,scopes FROM api_keys WHERE token=?1",[token_hash],apikey_from_row).optional().map_err(Into::into)
}

pub fn delete_api_key(db: &Db, id: i64) -> Result<()> {
    let conn = db.lock();
    conn.execute("DELETE FROM api_keys WHERE id=?1", [id])?;
    Ok(())
}

// ---------------- Website ----------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Website {
    pub id: i64,
    pub server_id: i64,
    pub domain: String,
    pub root_dir: String,
    pub port: Option<i64>,
    pub proxy_type: String,
    pub ssl: bool,
    pub enabled: bool,
    pub created_at: String,
}

fn website_from_row(r: &Row) -> rusqlite::Result<Website> {
    Ok(Website {
        id: r.get(0)?,
        server_id: r.get(1)?,
        domain: r.get(2)?,
        root_dir: r.get(3)?,
        port: r.get(4)?,
        proxy_type: r.get(5)?,
        ssl: r.get::<_, i64>(6)? != 0,
        enabled: r.get::<_, i64>(7)? != 0,
        created_at: r.get(8)?,
    })
}

pub fn list_websites(db: &Db, server_id: i64) -> Result<Vec<Website>> {
    let conn = db.lock();
    let mut stmt = conn.prepare("SELECT id,server_id,domain,root_dir,port,proxy_type,ssl,enabled,created_at FROM websites WHERE server_id=?1 ORDER BY id")?;
    let rows = stmt.query_map([server_id], website_from_row)?;
    let mut out = Vec::new();
    for w in rows {
        out.push(w?);
    }
    Ok(out)
}

pub fn create_website(db: &Db, server_id: i64, domain: &str, root_dir: &str, proxy_type: &str, ssl: bool) -> Result<i64> {
    let conn = db.lock();
    conn.execute(
        "INSERT INTO websites(server_id,domain,root_dir,proxy_type,ssl,created_at) VALUES(?1,?2,?3,?4,?5,?6)",
        params![server_id, domain, root_dir, proxy_type, ssl as i64, now()],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn update_website(db: &Db, w: &Website) -> Result<()> {
    let conn = db.lock();
    conn.execute(
        "UPDATE websites SET domain=?1, root_dir=?2, port=?3, proxy_type=?4, ssl=?5, enabled=?6 WHERE id=?7",
        params![w.domain, w.root_dir, w.port, w.proxy_type, w.ssl as i64, w.enabled as i64, w.id],
    )?;
    Ok(())
}

pub fn delete_website(db: &Db, id: i64) -> Result<()> {
    let conn = db.lock();
    conn.execute("DELETE FROM websites WHERE id=?1", [id])?;
    Ok(())
}

// ---------------- Audit log ----------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditLog {
    pub id: i64,
    pub user_id: Option<i64>,
    pub action: String,
    pub target: String,
    pub ip: String,
    pub details: String,
    pub created_at: String,
}

pub fn list_audit_logs(db: &Db, limit: i64) -> Result<Vec<AuditLog>> {
    let conn = db.lock();
    let mut stmt = conn.prepare("SELECT id,user_id,action,target,ip,details,created_at FROM audit_logs ORDER BY id DESC LIMIT ?1")?;
    let rows = stmt.query_map([limit], |r| {
        Ok(AuditLog {
            id: r.get(0)?,
            user_id: r.get(1)?,
            action: r.get(2)?,
            target: r.get(3)?,
            ip: r.get(4)?,
            details: r.get(5)?,
            created_at: r.get(6)?,
        })
    })?;
    let mut out = Vec::new();
    for a in rows {
        out.push(a?);
    }
    Ok(out)
}

pub fn audit(db: &Db, user_id: Option<i64>, action: &str, target: &str, ip: &str, details: &str) -> Result<()> {
    let conn = db.lock();
    conn.execute(
        "INSERT INTO audit_logs(user_id,action,target,ip,details,created_at) VALUES(?1,?2,?3,?4,?5,?6)",
        params![user_id, action, target, ip, details, now()],
    )?;
    Ok(())
}

// ---------------- Settings ----------------

pub fn get_setting(db: &Db, key: &str) -> Result<Option<String>> {
    let conn = db.lock();
    let v = conn
        .query_row("SELECT value FROM settings WHERE key=?1", [key], |r| r.get::<_, String>(0))
        .optional()?;
    Ok(v)
}

pub fn set_setting(db: &Db, key: &str, value: &str) -> Result<()> {
    let conn = db.lock();
    conn.execute(
        "INSERT INTO settings(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=?2",
        params![key, value],
    )?;
    Ok(())
}

pub fn all_settings(db: &Db) -> Result<Vec<(String, String)>> {
    let conn = db.lock();
    let mut stmt = conn.prepare("SELECT key,value FROM settings")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    let mut out = Vec::new();
    for s in rows {
        out.push(s?);
    }
    Ok(out)
}

// ---------------- Allocations ----------------

pub fn allocate_port(db: &Db, server_id: i64, port: i64) -> Result<()> {
    let mut conn = db.lock();
    let tx = conn.transaction()?;
    let node: String = tx.query_row("SELECT node FROM servers WHERE id=?1", [server_id], |r| r.get(0))?;
    tx.execute("UPDATE servers SET port=?1,updated_at=?2 WHERE id=?3", params![port, now(), server_id])?;
    tx.execute("INSERT INTO allocations(server_id,port,assigned_at,node) VALUES(?1,?2,?3,?4)", params![server_id, port, now(), node])?;
    tx.commit()?;
    Ok(())
}

pub fn free_ports(db: &Db, server_id: i64) -> Result<()> {
    let mut conn = db.lock();
    let tx = conn.transaction()?;
    tx.execute("DELETE FROM allocations WHERE server_id=?1", [server_id])?;
    tx.execute("UPDATE servers SET port=NULL,updated_at=?1 WHERE id=?2", params![now(), server_id])?;
    tx.commit()?;
    Ok(())
}

pub fn ports_for_server(db: &Db, server_id: i64) -> Result<Vec<i64>> {
    let conn = db.lock();
    let mut stmt = conn.prepare("SELECT port FROM allocations WHERE server_id=?1 ORDER BY port")?;
    let rows = stmt.query_map([server_id], |r| r.get::<_, i64>(0))?;
    let mut out = Vec::new();
    for p in rows {
        out.push(p?);
    }
    Ok(out)
}

// ---------------- Rate limits ----------------

pub fn bump_rate_limit(db: &Db, key: &str, window_start: i64) -> Result<i64> {
    let conn = db.lock();
    conn.execute(
        "INSERT INTO rate_limits(key,window_start,count) VALUES(?1,?2,1) ON CONFLICT(key) DO UPDATE SET count=CASE WHEN rate_limits.window_start=?2 THEN count+1 ELSE 1 END, window_start=?2",
        params![key, window_start],
    )?;
    let c: i64 = conn.query_row("SELECT count FROM rate_limits WHERE key=?1", [key], |r| r.get(0))?;
    Ok(c)
}

pub fn reset_rate_limits(db: &Db) -> Result<()> {
    let conn = db.lock();
    conn.execute("DELETE FROM rate_limits", [])?;
    Ok(())
}
