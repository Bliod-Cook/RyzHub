//! Partition maintenance, run with the migration role (runtime roles cannot perform DDL).
//!
//! Weekly `events` and monthly `audit_logs` and `usage_records` partitions are created ahead of
//! time. Each table also has a DEFAULT partition, so writes never fail when maintenance lapses;
//! rows that land there are moved into their proper partition before it is attached.
use anyhow::ensure;
use chrono::{Datelike, Duration, Months, NaiveDate, Utc, Weekday};
use sqlx::{PgPool, Postgres, Transaction};

use crate::config::Config;

/// Partitions created ahead of today.
const WEEKS_AHEAD: i64 = 2;
const MONTHS_AHEAD: u32 = 2;

struct Partitioned {
    table: &'static str,
    default: &'static str,
}

const EVENTS: Partitioned = Partitioned {
    table: "events",
    default: "events_default",
};
const AUDIT: Partitioned = Partitioned {
    table: "audit_logs",
    default: "audit_logs_default",
};
const USAGE: Partitioned = Partitioned {
    table: "usage_records",
    default: "usage_records_default",
};

fn week_name(start: NaiveDate) -> String {
    format!(
        "events_{}_{:02}",
        start.iso_week().year(),
        start.iso_week().week()
    )
}

fn month_name(parent: &Partitioned, start: NaiveDate) -> String {
    format!("{}_{}_{:02}", parent.table, start.year(), start.month())
}

fn monday(day: NaiveDate) -> NaiveDate {
    day - Duration::days(day.weekday().num_days_from_monday() as i64)
}

fn first_of_month(day: NaiveDate) -> NaiveDate {
    NaiveDate::from_ymd_opt(day.year(), day.month(), 1).expect("valid month")
}

/// Ensure `[start, end)` has its own partition, moving any rows the DEFAULT partition holds
/// for that range. All identifiers are generated from dates, never from input.
async fn ensure_partition(
    tx: &mut Transaction<'_, Postgres>,
    parent: &Partitioned,
    name: &str,
    start: NaiveDate,
    end: NaiveDate,
) -> anyhow::Result<()> {
    let exists: bool = sqlx::query_scalar("SELECT to_regclass($1) IS NOT NULL")
        .bind(name)
        .fetch_one(&mut **tx)
        .await?;
    if !exists {
        let (table, default) = (parent.table, parent.default);
        let range = format!("created_at>='{start}' AND created_at<'{end}'");
        let stranded: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT EXISTS(SELECT 1 FROM {default} WHERE {range})"
        )))
        .fetch_one(&mut **tx)
        .await?;
        if stranded {
            for statement in [
                format!(
                    "CREATE TABLE {name} (LIKE {table} INCLUDING DEFAULTS INCLUDING CONSTRAINTS)"
                ),
                format!("INSERT INTO {name} SELECT * FROM {default} WHERE {range}"),
                format!("DELETE FROM {default} WHERE {range}"),
                format!(
                    "ALTER TABLE {table} ATTACH PARTITION {name} FOR VALUES FROM ('{start}') TO ('{end}')"
                ),
            ] {
                sqlx::query(sqlx::AssertSqlSafe(statement))
                    .execute(&mut **tx)
                    .await?;
            }
        } else {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "CREATE TABLE {name} PARTITION OF {table} FOR VALUES FROM ('{start}') TO ('{end}')"
            )))
            .execute(&mut **tx)
            .await?;
        }
    }
    // Parent-table RLS is the only access path; leaf partitions are never granted.
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "REVOKE ALL ON {name} FROM ryzhub_app,ryzhub_worker"
    )))
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Partition names and their first day, by parsing `<prefix>_<year>_<nn>`.
async fn partitions(
    tx: &mut Transaction<'_, Postgres>,
    parent: &str,
) -> anyhow::Result<Vec<String>> {
    Ok(sqlx::query_scalar(
        "SELECT child.relname::text FROM pg_inherits i JOIN pg_class child ON child.oid=i.inhrelid
         WHERE i.inhparent=$1::regclass",
    )
    .bind(parent)
    .fetch_all(&mut **tx)
    .await?)
}

