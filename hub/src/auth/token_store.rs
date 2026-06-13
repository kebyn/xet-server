//! Async SQLite-based token store with connection pooling
//!
//! Migrated from rusqlite to sqlx for true async database operations.
//! This prevents blocking the async runtime on database I/O.

use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};
use sqlx::FromRow;
use sha2::{Sha256, Digest};

/// Token information returned after validation
#[derive(Debug, Clone)]
pub struct TokenInfo {
    pub user_id: String,
    pub username: String,
    pub token_name: String,
    pub scope: String, // "read" or "write"
}

/// Internal row type for database queries
#[derive(Debug, FromRow)]
struct TokenRow {
    user_id: String,
    username: String,
    name: String,
    scope: String,
    expires_at: Option<i64>,
    revoked_at: Option<i64>,
}

/// Async SQLite-based token store with connection pooling
pub struct TokenStore {
    pool: SqlitePool,
    /// M4 fix: Server-side salt for token hashing.
    /// Prevents offline dictionary attacks if database is compromised.
    /// Tokens are already high-entropy (UUID), but salt adds defense-in-depth.
    hash_salt: String,
}

impl TokenStore {
    /// Create a new TokenStore with the given database path
    pub async fn new(db_path: &str, pool_size: u32) -> Result<Self, sqlx::Error> {
        let pool = SqlitePoolOptions::new()
            .max_connections(pool_size)
            .min_connections(1)
            .acquire_timeout(std::time::Duration::from_secs(5))
            .connect(db_path)
            .await?;

        Self::init_tables(&pool).await?;

        // C1 fix: Use environment variable if set, otherwise persist generated salt to database.
        // This ensures tokens survive restarts even without explicit configuration.
        let hash_salt = match std::env::var("HUB_TOKEN_HASH_SALT") {
            Ok(salt) => {
                tracing::info!("Using token hash salt from HUB_TOKEN_HASH_SALT environment variable");
                salt
            }
            Err(_) => {
                // No env var - get or generate persistent salt from database
                Self::get_or_generate_hash_salt(&pool).await?
            }
        };

        Ok(Self { pool, hash_salt })
    }

    /// Create an in-memory TokenStore.
    ///
    /// **Warning:** This is intended for testing only. In-memory stores do not persist
    /// data across restarts and should not be used in production deployments.
    pub async fn in_memory() -> Result<Self, sqlx::Error> {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await?;

        Self::init_tables(&pool).await?;

        // For in-memory stores (testing), use a fixed salt for consistency within the test.
        // In-memory databases don't persist across restarts anyway, so persistence doesn't matter.
        let hash_salt = std::env::var("HUB_TOKEN_HASH_SALT")
            .unwrap_or_else(|_| "xet-hub-test-salt".to_string());

        Ok(Self { pool, hash_salt })
    }

