use rusqlite::{Connection, params};
use std::sync::Mutex;

pub struct Db {
    conn: Mutex<Connection>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Session {
    pub id: String,
    pub name: Option<String>,
    pub created_at: String,
    pub expires_at: String,
    pub has_gpx: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Ping {
    pub ts: String,
    pub lat: f64,
    pub lon: f64,
    pub ele: Option<f64>,
    pub speed: Option<f64>,
    pub heading: Option<f64>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GpxPoint {
    pub lat: f64,
    pub lon: f64,
    pub ele: Option<f64>,
    pub dist_km: f64,
}

impl Db {
    pub fn open(path: &str) -> anyhow::Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    pub fn migrate(&self) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS sessions (
                id          TEXT PRIMARY KEY,
                name        TEXT,
                created_at  TEXT NOT NULL DEFAULT (datetime('now')),
                expires_at  TEXT NOT NULL,
                gpx_json    TEXT
            );
            CREATE TABLE IF NOT EXISTS pings (
                session_id  TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                ts          TEXT NOT NULL DEFAULT (datetime('now')),
                lat         REAL NOT NULL,
                lon         REAL NOT NULL,
                ele         REAL,
                speed       REAL,
                heading     REAL
            );
            CREATE INDEX IF NOT EXISTS idx_pings_session ON pings(session_id, ts);"
        )?;
        Ok(())
    }

    pub fn create_session(&self, id: &str, name: Option<&str>, hours: u32) -> anyhow::Result<Session> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO sessions (id, name, expires_at) VALUES (?1, ?2, datetime('now', ?3))",
            params![id, name, format!("+{hours} hours")],
        )?;
        let session = conn.query_row(
            "SELECT id, name, created_at, expires_at, gpx_json IS NOT NULL FROM sessions WHERE id = ?1",
            params![id],
            |row| Ok(Session {
                id: row.get(0)?,
                name: row.get(1)?,
                created_at: row.get(2)?,
                expires_at: row.get(3)?,
                has_gpx: row.get(4)?,
            }),
        )?;
        Ok(session)
    }

    pub fn get_session(&self, id: &str) -> anyhow::Result<Option<Session>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, name, created_at, expires_at, gpx_json IS NOT NULL FROM sessions WHERE id = ?1 AND expires_at > datetime('now')"
        )?;
        let result = stmt.query_row(params![id], |row| Ok(Session {
            id: row.get(0)?,
            name: row.get(1)?,
            created_at: row.get(2)?,
            expires_at: row.get(3)?,
            has_gpx: row.get(4)?,
        }));
        match result {
            Ok(s) => Ok(Some(s)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn insert_ping(&self, session_id: &str, ping: &Ping) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO pings (session_id, ts, lat, lon, ele, speed, heading) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![session_id, ping.ts, ping.lat, ping.lon, ping.ele, ping.speed, ping.heading],
        )?;
        Ok(())
    }

    pub fn get_pings(&self, session_id: &str) -> anyhow::Result<Vec<Ping>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT ts, lat, lon, ele, speed, heading FROM pings WHERE session_id = ?1 ORDER BY ts"
        )?;
        let pings = stmt.query_map(params![session_id], |row| {
            Ok(Ping {
                ts: row.get(0)?,
                lat: row.get(1)?,
                lon: row.get(2)?,
                ele: row.get(3)?,
                speed: row.get(4)?,
                heading: row.get(5)?,
            })
        })?.collect::<Result<Vec<_>, _>>()?;
        Ok(pings)
    }

    pub fn set_gpx(&self, session_id: &str, gpx_json: &str) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE sessions SET gpx_json = ?2 WHERE id = ?1",
            params![session_id, gpx_json],
        )?;
        Ok(())
    }

    pub fn get_gpx(&self, session_id: &str) -> anyhow::Result<Option<Vec<GpxPoint>>> {
        let conn = self.conn.lock().unwrap();
        let result: Result<String, _> = conn.query_row(
            "SELECT gpx_json FROM sessions WHERE id = ?1 AND gpx_json IS NOT NULL",
            params![session_id],
            |row| row.get(0),
        );
        match result {
            Ok(json) => Ok(Some(serde_json::from_str(&json)?)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn purge_expired(&self) -> anyhow::Result<usize> {
        let conn = self.conn.lock().unwrap();
        let deleted = conn.execute(
            "DELETE FROM sessions WHERE expires_at < datetime('now')",
            [],
        )?;
        if deleted > 0 {
            tracing::info!("purged {deleted} expired sessions");
        }
        Ok(deleted)
    }
}
