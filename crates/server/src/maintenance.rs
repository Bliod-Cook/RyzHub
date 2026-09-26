//! Run daily with the migration role; runtime and worker roles cannot perform DDL.
use anyhow::ensure;
use chrono::{Datelike, Duration, Months, NaiveDate, Utc, Weekday};
use sqlx::PgPool;

pub async fn maintain(pool: &PgPool, retention_days: u32) -> anyhow::Result<()> {
    ensure!(retention_days >= 7, "retention must be at least seven days");
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(724998201)")
        .execute(&mut *tx)
        .await?;
    let today = Utc::now().date_naive();
    let monday = today - Duration::days(today.weekday().num_days_from_monday() as i64);
    for n in 0..=2 {
        let start = monday + Duration::weeks(n);
        let end = start + Duration::weeks(1);
        let name = format!(
            "events_{}_{:02}",
            start.iso_week().year(),
            start.iso_week().week()
        );
        // All interpolated identifiers and literals below are generated dates, never user text.
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE TABLE IF NOT EXISTS {name} PARTITION OF events FOR VALUES FROM ('{start}') TO ('{end}')"))).execute(&mut *tx).await?;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "REVOKE ALL ON {name} FROM ryzhub_app,ryzhub_worker"
        )))
        .execute(&mut *tx)
        .await?;
    }
    let month = NaiveDate::from_ymd_opt(today.year(), today.month(), 1).unwrap();
    for n in 0..=2 {
        let start = month.checked_add_months(Months::new(n)).unwrap();
        let end = start.checked_add_months(Months::new(1)).unwrap();
        let name = format!("audit_logs_{}_{:02}", start.year(), start.month());
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE TABLE IF NOT EXISTS {name} PARTITION OF audit_logs FOR VALUES FROM ('{start}') TO ('{end}')"))).execute(&mut *tx).await?;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "REVOKE ALL ON {name} FROM ryzhub_app,ryzhub_worker"
        )))
        .execute(&mut *tx)
        .await?;
    }
    let names:Vec<String>=sqlx::query_scalar("SELECT child.relname FROM pg_inherits i JOIN pg_class child ON child.oid=i.inhrelid JOIN pg_class parent ON parent.oid=i.inhparent WHERE parent.oid='events'::regclass").fetch_all(&mut *tx).await?;
    for name in names {
        let parts: Vec<_> = name.split('_').collect();
        if let ["events", year, week] = parts.as_slice()
            && let (Ok(year), Ok(week)) = (year.parse::<i32>(), week.parse::<u32>())
            && let Some(start) = NaiveDate::from_isoywd_opt(year, week, Weekday::Mon)
            && start + Duration::weeks(1) <= today - Duration::days(retention_days as i64)
        {
            let safe_name = format!("events_{year}_{week:02}");
            ensure!(safe_name == name, "noncanonical partition name");
            sqlx::query(sqlx::AssertSqlSafe(format!("DROP TABLE {safe_name}")))
                .execute(&mut *tx)
                .await?;
        }
    }
    sqlx::query("DELETE FROM ws_tickets WHERE expires_at<now()")
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM ws_resumes WHERE expires_at<now()")
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}
