use anyhow::{Context, Result};
use rusqlite::Connection;

/// A rating row returned by [`get_unsynced_ratings`].
#[derive(Debug, Clone)]
pub struct LocalRating {
    pub id: i64,
    pub skill_id: String,
    pub version: String,
    pub rating: String,
    pub rated_at: i64,
}

/// Increment the execution count for a skill+version. Returns the new count.
///
/// Only called on successful executions. Increments `count` (for rating prompt
/// schedule), `total_runs`, and `successful_runs`.
pub fn increment_execution_count(conn: &Connection, skill_id: &str, version: &str) -> Result<i64> {
    conn.execute(
        "INSERT INTO skill_execution_counts (skill_id, version, count, total_runs, successful_runs)
         VALUES (?1, ?2, 1, 1, 1)
         ON CONFLICT (skill_id, version) DO UPDATE SET
           count = count + 1,
           total_runs = total_runs + 1,
           successful_runs = successful_runs + 1",
        rusqlite::params![skill_id, version],
    )
    .with_context(|| format!("failed to increment execution count for {skill_id}@{version}"))?;

    let count: i64 = conn
        .query_row(
            "SELECT count FROM skill_execution_counts WHERE skill_id = ?1 AND version = ?2",
            rusqlite::params![skill_id, version],
            |row| row.get(0),
        )
        .with_context(|| {
            format!("failed to read execution count for {skill_id}@{version} after increment")
        })?;

    Ok(count)
}

/// Record a failed execution. Increments `total_runs` but NOT `successful_runs` or `count`.
pub fn record_failed_execution(conn: &Connection, skill_id: &str, version: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO skill_execution_counts (skill_id, version, count, total_runs, successful_runs)
         VALUES (?1, ?2, 0, 1, 0)
         ON CONFLICT (skill_id, version) DO UPDATE SET total_runs = total_runs + 1",
        rusqlite::params![skill_id, version],
    )
    .with_context(|| format!("failed to record failed execution for {skill_id}@{version}"))?;
    Ok(())
}

/// Check if a rating prompt should be shown for the given execution count.
///
/// Prompts on the 3rd use, then every 5th use after that (8th, 13th, 18th, …).
pub fn should_prompt_for_rating(count: i64) -> bool {
    count == 3 || (count > 3 && (count - 3) % 5 == 0)
}

/// Check if a rating already exists for this skill+version.
pub fn has_existing_rating(conn: &Connection, skill_id: &str, version: &str) -> Result<bool> {
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM skill_ratings WHERE skill_id = ?1 AND version = ?2",
            rusqlite::params![skill_id, version],
            |row| row.get(0),
        )
        .with_context(|| format!("failed to check existing rating for {skill_id}@{version}"))?;

    Ok(count > 0)
}

/// Record a rating (upsert — overwrites any previous rating for the same skill+version).
///
/// `rating` must be `"up"` or `"down"`. The SQLite CHECK constraint will reject
/// any other value.
pub fn record_rating(conn: &Connection, skill_id: &str, version: &str, rating: &str) -> Result<()> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .context("system clock is before UNIX epoch")?
        .as_secs() as i64;

    conn.execute(
        "INSERT INTO skill_ratings (skill_id, version, rating, rated_at, synced)
         VALUES (?1, ?2, ?3, ?4, 0)
         ON CONFLICT (skill_id, version) DO UPDATE SET rating = ?3, rated_at = ?4, synced = 0",
        rusqlite::params![skill_id, version, rating, now],
    )
    .with_context(|| format!("failed to record rating for {skill_id}@{version}"))?;

    Ok(())
}

/// Return all ratings that have not yet been synced to the registry.
pub fn get_unsynced_ratings(conn: &Connection) -> Result<Vec<LocalRating>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, skill_id, version, rating, rated_at
             FROM skill_ratings WHERE synced = 0",
        )
        .context("failed to prepare unsynced-ratings query")?;

    let ratings = stmt
        .query_map([], |row| {
            Ok(LocalRating {
                id: row.get(0)?,
                skill_id: row.get(1)?,
                version: row.get(2)?,
                rating: row.get(3)?,
                rated_at: row.get(4)?,
            })
        })
        .context("failed to execute unsynced-ratings query")?
        .collect::<Result<Vec<_>, _>>()
        .context("failed to collect unsynced-ratings rows")?;

    Ok(ratings)
}

