use sqlx::{SqlitePool, sqlite::SqlitePoolOptions};

pub async fn connect(database_url: &str) -> Result<SqlitePool, sqlx::Error> {
    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect(database_url)
        .await?;
    sqlx::migrate!("./migrations").run(&pool).await?;
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    sqlx::query("UPDATE render_jobs SET status='interrupted', error_code='startup_interrupted' WHERE status IN ('preparing','voicing','animating','mixing','validating')")
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(pool)
}