    async fn init_tables(pool: &SqlitePool) -> Result<(), sqlx::Error> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS users (
                user_id TEXT PRIMARY KEY,
                username TEXT NOT NULL UNIQUE,
                created_at INTEGER NOT NULL
            )"
        )
        .execute(pool)
        .await?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS tokens (
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
        )
        .execute(pool)
        .await?;

        // C1 fix: Create config table for persisting critical configuration
        // This ensures hash salt survives restarts even without env var
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS _config (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL,
                created_at INTEGER NOT NULL
            )"
        )
        .execute(pool)
        .await?;

        Ok(())
    }

    /// Get or generate persistent hash salt from database.
    /// C1 fix: Ensures token hashes remain consistent across restarts.
    async fn get_or_generate_hash_salt(pool: &SqlitePool) -> Result<String, sqlx::Error> {
        const SALT_KEY: &str = "token_hash_salt";

        // Try to load existing salt from database
        let result: Option<(String,)> = sqlx::query_as(
            "SELECT value FROM _config WHERE key = ?1"
        )
        .bind(SALT_KEY)
        .fetch_optional(pool)
        .await?;

        if let Some((salt,)) = result {
            tracing::info!("Loaded persistent token hash salt from database");
            return Ok(salt);
        }

        // No salt found - generate and persist a new one
        let new_salt = uuid::Uuid::new_v4().to_string();
        let now = now_secs() as i64;

        sqlx::query(
            "INSERT INTO _config (key, value, created_at) VALUES (?1, ?2, ?3)"
        )
        .bind(SALT_KEY)
        .bind(&new_salt)
        .bind(now)
        .execute(pool)
        .await?;

        tracing::warn!(
            "Generated and persisted new token hash salt. \
            This salt will be used for all future restarts. \
            For multi-instance deployments, set HUB_TOKEN_HASH_SALT environment variable."
        );

        Ok(new_salt)
    }

    /// Create a new user and token (admin setup). Returns the plaintext hf_ token.
    pub async fn create_token(
        &self,
        username: &str,
        token_name: &str,
        scope: &str,
    ) -> Result<String, sqlx::Error> {
        let token = format!("hf_{}", uuid::Uuid::new_v4().to_string().replace('-', ""));
        let token_hash = self.hash_token(&token);
        let now = now_secs() as i64;
        let user_id = format!("user_{}", &token_hash[..16]);

        let mut tx = self.pool.begin().await?;

        sqlx::query(
            "INSERT OR IGNORE INTO users (user_id, username, created_at) VALUES (?1, ?2, ?3)"
        )
        .bind(&user_id)
        .bind(username)
        .bind(now)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "INSERT INTO tokens (token_hash, user_id, name, scope, created_at) VALUES (?1, ?2, ?3, ?4, ?5)"
        )
        .bind(&token_hash)
        .bind(&user_id)
        .bind(token_name)
        .bind(scope)
        .bind(now)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;

        Ok(token)
    }

    /// Create a token for an existing user. Returns the plaintext hf_ token.
    pub async fn create_token_for_user(
        &self,
        user_id: &str,
        token_name: &str,
        scope: &str,
    ) -> Result<String, sqlx::Error> {
        let token = format!("hf_{}", uuid::Uuid::new_v4().to_string().replace('-', ""));
        let token_hash = self.hash_token(&token);
        let now = now_secs() as i64;

        sqlx::query(
            "INSERT INTO tokens (token_hash, user_id, name, scope, created_at) VALUES (?1, ?2, ?3, ?4, ?5)"
        )
        .bind(&token_hash)
        .bind(user_id)
        .bind(token_name)
        .bind(scope)
        .bind(now)
        .execute(&self.pool)
        .await?;

        Ok(token)
    }

    /// Validate a token. Returns None if invalid/expired/revoked.
    pub async fn validate_token(&self, token: &str) -> Result<Option<TokenInfo>, sqlx::Error> {
        let token_hash = self.hash_token(token);
        let now = now_secs() as i64;

        let result: Option<TokenRow> = sqlx::query_as(
            "SELECT u.user_id, u.username, t.name, t.scope, t.expires_at, t.revoked_at
             FROM tokens t JOIN users u ON t.user_id = u.user_id
             WHERE t.token_hash = ?1"
        )
        .bind(token_hash)
        .fetch_optional(&self.pool)
        .await?;

        match result {
            Some(row) => {
                // Check if revoked
                if row.revoked_at.is_some() {
                    return Ok(None);
                }

                // Check if expired
                if let Some(exp) = row.expires_at {
                    if exp < now {
                        return Ok(None);
                    }
                }

                Ok(Some(TokenInfo {
                    user_id: row.user_id,
                    username: row.username,
                    token_name: row.name,
                    scope: row.scope,
                }))
            }
            None => Ok(None),
        }
    }

    /// Revoke a token
    pub async fn revoke_token(&self, token: &str) -> Result<bool, sqlx::Error> {
        let token_hash = self.hash_token(token);
        let now = now_secs() as i64;

        let result = sqlx::query(
            "UPDATE tokens SET revoked_at = ?1 WHERE token_hash = ?2 AND revoked_at IS NULL"
        )
        .bind(now)
        .bind(token_hash)
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected() > 0)
    }

    /// Get all tokens for a user (without returning the actual tokens, just metadata)
    pub async fn list_tokens_for_user(&self, user_id: &str) -> Result<Vec<TokenMetadata>, sqlx::Error> {
        let rows = sqlx::query_as::<_, TokenMetadataRow>(
            "SELECT name, scope, created_at, expires_at, revoked_at
             FROM tokens WHERE user_id = ?1 ORDER BY created_at DESC"
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|row| TokenMetadata {
                name: row.name,
                scope: row.scope,
                created_at: row.created_at as u64,
                expires_at: row.expires_at.map(|v| v as u64),
                revoked_at: row.revoked_at.map(|v| v as u64),
            })
            .collect())
    }

    /// Hash a token using SHA256 with server-side salt.
    /// M4 fix: Salt prevents offline dictionary attacks if database is compromised.
    fn hash_token(&self, token: &str) -> String {
        let salted_token = format!("{}{}", self.hash_salt, token);
        hex::encode(Sha256::digest(salted_token.as_bytes()))
    }

    /// Set expiration on a token (for testing)
    pub async fn set_token_expiration(&self, token: &str, expires_at: u64) -> Result<(), sqlx::Error> {
        let token_hash = self.hash_token(token);

        sqlx::query("UPDATE tokens SET expires_at = ?1 WHERE token_hash = ?2")
            .bind(expires_at as i64)
            .bind(token_hash)
            .execute(&self.pool)
            .await?;

        Ok(())
    }
}