/// Monthly partitions of `parent` for the coming months and any month with rows stranded in
/// its default partition.
async fn ensure_months(
    tx: &mut Transaction<'_, Postgres>,
    parent: &Partitioned,
    today: NaiveDate,
) -> anyhow::Result<()> {
    let mut months: Vec<NaiveDate> = (0..=MONTHS_AHEAD)
        .filter_map(|n| first_of_month(today).checked_add_months(Months::new(n)))
        .collect();
    let stranded: Vec<NaiveDate> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT DISTINCT date_trunc('month',created_at)::date FROM {}",
        parent.default
    )))
    .fetch_all(&mut **tx)
    .await?;
    months.extend(stranded);
    months.sort();
    months.dedup();
    for start in months {
        let end = start
            .checked_add_months(Months::new(1))
            .expect("valid month");
        ensure_partition(tx, parent, &month_name(parent, start), start, end).await?;
    }
    Ok(())
}

/// Drop the monthly partitions of `parent` that end on or before `cutoff`.
async fn drop_months(
    tx: &mut Transaction<'_, Postgres>,
    parent: &Partitioned,
    cutoff: NaiveDate,
) -> anyhow::Result<()> {
    for name in partitions(tx, parent.table).await? {
        let Some((year, month)) = name
            .strip_prefix(parent.table)
            .and_then(|rest| rest.strip_prefix('_'))
            .and_then(|rest| rest.split_once('_'))
        else {
            continue;
        };
        if let (Ok(year), Ok(month)) = (year.parse::<i32>(), month.parse::<u32>())
            && let Some(start) = NaiveDate::from_ymd_opt(year, month, 1)
            && start
                .checked_add_months(Months::new(1))
                .is_some_and(|end| end <= cutoff)
        {
            ensure!(
                month_name(parent, start) == name,
                "noncanonical partition name"
            );
            sqlx::query(sqlx::AssertSqlSafe(format!("DROP TABLE {name}")))
                .execute(&mut **tx)
                .await?;
        }
    }
    Ok(())
}

pub async fn maintain(pool: &PgPool, config: &Config) -> anyhow::Result<()> {
    ensure!(
        config.events_retention_days >= 7,
        "retention must be at least seven days"
    );
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(724998201)")
        .execute(&mut *tx)
        .await?;
    let today = Utc::now().date_naive();

    // Weekly events: upcoming weeks plus any week with rows stranded in the default.
    let mut weeks: Vec<NaiveDate> = (0..=WEEKS_AHEAD)
        .map(|n| monday(today) + Duration::weeks(n))
        .collect();
    let stranded: Vec<NaiveDate> = sqlx::query_scalar(
        "SELECT DISTINCT date_trunc('week',created_at)::date FROM events_default",
    )
    .fetch_all(&mut *tx)
    .await?;
    weeks.extend(stranded);
    weeks.sort();
    weeks.dedup();
    for start in weeks {
        ensure_partition(
            &mut tx,
            &EVENTS,
            &week_name(start),
            start,
            start + Duration::weeks(1),
        )
        .await?;
    }

    // Monthly audit logs and usage records, likewise.
    for parent in [&AUDIT, &USAGE] {
        ensure_months(&mut tx, parent, today).await?;
    }

    // Retention drops whole partitions: events keep up to a week beyond the setting.
    let cutoff = today - Duration::days(config.events_retention_days as i64);
    for name in partitions(&mut tx, "events").await? {
        let parts: Vec<_> = name.split('_').collect();
        if let ["events", year, week] = parts.as_slice()
            && let (Ok(year), Ok(week)) = (year.parse::<i32>(), week.parse::<u32>())
            && let Some(start) = NaiveDate::from_isoywd_opt(year, week, Weekday::Mon)
            && start + Duration::weeks(1) <= cutoff
        {
            ensure!(week_name(start) == name, "noncanonical partition name");
            sqlx::query(sqlx::AssertSqlSafe(format!("DROP TABLE {name}")))
                .execute(&mut *tx)
                .await?;
        }
    }
    // Monthly tables keep up to a month beyond their setting.
    for (parent, days) in [
        (&AUDIT, config.retention.audit_days),
        (&USAGE, config.retention.usage_days),
    ] {
        if days > 0 {
            drop_months(&mut tx, parent, today - Duration::days(days as i64)).await?;
        }
    }
    sqlx::query("DELETE FROM ws_tickets WHERE expires_at<now()")
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partition_names_follow_iso_weeks_and_months() {
        let day = NaiveDate::from_ymd_opt(2026, 1, 1).unwrap();
        assert_eq!(week_name(monday(day)), "events_2026_01");
        assert_eq!(
            month_name(&AUDIT, first_of_month(day)),
            "audit_logs_2026_01"
        );
        assert_eq!(
            month_name(&USAGE, first_of_month(day)),
            "usage_records_2026_01"
        );
    }
}
