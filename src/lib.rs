#![doc = include_str!("../README.md")]

use std::future::Future;
use std::pin::Pin;
use thiserror::Error;
use turso::{Connection, Error};

/// Type alias for the future returned by migration functions.
pub type MigrationFuture<'a> = Pin<Box<dyn Future<Output = Result<(), Error>> + Send + 'a>>;

#[derive(Error, Debug)]
pub enum MigrationError {
    #[error("Turso error: {0}")]
    Turso(#[from] Error),
    #[error("Database has user_version {0}, but it must be between 0 and {1}")]
    InvalidUserVersion(i32, usize),
}

/// A single migration step.
/// Can be a raw SQL string, an async function, or a file.
#[derive(Clone, Copy)]
pub enum Migration<'a> {
    Sql {
        name: &'a str,
        sql: &'a str,
    },
    Fn {
        name: &'a str,
        f: fn(&Connection) -> MigrationFuture,
    },
}

impl<'a> Migration<'a> {
    /// Creates a migration from a raw SQL string.
    pub const fn up(name: &'a str, sql: &'a str) -> Self {
        Self::Sql { name, sql }
    }

    /// Creates a migration from a rust function.
    /// The function must take a `&Connection` and return a `MigrationFuture`.
    /// Use the `up_fn!` macro to directly take an async function.
    pub const fn up_fn(name: &'a str, f: fn(&Connection) -> MigrationFuture) -> Self {
        Self::Fn { name, f }
    }
}

/// Helper macro to create a migration from a file.
/// Uses `include_str!` to embed the SQL content.
///
/// # Example
///
/// ```rust
/// use turso_migrate::{up_file, Migration, Migrations};
///
/// const MIGRATIONS: Migrations = Migrations::new(&[
///     up_file!("../tests/migration-files/001_test.sql"),
/// ]);
/// ```
#[macro_export]
macro_rules! up_file {
    ($path:literal) => {
        $crate::Migration::up($path, include_str!($path))
    };
}

/// Helper macro to create a migration from an async function.
///
/// # Example
///
/// ```rust
/// use turso_migrate::{up_fn, Migration, Migrations};
///
/// async fn my_migration(conn: &turso::Connection) -> turso::Result<()> {
///     Ok(())
/// }
///
/// const MIGRATIONS: Migrations = Migrations::new(&[
///     up_fn!("001", my_migration),
/// ]);
/// ```
#[macro_export]
macro_rules! up_fn {
    ($name:expr, $func:path) => {{
        fn wrapper(conn: &turso::Connection) -> $crate::MigrationFuture<'_> {
            Box::pin($func(conn))
        }
        $crate::Migration::up_fn($name, wrapper)
    }};
}

/// Manages the application of migrations.
pub struct Migrations<'a> {
    migrations: &'a [Migration<'a>],
}

impl<'a> Migrations<'a> {
    pub const fn new(migrations: &'a [Migration<'a>]) -> Self {
        Self { migrations }
    }

    /// Applies all pending migrations to bring the database to the latest version.
    /// Each migration is applied in its own transaction.
    /// Returns the number of applied migrations.
    /// Uses the turso/sqlite table user_version to track the current version.
    pub async fn to_latest(&self, conn: &mut Connection) -> Result<usize, (usize, MigrationError)> {
        let current_version = match get_user_version(conn).await {
            Ok(v) => v,
            Err(e) => return Err((0, e.into())),
        };
        let target_version = self.migrations.len() as i32;
        if current_version == target_version {
            return Ok(0);
        }
        if current_version < 0 || current_version > target_version {
            return Err((
                0,
                MigrationError::InvalidUserVersion(current_version, self.migrations.len()),
            ));
        }

        let mut applied_count = 0;
        for (i, migration) in self
            .migrations
            .iter()
            .enumerate()
            .skip(current_version as usize)
        {
            let version = (i + 1) as i32;

            // Start a transaction for this migration
            let tx = match conn.transaction().await {
                Ok(tx) => tx,
                Err(e) => return Err((applied_count, e.into())),
            };

            // Apply the migration logic
            let result = match migration {
                Migration::Sql { name: _name, sql } => tx.execute(sql, ()).await.map(|_| ()),
                Migration::Fn { name: _name, f } => f(&tx).await,
            };

            // Capture the name for logging, ensuring we use the one bound in the match or constructing it
            #[cfg(feature = "tracing")]
            let migration_name_log = match migration {
                Migration::Sql { name, .. } => *name,
                Migration::Fn { name, .. } => *name,
            };

            if let Err(e) = result {
                #[cfg(feature = "tracing")]
                tracing::error!(error = ?e, migration_name = %migration_name_log, "Migration failed");
                return Err((applied_count, e.into()));
            }

            // Update the user_version within the same transaction
            if let Err(e) = set_user_version(&tx, version).await {
                #[cfg(feature = "tracing")]
                tracing::error!(error = ?e, migration_name = %migration_name_log, "Failed to update user_version");
                return Err((applied_count, e.into()));
            }

            // Commit the transaction
            if let Err(e) = tx.commit().await {
                #[cfg(feature = "tracing")]
                tracing::error!(error = ?e, migration_name = %migration_name_log, "Failed to commit transaction");
                return Err((applied_count, e.into()));
            }

            #[cfg(feature = "tracing")]
            tracing::debug!(migration_name = %migration_name_log, "Migration applied successfully");

            applied_count += 1;
        }

        Ok(applied_count)
    }

    /// Helper function to validate migrations by applying them to an in-memory database.
    /// Returns the connection to the in-memory database if successful.
    pub async fn run_all_in_memory(&self) -> Result<Connection, MigrationError> {
        let db = turso::Builder::new_local(":memory:").build().await?;
        let mut conn = db.connect()?;
        if let Err((_, e)) = self.to_latest(&mut conn).await {
            return Err(e);
        }
        Ok(conn)
    }
}