/// Internal row type for list_tokens_for_user query
#[derive(Debug, FromRow)]
struct TokenMetadataRow {
    name: String,
    scope: String,
    created_at: i64,
    expires_at: Option<i64>,
    revoked_at: Option<i64>,
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

    #[tokio::test]
    async fn test_hash_token() {
        let store = TokenStore::in_memory().await.unwrap();
        let hash1 = store.hash_token("hf_test123");
        let hash2 = store.hash_token("hf_test123");
        assert_eq!(hash1, hash2, "Same token should produce same hash");

        let hash3 = store.hash_token("hf_test456");
        assert_ne!(hash1, hash3, "Different tokens should produce different hashes");
    }

    #[tokio::test]
    async fn test_create_and_validate_token() {
        let store = TokenStore::in_memory().await.unwrap();

        let token = store.create_token("testuser", "test-token", "read").await.unwrap();
        assert!(token.starts_with("hf_"), "Token should start with hf_");

        let info = store.validate_token(&token).await.unwrap();
        assert!(info.is_some());

        let info = info.unwrap();
        assert_eq!(info.username, "testuser");
        assert_eq!(info.token_name, "test-token");
        assert_eq!(info.scope, "read");
    }

    #[tokio::test]
    async fn test_invalid_token() {
        let store = TokenStore::in_memory().await.unwrap();

        let info = store.validate_token("hf_invalid").await.unwrap();
        assert!(info.is_none(), "Invalid token should return None");
    }

    #[tokio::test]
    async fn test_expired_token() {
        let store = TokenStore::in_memory().await.unwrap();

        let token = store.create_token("testuser", "test-token", "read").await.unwrap();

        // Set expiration in the past
        let past_time = now_secs() - 3600; // 1 hour ago
        store.set_token_expiration(&token, past_time).await.unwrap();

        let info = store.validate_token(&token).await.unwrap();
        assert!(info.is_none(), "Expired token should return None");
    }

    #[tokio::test]
    async fn test_revoked_token() {
        let store = TokenStore::in_memory().await.unwrap();

        let token = store.create_token("testuser", "test-token", "read").await.unwrap();

        // Verify token works
        let info = store.validate_token(&token).await.unwrap();
        assert!(info.is_some());

        // Revoke the token
        let revoked = store.revoke_token(&token).await.unwrap();
        assert!(revoked, "revoke_token should return true for existing token");

        // Verify token no longer works
        let info = store.validate_token(&token).await.unwrap();
        assert!(info.is_none(), "Revoked token should return None");
    }

    #[tokio::test]
    async fn test_write_scope() {
        let store = TokenStore::in_memory().await.unwrap();

        let token = store.create_token("testuser", "write-token", "write").await.unwrap();

        let info = store.validate_token(&token).await.unwrap().unwrap();
        assert_eq!(info.scope, "write");
    }
}
