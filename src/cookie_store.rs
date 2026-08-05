use rusqlite::{Connection, OptionalExtension, params};
use shell_protocol::wire;
use std::{
    error::Error,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

const WINDOWS_EPOCH_MICROSECONDS: i64 = 11_644_473_600_000_000;

type StoreResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

#[derive(Clone, Debug)]
pub struct CanonicalCookie {
    pub cookie: wire::Cookie,
    pub deleted: bool,
    pub modified_at: i64,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct CookieIdentity {
    name: String,
    domain: String,
    path: String,
    partition_site: String,
    partition_ancestor: bool,
    partition_opaque: bool,
}

pub struct CookieStore {
    connection: Connection,
}

impl CookieIdentity {
    pub fn from_cookie(cookie: &wire::Cookie) -> StoreResult<Self> {
        let key = cookie.key.as_ref().ok_or("cookie key is required")?;
        if key.domain.is_empty() || key.path.is_empty() {
            return Err("cookie domain and path are required".into());
        }
        let partition = key.partition_key.as_ref();
        Ok(Self {
            name: key.name.clone(),
            domain: key.domain.clone(),
            path: key.path.clone(),
            partition_site: partition
                .map(|partition| partition.top_level_site.clone())
                .unwrap_or_default(),
            partition_ancestor: partition
                .is_some_and(|partition| partition.has_cross_site_ancestor),
            partition_opaque: partition.is_some_and(|partition| partition.opaque),
        })
    }

    pub fn is_opaque(&self) -> bool {
        self.partition_opaque
    }
}

impl CookieStore {
    pub fn open(profile_data: &Path) -> StoreResult<Self> {
        std::fs::create_dir_all(profile_data)?;
        let connection = Connection::open(profile_data.join("cookies.sqlite3"))?;
        connection.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=FULL;
             CREATE TABLE IF NOT EXISTS cookies (
               name TEXT NOT NULL,
               domain TEXT NOT NULL,
               path TEXT NOT NULL,
               partition_site TEXT NOT NULL,
               partition_ancestor INTEGER NOT NULL,
               partition_opaque INTEGER NOT NULL,
               value TEXT NOT NULL,
               secure INTEGER NOT NULL,
               http_only INTEGER NOT NULL,
               creation INTEGER NOT NULL,
               last_access INTEGER NOT NULL,
               expires INTEGER,
               same_site INTEGER NOT NULL,
               priority INTEGER NOT NULL,
               last_update INTEGER NOT NULL,
               deleted INTEGER NOT NULL,
               modified_at INTEGER NOT NULL,
               PRIMARY KEY (
                 name, domain, path, partition_site,
                 partition_ancestor, partition_opaque
               )
             ) WITHOUT ROWID;",
        )?;
        Ok(Self { connection })
    }

    pub fn merge_snapshot(&self, cookie: &wire::Cookie) -> StoreResult<()> {
        let identity = CookieIdentity::from_cookie(cookie)?;
        if identity.is_opaque() {
            return Ok(());
        }
        let incoming_modified = snapshot_modified_at(cookie);
        let existing = self.get(&identity)?;
        if let Some(existing) = existing {
            if cookie_contents_equal(&existing.cookie, cookie) {
                return Ok(());
            }
            if existing.modified_at >= incoming_modified {
                return Ok(());
            }
        }
        self.write(cookie, false, incoming_modified)
    }

    pub fn apply_live_change(&self, cookie: &wire::Cookie) -> StoreResult<bool> {
        let identity = CookieIdentity::from_cookie(cookie)?;
        if identity.is_opaque() {
            return Ok(false);
        }
        if let Some(existing) = self.get(&identity)?
            && !existing.deleted
            && cookie_contents_equal(&existing.cookie, cookie)
        {
            return Ok(false);
        }
        self.write(
            cookie,
            false,
            now_windows_microseconds().max(cookie.last_update),
        )?;
        Ok(true)
    }

    pub fn apply_deletion(&self, cookie: &wire::Cookie) -> StoreResult<bool> {
        let identity = CookieIdentity::from_cookie(cookie)?;
        if identity.is_opaque() {
            return Ok(false);
        }
        if self
            .get(&identity)?
            .is_some_and(|existing| existing.deleted)
        {
            return Ok(false);
        }
        self.write(
            cookie,
            true,
            now_windows_microseconds().max(cookie.last_update),
        )?;
        Ok(true)
    }

    pub fn all(&self) -> StoreResult<Vec<CanonicalCookie>> {
        let mut statement = self.connection.prepare(
            "SELECT name, domain, path, partition_site, partition_ancestor,
                    partition_opaque, value, secure, http_only, creation,
                    last_access, expires, same_site, priority, last_update,
                    deleted, modified_at
               FROM cookies",
        )?;
        let rows = statement.query_map([], row_cookie)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    fn get(&self, identity: &CookieIdentity) -> StoreResult<Option<CanonicalCookie>> {
        Ok(self
            .connection
            .query_row(
                "SELECT name, domain, path, partition_site, partition_ancestor,
                        partition_opaque, value, secure, http_only, creation,
                        last_access, expires, same_site, priority, last_update,
                        deleted, modified_at
                   FROM cookies
                  WHERE name = ?1 AND domain = ?2 AND path = ?3
                    AND partition_site = ?4 AND partition_ancestor = ?5
                    AND partition_opaque = ?6",
                params![
                    identity.name,
                    identity.domain,
                    identity.path,
                    identity.partition_site,
                    identity.partition_ancestor,
                    identity.partition_opaque,
                ],
                row_cookie,
            )
            .optional()?)
    }

    fn write(&self, cookie: &wire::Cookie, deleted: bool, modified_at: i64) -> StoreResult<()> {
        let identity = CookieIdentity::from_cookie(cookie)?;
        self.connection.execute(
            "INSERT INTO cookies (
               name, domain, path, partition_site, partition_ancestor,
               partition_opaque, value, secure, http_only, creation,
               last_access, expires, same_site, priority, last_update,
               deleted, modified_at
             ) VALUES (
               ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10,
               ?11, ?12, ?13, ?14, ?15, ?16, ?17
             )
             ON CONFLICT (
               name, domain, path, partition_site,
               partition_ancestor, partition_opaque
             ) DO UPDATE SET
               value = excluded.value,
               secure = excluded.secure,
               http_only = excluded.http_only,
               creation = excluded.creation,
               last_access = excluded.last_access,
               expires = excluded.expires,
               same_site = excluded.same_site,
               priority = excluded.priority,
               last_update = excluded.last_update,
               deleted = excluded.deleted,
               modified_at = excluded.modified_at",
            params![
                identity.name,
                identity.domain,
                identity.path,
                identity.partition_site,
                identity.partition_ancestor,
                identity.partition_opaque,
                cookie.value,
                cookie.secure,
                cookie.http_only,
                cookie.creation,
                cookie.last_access,
                cookie.expires,
                cookie.same_site,
                cookie.priority,
                cookie.last_update,
                deleted,
                modified_at,
            ],
        )?;
        Ok(())
    }
}

fn row_cookie(row: &rusqlite::Row<'_>) -> rusqlite::Result<CanonicalCookie> {
    let partition_site = row.get::<_, String>(3)?;
    let partition_ancestor = row.get::<_, bool>(4)?;
    let partition_opaque = row.get::<_, bool>(5)?;
    let partition_key =
        (!partition_site.is_empty() || partition_opaque).then_some(wire::CookiePartitionKey {
            top_level_site: partition_site,
            has_cross_site_ancestor: partition_ancestor,
            opaque: partition_opaque,
        });
    Ok(CanonicalCookie {
        cookie: wire::Cookie {
            key: Some(wire::CookieKey {
                name: row.get(0)?,
                domain: row.get(1)?,
                path: row.get(2)?,
                partition_key,
            }),
            value: row.get(6)?,
            secure: row.get(7)?,
            http_only: row.get(8)?,
            creation: row.get(9)?,
            last_access: row.get(10)?,
            expires: row.get(11)?,
            same_site: row.get(12)?,
            priority: row.get(13)?,
            last_update: row.get(14)?,
        },
        deleted: row.get(15)?,
        modified_at: row.get(16)?,
    })
}

pub fn cookie_contents_equal(left: &wire::Cookie, right: &wire::Cookie) -> bool {
    left.key == right.key
        && left.value == right.value
        && left.secure == right.secure
        && left.http_only == right.http_only
        && left.expires == right.expires
        && left.same_site == right.same_site
        && left.priority == right.priority
}

fn snapshot_modified_at(cookie: &wire::Cookie) -> i64 {
    cookie.last_update.max(cookie.creation)
}

fn now_windows_microseconds() -> i64 {
    let unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros();
    i64::try_from(unix)
        .unwrap_or(i64::MAX)
        .saturating_add(WINDOWS_EPOCH_MICROSECONDS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn cookie(value: &str, last_update: i64) -> wire::Cookie {
        wire::Cookie {
            key: Some(wire::CookieKey {
                name: "session".to_owned(),
                domain: ".example.com".to_owned(),
                path: "/".to_owned(),
                partition_key: Some(wire::CookiePartitionKey {
                    top_level_site: "https://shop.test".to_owned(),
                    has_cross_site_ancestor: true,
                    opaque: false,
                }),
            }),
            value: value.to_owned(),
            secure: true,
            http_only: true,
            creation: 10,
            last_access: 20,
            expires: Some(30),
            same_site: wire::CookieSameSite::Lax as i32,
            priority: wire::CookiePriority::Medium as i32,
            last_update,
        }
    }

    #[test]
    fn a_newer_snapshot_wins_and_a_tombstone_blocks_stale_data() {
        let directory = tempdir().unwrap();
        let store = CookieStore::open(directory.path()).unwrap();
        store.merge_snapshot(&cookie("old", 100)).unwrap();
        store.merge_snapshot(&cookie("new", 200)).unwrap();
        assert_eq!(store.all().unwrap()[0].cookie.value, "new");
        assert!(store.apply_deletion(&cookie("new", 200)).unwrap());
        store.merge_snapshot(&cookie("stale", 300)).unwrap();
        let entry = store.all().unwrap().pop().unwrap();
        assert!(entry.deleted);
        assert_eq!(entry.cookie.value, "new");
    }

    #[test]
    fn matching_content_suppresses_engine_echoes() {
        let directory = tempdir().unwrap();
        let store = CookieStore::open(directory.path()).unwrap();
        assert!(store.apply_live_change(&cookie("value", 100)).unwrap());
        let mut echoed = cookie("value", 500);
        echoed.creation = 400;
        assert!(!store.apply_live_change(&echoed).unwrap());
    }
}