/// Execution statistics for a skill+version, used for batch sync to the registry.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ExecutionStats {
    pub skill_id: String,
    pub version: String,
    pub total_runs: i64,
    pub successful_runs: i64,
}

/// Get execution stats for all skills that have at least one recorded run.
pub fn get_execution_stats(conn: &Connection) -> Result<Vec<ExecutionStats>> {
    let mut stmt = conn
        .prepare(
            "SELECT skill_id, version, total_runs, successful_runs
             FROM skill_execution_counts WHERE total_runs > 0",
        )
        .context("failed to prepare execution-stats query")?;

    let stats = stmt
        .query_map([], |row| {
            Ok(ExecutionStats {
                skill_id: row.get(0)?,
                version: row.get(1)?,
                total_runs: row.get(2)?,
                successful_runs: row.get(3)?,
            })
        })
        .context("failed to execute execution-stats query")?
        .collect::<Result<Vec<_>, _>>()
        .context("failed to collect execution-stats rows")?;

    Ok(stats)
}

/// Mark the given rating rows as synced after a successful registry upload.
pub fn mark_ratings_synced(conn: &Connection, ids: &[i64]) -> Result<()> {
    for id in ids {
        conn.execute(
            "UPDATE skill_ratings SET synced = 1 WHERE id = ?1",
            rusqlite::params![id],
        )
        .with_context(|| format!("failed to mark rating {id} as synced"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[allow(clippy::wildcard_imports)]
    use super::*;
    use rusqlite::Connection;

    fn setup_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE skill_execution_counts (
                skill_id        TEXT NOT NULL,
                version         TEXT NOT NULL,
                count           INTEGER NOT NULL DEFAULT 0,
                total_runs      INTEGER NOT NULL DEFAULT 0,
                successful_runs INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (skill_id, version)
            );
            CREATE TABLE skill_ratings (
                id       INTEGER PRIMARY KEY AUTOINCREMENT,
                skill_id TEXT NOT NULL,
                version  TEXT NOT NULL,
                rating   TEXT NOT NULL CHECK (rating IN ('up', 'down')),
                rated_at INTEGER NOT NULL,
                synced   INTEGER NOT NULL DEFAULT 0
            );
            CREATE UNIQUE INDEX idx_skill_ratings_one_per_version
                ON skill_ratings (skill_id, version);",
        )
        .unwrap();
        conn
    }

    #[test]
    fn increment_execution_count_starts_at_one() {
        let conn = setup_db();
        assert_eq!(
            increment_execution_count(&conn, "test-skill", "0.1.0").unwrap(),
            1
        );
    }

    #[test]
    fn increment_execution_count_increments() {
        let conn = setup_db();
        increment_execution_count(&conn, "test-skill", "0.1.0").unwrap();
        increment_execution_count(&conn, "test-skill", "0.1.0").unwrap();
        assert_eq!(
            increment_execution_count(&conn, "test-skill", "0.1.0").unwrap(),
            3
        );
    }

    #[test]
    fn increment_is_per_skill_version() {
        let conn = setup_db();
        increment_execution_count(&conn, "skill-a", "0.1.0").unwrap();
        increment_execution_count(&conn, "skill-a", "0.1.0").unwrap();
        let count_b = increment_execution_count(&conn, "skill-b", "0.1.0").unwrap();
        assert_eq!(count_b, 1, "counts are isolated per skill_id");
    }

    #[test]
    fn prompt_schedule_fires_at_3() {
        assert!(!should_prompt_for_rating(1));
        assert!(!should_prompt_for_rating(2));
        assert!(should_prompt_for_rating(3));
        assert!(!should_prompt_for_rating(4));
    }

    #[test]
    fn prompt_schedule_fires_every_5_after_3() {
        assert!(should_prompt_for_rating(8));
        assert!(should_prompt_for_rating(13));
        assert!(should_prompt_for_rating(18));
        assert!(!should_prompt_for_rating(9));
        assert!(!should_prompt_for_rating(10));
    }

    #[test]
    fn record_rating_upserts() {
        let conn = setup_db();
        record_rating(&conn, "test-skill", "0.1.0", "up").unwrap();
        assert!(has_existing_rating(&conn, "test-skill", "0.1.0").unwrap());
        // Overwrite with down
        record_rating(&conn, "test-skill", "0.1.0", "down").unwrap();
        let ratings = get_unsynced_ratings(&conn).unwrap();
        assert_eq!(ratings.len(), 1);
        assert_eq!(ratings[0].rating, "down");
    }

    #[test]
    fn no_rating_means_has_existing_is_false() {
        let conn = setup_db();
        assert!(!has_existing_rating(&conn, "ghost-skill", "1.0.0").unwrap());
    }

    #[test]
    fn mark_synced_excludes_from_unsynced() {
        let conn = setup_db();
        record_rating(&conn, "s1", "0.1.0", "up").unwrap();
        record_rating(&conn, "s2", "0.1.0", "down").unwrap();
        let ratings = get_unsynced_ratings(&conn).unwrap();
        assert_eq!(ratings.len(), 2);
        mark_ratings_synced(&conn, &[ratings[0].id]).unwrap();
        let remaining = get_unsynced_ratings(&conn).unwrap();
        assert_eq!(remaining.len(), 1);
    }

    #[test]
    fn mark_synced_all_leaves_none_unsynced() {
        let conn = setup_db();
        record_rating(&conn, "s1", "0.1.0", "up").unwrap();
        let ratings = get_unsynced_ratings(&conn).unwrap();
        let ids: Vec<i64> = ratings.iter().map(|r| r.id).collect();
        mark_ratings_synced(&conn, &ids).unwrap();
        assert!(get_unsynced_ratings(&conn).unwrap().is_empty());
    }

    #[test]
    fn test_record_failed_execution() {
        let conn = setup_db();
        record_failed_execution(&conn, "test-skill", "0.1.0").unwrap();

        let stats = get_execution_stats(&conn).unwrap();
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].total_runs, 1);
        assert_eq!(stats[0].successful_runs, 0);

        // count (rating schedule) should still be 0 — failure does not trigger rating prompts.
        // Verify by checking that increment_execution_count still starts at 1 for count.
        let count = increment_execution_count(&conn, "test-skill", "0.1.0").unwrap();
        assert_eq!(
            count, 1,
            "count starts at 1 for first success, not incremented by failures"
        );

        let stats = get_execution_stats(&conn).unwrap();
        assert_eq!(stats[0].total_runs, 2);
        assert_eq!(stats[0].successful_runs, 1);
    }

    #[test]
    fn test_execution_stats_returned() {
        let conn = setup_db();

        increment_execution_count(&conn, "skill-a", "1.0.0").unwrap();
        increment_execution_count(&conn, "skill-a", "1.0.0").unwrap();
        increment_execution_count(&conn, "skill-a", "1.0.0").unwrap();
        record_failed_execution(&conn, "skill-a", "1.0.0").unwrap();
        record_failed_execution(&conn, "skill-a", "1.0.0").unwrap();

        record_failed_execution(&conn, "skill-b", "0.2.0").unwrap();

        let mut stats = get_execution_stats(&conn).unwrap();
        stats.sort_by(|a, b| a.skill_id.cmp(&b.skill_id));

        assert_eq!(stats.len(), 2);

        assert_eq!(stats[0].skill_id, "skill-a");
        assert_eq!(stats[0].total_runs, 5);
        assert_eq!(stats[0].successful_runs, 3);

        assert_eq!(stats[1].skill_id, "skill-b");
        assert_eq!(stats[1].total_runs, 1);
        assert_eq!(stats[1].successful_runs, 0);
    }

    #[test]
    fn test_execution_stats_empty_when_no_runs() {
        let conn = setup_db();
        let stats = get_execution_stats(&conn).unwrap();
        assert!(stats.is_empty());
    }
}
