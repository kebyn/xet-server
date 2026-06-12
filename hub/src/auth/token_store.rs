use rusqlite::{Connection, params};
use sha2::{Sha256, Digest};
use std::sync::Mutex;

/// Token information returned after validation
#[derive(Debug, Clone)]
pub struct TokenInfo {
    pub user_id: String,
    pub username: String,
    pub token_name: String,
    pub scope: String, // "read" or "write"
}

/// SQLite-based token store with SHA256 token hashing
pub struct TokenStore {
    conn: Mutex<Connection>,
}

impl TokenStore {
    /// Create a new TokenStore with the given database path
    pub fn new(db_path: &str) -> Result<Self, rusqlite::Error> {
        let conn = Connection::open(db_path)?;
        Self::init_tables(&conn)?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    /// Create an in-memory TokenStore.
    ///
    /// **Warning:** This is intended for testing only. In-memory stores do not persist
    /// data across restarts and should not be used in production deployments.
    pub fn in_memory() -> Result<Self, rusqlite::Error> {
        let conn = Connection::open_in_memory()?;
        Self::init_tables(&conn)?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    fn init_tables(conn: &Connection) -> Result<(), rusqlite::Error> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS users (
                user_id TEXT PRIMARY KEY,
                username TEXT NOT NULL UNIQUE,
                created_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS tokens (
                token_hash TEXT PRIMARY KEY,
                user_id TEXT NOT NULL,
                name TEXT NOT NULL,
                scope TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                expires_at INTEGER,
                revoked_at INTEGER,
                FOREIGN KEY (user_id) REFERENCES users(user_id)
            );
            CREATE INDEX IF NOT EXISTS idx_tokens_user ON tokens(user_id);"
        )?;
        Ok(())
    }

    /// Create a new user and token (admin setup). Returns the plaintext hf_ token.
    pub fn create_token(&self, username: &str, token_name: &str, scope: &str) -> Result<String, rusqlite::Error> {
        let token = format!("hf_{}", uuid::Uuid::new_v4().to_string().replace('-', ""));
        let token_hash = Self::hash_token(&token);
        let now = now_secs();
        let user_id = format!("user_{}", &token_hash[..16]);

        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO users (user_id, username, created_at) VALUES (?1, ?2, ?3)",
            params![user_id, username, now as i64],
        )?;
        conn.execute(
            "INSERT INTO tokens (token_hash, user_id, name, scope, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![token_hash, user_id, token_name, scope, now as i64],
        )?;
        Ok(token)
    }

    /// Create a token for an existing user. Returns the plaintext hf_ token.
    pub fn create_token_for_user(&self, user_id: &str, token_name: &str, scope: &str) -> Result<String, rusqlite::Error> {
        let token = format!("hf_{}", uuid::Uuid::new_v4().to_string().replace('-', ""));
        let token_hash = Self::hash_token(&token);
        let now = now_secs();

        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO tokens (token_hash, user_id, name, scope, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![token_hash, user_id, token_name, scope, now as i64],
        )?;
        Ok(token)
    }

