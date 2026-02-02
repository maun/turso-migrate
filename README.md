# turso-migrate

A simple migration library for Turso, inspired by [rusqlite_migration](https://crates.io/crates/rusqlite_migration).

Stores the number of applied migrations in the `user_version` of the SQLite/Turso database.
Supports migrations from a string, file, or Rust function. Migrations are applied in definition order, each within one transaction.

## Example

```rust
use turso_migrate::{up_file, up_fn, Migration, Migrations};

// Migrations are applied in definition order.
// You have to ensure not to modify previously applied migrations.
const MIGRATIONS: Migrations = Migrations::new(&[
    // 1. Hardcoded SQL
    Migration::up("001", "CREATE TABLE friend(name TEXT NOT NULL);"),
    // 2. From a file
    up_file!("../tests/migration-files/001_test.sql"),
    // 3. Rust function
    up_fn!("003", my_migration),
]);

async fn my_migration(conn: &turso::Connection) -> turso::Result<()> {
    conn.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)", ()).await?;
    Ok(())
}

#[tokio::main]
async fn main() {
    let db = turso::Builder::new_local(":memory:").build().await.unwrap();
    let mut conn = db.connect().unwrap();

    // Apply all pending migrations
    MIGRATIONS.to_latest(&mut conn).await.unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_migrations() {
        // Verify that migrations are applied successfully
        assert!(MIGRATIONS.run_all_in_memory().await.is_ok());
    }
}
```

## Features

- **tracing**: Optional. Enables logging of migration progress and errors using the `tracing` crate.