async fn get_user_version(conn: &Connection) -> Result<i32, Error> {
    let version = std::cell::Cell::new(0);
    conn.pragma_query("user_version", |row| {
        let v = row.get::<i32>(0).unwrap();
        version.set(v);
        Ok(())
    })
    .await?;
    Ok(version.get())
}

async fn set_user_version(conn: &Connection, version: i32) -> Result<(), Error> {
    conn.pragma_update("user_version", version).await?;
    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;
    use turso::Builder;

    const MIGRATIONS: Migrations = Migrations::new(&[
        Migration::up("001", "CREATE TABLE friend(name TEXT NOT NULL);"),
        up_fn!("002", my_complex_migration),
    ]);

    // Example of an external migration function
    async fn my_complex_migration(conn: &Connection) -> turso::Result<()> {
        conn.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)", ())
            .await?;
        conn.execute("INSERT INTO users (name) VALUES ('Alice')", ())
            .await?;
        Ok(())
    }

    async fn get_in_memory_conn() -> Connection {
        let db = Builder::new_local(":memory:").build().await.unwrap();
        db.connect().unwrap()
    }

    #[tokio::test]
    async fn test_migrations_success() {
        let mut conn = get_in_memory_conn().await;

        // Apply migrations
        let applied_count = MIGRATIONS.to_latest(&mut conn).await.unwrap();
        assert_eq!(applied_count, 2);

        // Verify version
        let version = get_user_version(&conn).await.unwrap();
        assert_eq!(version, 2);
    }

    #[tokio::test]
    async fn test_idempotency() {
        let mut conn = get_in_memory_conn().await;

        // First run
        let applied_count = MIGRATIONS.to_latest(&mut conn).await.unwrap();
        assert_eq!(applied_count, 2);

        // Second run
        let applied_count = MIGRATIONS.to_latest(&mut conn).await.unwrap();
        assert_eq!(applied_count, 0);
    }

    #[tokio::test]
    async fn test_partial_migration() {
        let mut conn = get_in_memory_conn().await;

        // Manually set version to 1 (pretending migration 1 is already applied)
        conn.pragma_update("user_version", 1).await.unwrap();

        let count = MIGRATIONS.to_latest(&mut conn).await.unwrap();
        assert_eq!(count, 1);

        let version = get_user_version(&conn).await.unwrap();
        assert_eq!(version, 2);
    }

    #[tokio::test]
    async fn test_validate_helper() {
        // Validate should pass without error
        let conn = MIGRATIONS.run_all_in_memory().await;
        assert!(conn.is_ok());

        // Verify we can query the db
        let conn = conn.unwrap();
        let version = get_user_version(&conn).await.unwrap();
        assert_eq!(version, 2);
    }

    #[tokio::test]
    async fn test_negative_user_version() {
        let mut conn = get_in_memory_conn().await;

        // Manually set negative version
        conn.pragma_update("user_version", -5).await.unwrap();

        let result = MIGRATIONS.to_latest(&mut conn).await;

        match result {
            Err((0, MigrationError::InvalidUserVersion(v, 2))) => assert_eq!(v, -5),
            _ => panic!("Expected InvalidUserVersion error, got {:?}", result),
        }
    }

    #[tokio::test]
    async fn test_user_version_too_high() {
        let mut conn = get_in_memory_conn().await;

        // Manually set version higher than migration count (2)
        conn.pragma_update("user_version", 10).await.unwrap();

        let result = MIGRATIONS.to_latest(&mut conn).await;

        match result {
            Err((0, MigrationError::InvalidUserVersion(v, max))) => {
                assert_eq!(v, 10);
                assert_eq!(max, 2);
            }
            _ => panic!("Expected InvalidUserVersion error, got {:?}", result),
        }
    }

    #[tokio::test]
    async fn test_failing_migration() {
        let mut conn = get_in_memory_conn().await;

        const BROKEN_MIGRATIONS: Migrations = Migrations::new(&[
            Migration::up("001", "CREATE TABLE ok (id int)"),
            Migration::up("002", "SELECT * FROM non_existent_table"), // This should fail
        ]);

        let result = BROKEN_MIGRATIONS.to_latest(&mut conn).await;
        match result {
            Err((1, MigrationError::Turso(_))) => {} // Expected one successful migration
            _ => panic!("Expected Turso error, got {:?}", result),
        }

        // Ensure database version was NOT updated for the failing migration
        let version = get_user_version(&conn).await.unwrap();
        assert_eq!(version, 1);
    }

    #[tokio::test]
    async fn test_up_file_macro() {
        let mut conn = get_in_memory_conn().await;

        const FILE_MIGRATIONS: Migrations =
            Migrations::new(&[up_file!("../tests/migration-files/001_test.sql")]);

        let result = FILE_MIGRATIONS.to_latest(&mut conn).await;
        assert!(result.is_ok(), "File migration failed: {:?}", result.err());

        // Verify version
        let version = get_user_version(&conn).await.unwrap();
        assert_eq!(version, 1);

        // Verify table exists
        conn.execute("INSERT INTO file_test (id) VALUES (1)", ())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_dynamic_migration_name() {
        let mut conn = get_in_memory_conn().await;

        let dynamic_name = String::from("003_dynamic");
        let dynamic_sql = String::from("CREATE TABLE dynamic(id int)");

        let migrations = [Migration::up(&dynamic_name, &dynamic_sql)];
        let migrations = Migrations::new(&migrations);

        let applied = migrations.to_latest(&mut conn).await.unwrap();
        assert_eq!(applied, 1);

        let version = get_user_version(&conn).await.unwrap();
        assert_eq!(version, 1);
    }
}