    /// Validate a token. Returns None if invalid/expired/revoked.
    pub fn validate_token(&self, token: &str) -> Result<Option<TokenInfo>, rusqlite::Error> {
        let token_hash = Self::hash_token(token);
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT u.user_id, u.username, t.name, t.scope, t.expires_at, t.revoked_at
             FROM tokens t JOIN users u ON t.user_id = u.user_id
             WHERE t.token_hash = ?1"
        )?;
        let result = stmt.query_row(params![token_hash], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<i64>>(4)?,
                row.get::<_, Option<i64>>(5)?,
            ))
        });
        match result {
            Ok((user_id, username, name, scope, expires_at, revoked_at)) => {
                if revoked_at.is_some() {
                    return Ok(None);
                }
                if let Some(exp) = expires_at
                    && (exp as u64) < now_secs() {
                        return Ok(None);
                    }
                Ok(Some(TokenInfo {
                    user_id,
                    username,
                    token_name: name,
                    scope
                }))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Revoke a token
    pub fn revoke_token(&self, token: &str) -> Result<bool, rusqlite::Error> {
        let token_hash = Self::hash_token(token);
        let now = now_secs();
        let conn = self.conn.lock().unwrap();
        let rows = conn.execute(
            "UPDATE tokens SET revoked_at = ?1 WHERE token_hash = ?2 AND revoked_at IS NULL",
            params![now as i64, token_hash],
        )?;
        Ok(rows > 0)
    }

    /// Get all tokens for a user (without returning the actual tokens, just metadata)
    pub fn list_tokens_for_user(&self, user_id: &str) -> Result<Vec<TokenMetadata>, rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT name, scope, created_at, expires_at, revoked_at
             FROM tokens WHERE user_id = ?1 ORDER BY created_at DESC"
        )?;
        let tokens = stmt.query_map(params![user_id], |row| {
            Ok(TokenMetadata {
                name: row.get(0)?,
                scope: row.get(1)?,
                created_at: row.get::<_, i64>(2)? as u64,
                expires_at: row.get::<_, Option<i64>>(3)?.map(|v| v as u64),
                revoked_at: row.get::<_, Option<i64>>(4)?.map(|v| v as u64),
            })
        })?.collect::<Result<Vec<_>, _>>()?;
        Ok(tokens)
    }

    /// Hash a token using SHA256
    fn hash_token(token: &str) -> String {
        hex::encode(Sha256::digest(token.as_bytes()))
    }

    /// Set expiration on a token (for testing)
    pub fn set_token_expiration(&self, token: &str, expires_at: u64) -> Result<(), rusqlite::Error> {
        let token_hash = Self::hash_token(token);
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE tokens SET expires_at = ?1 WHERE token_hash = ?2",
            params![expires_at as i64, token_hash],
        )?;
        Ok(())
    }
}

/// Token metadata (without actual token)
#[derive(Debug, Clone)]
pub struct TokenMetadata {
    pub name: String,
    pub scope: String,
    pub created_at: u64,
    pub expires_at: Option<u64>,
    pub revoked_at: Option<u64>,
}

/// Get current Unix timestamp in seconds
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hash_token() {
        let hash1 = TokenStore::hash_token("hf_test123");
        let hash2 = TokenStore::hash_token("hf_test123");
        assert_eq!(hash1, hash2, "Same token should produce same hash");

        let hash3 = TokenStore::hash_token("hf_test456");
        assert_ne!(hash1, hash3, "Different tokens should produce different hashes");
    }

    #[test]
    fn test_create_and_validate_token() {
        let store = TokenStore::in_memory().unwrap();

        let token = store.create_token("testuser", "test-token", "read").unwrap();
        assert!(token.starts_with("hf_"), "Token should start with hf_");

        let info = store.validate_token(&token).unwrap();
        assert!(info.is_some());

        let info = info.unwrap();
        assert_eq!(info.username, "testuser");
        assert_eq!(info.token_name, "test-token");
        assert_eq!(info.scope, "read");
    }

    #[test]
    fn test_invalid_token() {
        let store = TokenStore::in_memory().unwrap();

        let info = store.validate_token("hf_invalid").unwrap();
        assert!(info.is_none(), "Invalid token should return None");
    }

    #[test]
    fn test_expired_token() {
        let store = TokenStore::in_memory().unwrap();

        let token = store.create_token("testuser", "test-token", "read").unwrap();

        // Set expiration in the past
        let past_time = now_secs() - 3600; // 1 hour ago
        store.set_token_expiration(&token, past_time).unwrap();

        let info = store.validate_token(&token).unwrap();
        assert!(info.is_none(), "Expired token should return None");
    }

    #[test]
    fn test_revoked_token() {
        let store = TokenStore::in_memory().unwrap();

        let token = store.create_token("testuser", "test-token", "read").unwrap();

        // Verify token works
        let info = store.validate_token(&token).unwrap();
        assert!(info.is_some());

        // Revoke the token
        let revoked = store.revoke_token(&token).unwrap();
        assert!(revoked, "revoke_token should return true for existing token");

        // Verify token no longer works
        let info = store.validate_token(&token).unwrap();
        assert!(info.is_none(), "Revoked token should return None");
    }

    #[test]
    fn test_write_scope() {
        let store = TokenStore::in_memory().unwrap();

        let token = store.create_token("testuser", "write-token", "write").unwrap();

        let info = store.validate_token(&token).unwrap().unwrap();
        assert_eq!(info.scope, "write");
    }
}