//! `sanctuary grant-super-user <user_id>`: the only way to move the server's single super_user.
use sqlx::postgres::PgPoolOptions;
use std::env;

pub(crate) async fn run(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let id = args
        .first()
        .and_then(|value| value.parse::<i64>().ok())
        .filter(|id| *id > 0)
        .ok_or("usage: sanctuary grant-super-user <user_id>")?;
    let db = PgPoolOptions::new()
        .max_connections(1)
        .connect(&env::var("DATABASE_URL")?)
        .await?;
    let mut tx = db.begin().await?;
    sqlx::query("LOCK TABLE roles,user_roles IN SHARE ROW EXCLUSIVE MODE")
        .execute(&mut *tx)
        .await?;
    let role: i64 = sqlx::query_scalar("SELECT id FROM roles WHERE name='super_user'")
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(
            "The super_user role does not exist yet; start the server once to apply migrations",
        )?;
    let target: Option<(String, bool)> =
        sqlx::query_as("SELECT username,is_restricted FROM users WHERE id=$1")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    let (username, restricted) = target.ok_or("No user has that ID")?;
    if restricted {
        return Err("A restricted user cannot become the super_user".into());
    }
    let previous: Vec<i64> =
        sqlx::query_scalar("SELECT user_id FROM user_roles WHERE role_id=$1 AND user_id<>$2")
            .bind(role)
            .bind(id)
            .fetch_all(&mut *tx)
            .await?;
    for holder in &previous {
        sqlx::query("DELETE FROM user_roles WHERE user_id=$1 AND role_id=$2")
            .bind(holder)
            .bind(role)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO user_roles(user_id,role_id) SELECT $1,id FROM roles WHERE name='administrator' ON CONFLICT DO NOTHING")
            .bind(holder)
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query("INSERT INTO user_roles(user_id,role_id) VALUES($1,$2) ON CONFLICT DO NOTHING")
        .bind(id)
        .bind(role)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO audit_events(actor_id,action,target_type,target_id,details) VALUES(NULL,'roles.super_user.grant','user',$1,$2)")
        .bind(id.to_string())
        .bind(serde_json::json!({"previous_holders": previous, "via": "cli"}))
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    println!("{username} (id {id}) is now the super_user.");
    for holder in previous {
        println!("Previous holder (id {holder}) was moved to administrator.");
    }
    Ok(())
}
