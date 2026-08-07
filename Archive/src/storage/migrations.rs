use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use super::StorageError;

pub(super) const LATEST_SCHEMA_VERSION: i64 = 3;
const SCHEMA_V1: &str = include_str!("schema_v1.sql");
const SCHEMA_V2: &str = include_str!("schema_v2.sql");
const SCHEMA_V3: &str = include_str!("schema_v3.sql");

#[derive(Debug, Eq, PartialEq)]
struct ColumnShape {
    name: String,
    declared_type: String,
    not_null: bool,
    default_value: Option<String>,
    primary_key_position: i64,
}

#[derive(Clone, Copy)]
struct ExpectedColumn {
    name: &'static str,
    declared_type: &'static str,
    not_null: bool,
    default_value: Option<&'static str>,
    primary_key_position: i64,
}

const MIGRATION_COLUMNS: &[ExpectedColumn] = &[
    ExpectedColumn {
        name: "version",
        declared_type: "INTEGER",
        not_null: false,
        default_value: None,
        primary_key_position: 1,
    },
    ExpectedColumn {
        name: "applied_at",
        declared_type: "INTEGER",
        not_null: true,
        default_value: None,
        primary_key_position: 0,
    },
];

const SESSION_COLUMNS: &[ExpectedColumn] = &[
    ExpectedColumn {
        name: "singleton",
        declared_type: "INTEGER",
        not_null: false,
        default_value: None,
        primary_key_position: 1,
    },
    ExpectedColumn {
        name: "payload",
        declared_type: "TEXT",
        not_null: true,
        default_value: None,
        primary_key_position: 0,
    },
    ExpectedColumn {
        name: "updated_at",
        declared_type: "INTEGER",
        not_null: true,
        default_value: None,
        primary_key_position: 0,
    },
];

const PODCAST_V1_COLUMNS: &[ExpectedColumn] = &[
    ExpectedColumn {
        name: "video_id",
        declared_type: "TEXT",
        not_null: false,
        default_value: None,
        primary_key_position: 1,
    },
    ExpectedColumn {
        name: "position_ms",
        declared_type: "INTEGER",
        not_null: true,
        default_value: None,
        primary_key_position: 0,
    },
    ExpectedColumn {
        name: "duration_ms",
        declared_type: "INTEGER",
        not_null: false,
        default_value: None,
        primary_key_position: 0,
    },
    ExpectedColumn {
        name: "played",
        declared_type: "INTEGER",
        not_null: true,
        default_value: Some("0"),
        primary_key_position: 0,
    },
    ExpectedColumn {
        name: "updated_at",
        declared_type: "INTEGER",
        not_null: true,
        default_value: None,
        primary_key_position: 0,
    },
];

const PODCAST_V2_COLUMNS: &[ExpectedColumn] = &[
    ExpectedColumn {
        name: "video_id",
        declared_type: "TEXT",
        not_null: false,
        default_value: None,
        primary_key_position: 1,
    },
    ExpectedColumn {
        name: "playback_epoch",
        declared_type: "INTEGER",
        not_null: true,
        default_value: Some("0"),
        primary_key_position: 0,
    },
    ExpectedColumn {
        name: "position_ms",
        declared_type: "INTEGER",
        not_null: true,
        default_value: None,
        primary_key_position: 0,
    },
    ExpectedColumn {
        name: "duration_ms",
        declared_type: "INTEGER",
        not_null: false,
        default_value: None,
        primary_key_position: 0,
    },
    ExpectedColumn {
        name: "played",
        declared_type: "INTEGER",
        not_null: true,
        default_value: Some("0"),
        primary_key_position: 0,
    },
    ExpectedColumn {
        name: "updated_at",
        declared_type: "INTEGER",
        not_null: true,
        default_value: None,
        primary_key_position: 0,
    },
];

const HISTORY_COLUMNS: &[ExpectedColumn] = &[
    ExpectedColumn {
        name: "id",
        declared_type: "INTEGER",
        not_null: false,
        default_value: None,
        primary_key_position: 1,
    },
    ExpectedColumn {
        name: "video_id",
        declared_type: "TEXT",
        not_null: true,
        default_value: None,
        primary_key_position: 0,
    },
    ExpectedColumn {
        name: "item_json",
        declared_type: "TEXT",
        not_null: true,
        default_value: None,
        primary_key_position: 0,
    },
    ExpectedColumn {
        name: "played_at",
        declared_type: "INTEGER",
        not_null: true,
        default_value: None,
        primary_key_position: 0,
    },
];

const CACHE_COLUMNS: &[ExpectedColumn] = &[
    ExpectedColumn {
        name: "cache_key",
        declared_type: "TEXT",
        not_null: false,
        default_value: None,
        primary_key_position: 1,
    },
    ExpectedColumn {
        name: "payload",
        declared_type: "TEXT",
        not_null: true,
        default_value: None,
        primary_key_position: 0,
    },
    ExpectedColumn {
        name: "expires_at",
        declared_type: "INTEGER",
        not_null: true,
        default_value: None,
        primary_key_position: 0,
    },
    ExpectedColumn {
        name: "stored_at",
        declared_type: "INTEGER",
        not_null: true,
        default_value: None,
        primary_key_position: 0,
    },
];

const FAVORITES_COLUMNS: &[ExpectedColumn] = &[
    ExpectedColumn {
        name: "id",
        declared_type: "INTEGER",
        not_null: false,
        default_value: None,
        primary_key_position: 1,
    },
    ExpectedColumn {
        name: "provider",
        declared_type: "TEXT",
        not_null: true,
        default_value: None,
        primary_key_position: 0,
    },
    ExpectedColumn {
        name: "video_id",
        declared_type: "TEXT",
        not_null: true,
        default_value: None,
        primary_key_position: 0,
    },
    ExpectedColumn {
        name: "item_json",
        declared_type: "TEXT",
        not_null: true,
        default_value: None,
        primary_key_position: 0,
    },
    ExpectedColumn {
        name: "favorited_at",
        declared_type: "INTEGER",
        not_null: true,
        default_value: None,
        primary_key_position: 0,
    },
];

pub(super) fn run(connection: &mut Connection, applied_at: i64) -> Result<(), StorageError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let migration_table_exists = transaction.query_row(
        "SELECT EXISTS(
            SELECT 1
            FROM sqlite_schema
            WHERE type = 'table' AND name = 'schema_migrations'
        )",
        [],
        |row| row.get::<_, bool>(0),
    )?;

    if migration_table_exists {
        validate_migration_table(&transaction)?;
        let versions = {
            let mut statement = transaction
                .prepare("SELECT version FROM schema_migrations ORDER BY version DESC")?;
            let rows = statement.query_map([], |row| row.get::<_, i64>(0))?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        if versions.is_empty() {
            return Err(StorageError::CorruptData {
                entity: "schema_migrations",
                reason: "migration ledger exists but contains no applied version".to_owned(),
            });
        }

        for &version in &versions {
            if version > LATEST_SCHEMA_VERSION {
                return Err(StorageError::UnsupportedSchemaVersion {
                    found: version,
                    supported: LATEST_SCHEMA_VERSION,
                });
            }
            if !matches!(version, 1..=3) {
                return Err(StorageError::InvalidSchemaVersion { found: version });
            }
        }
        let latest = versions[0];
        let expected_versions = (1..=latest).rev().collect::<Vec<_>>();
        if versions != expected_versions {
            return Err(StorageError::CorruptData {
                entity: "schema_migrations",
                reason: format!("migration ledger is not contiguous: {versions:?}"),
            });
        }
        match latest {
            1 => {
                validate_schema_v1(&transaction)?;
                apply_schema_v2(&transaction, applied_at)?;
                apply_schema_v3(&transaction, applied_at)?;
            }
            2 => {
                validate_schema_v2(&transaction)?;
                apply_schema_v3(&transaction, applied_at)?;
            }
            3 => validate_schema_v3(&transaction)?,
            _ => return Err(StorageError::InvalidSchemaVersion { found: latest }),
        }
    } else {
        transaction.execute_batch(SCHEMA_V1)?;
        transaction.execute(
            "INSERT INTO schema_migrations(version, applied_at) VALUES (?1, ?2)",
            params![1, applied_at],
        )?;
        validate_migration_table(&transaction)?;
        validate_schema_v1(&transaction)?;
        apply_schema_v2(&transaction, applied_at)?;
        apply_schema_v3(&transaction, applied_at)?;
    }

    transaction.commit()?;
    Ok(())
}

fn apply_schema_v2(connection: &Connection, applied_at: i64) -> Result<(), StorageError> {
    connection.execute_batch(SCHEMA_V2)?;
    connection.execute(
        "INSERT INTO schema_migrations(version, applied_at) VALUES (?1, ?2)",
        params![2, applied_at],
    )?;
    validate_schema_v2(connection)
}

fn apply_schema_v3(connection: &Connection, applied_at: i64) -> Result<(), StorageError> {
    connection.execute_batch(SCHEMA_V3)?;
    connection.execute(
        "INSERT INTO schema_migrations(version, applied_at) VALUES (?1, ?2)",
        params![3, applied_at],
    )?;
    validate_schema_v3(connection)
}

fn validate_migration_table(connection: &Connection) -> Result<(), StorageError> {
    validate_table(
        connection,
        "schema_migrations",
        MIGRATION_COLUMNS,
        "schema_migrations",
    )
}

fn validate_schema_v1(connection: &Connection) -> Result<(), StorageError> {
    validate_object_inventory(connection, "schema_v1", false)?;
    validate_table(connection, "session_state", SESSION_COLUMNS, "schema_v1")?;
    validate_table(
        connection,
        "podcast_progress",
        PODCAST_V1_COLUMNS,
        "schema_v1",
    )?;
    validate_table(
        connection,
        "listening_history",
        HISTORY_COLUMNS,
        "schema_v1",
    )?;
    validate_table(connection, "metadata_cache", CACHE_COLUMNS, "schema_v1")?;
    validate_history_index(connection, "schema_v1")
}

fn validate_schema_v2(connection: &Connection) -> Result<(), StorageError> {
    validate_object_inventory(connection, "schema_v2", false)?;
    validate_table(connection, "session_state", SESSION_COLUMNS, "schema_v2")?;
    validate_table(
        connection,
        "podcast_progress",
        PODCAST_V2_COLUMNS,
        "schema_v2",
    )?;
    validate_table(
        connection,
        "listening_history",
        HISTORY_COLUMNS,
        "schema_v2",
    )?;
    validate_table(connection, "metadata_cache", CACHE_COLUMNS, "schema_v2")?;
    validate_history_index(connection, "schema_v2")
}

fn validate_schema_v3(connection: &Connection) -> Result<(), StorageError> {
    validate_object_inventory(connection, "schema_v3", true)?;
    validate_table(connection, "session_state", SESSION_COLUMNS, "schema_v3")?;
    validate_table(
        connection,
        "podcast_progress",
        PODCAST_V2_COLUMNS,
        "schema_v3",
    )?;
    validate_table(
        connection,
        "listening_history",
        HISTORY_COLUMNS,
        "schema_v3",
    )?;
    validate_table(connection, "metadata_cache", CACHE_COLUMNS, "schema_v3")?;
    validate_table(connection, "favorites", FAVORITES_COLUMNS, "schema_v3")?;
    validate_history_index(connection, "schema_v3")?;
    validate_favorites_index(connection, "schema_v3")
}

fn validate_object_inventory(
    connection: &Connection,
    error_entity: &'static str,
    includes_favorites: bool,
) -> Result<(), StorageError> {
    const V2_EXPECTED: &[(&str, &str)] = &[
        ("index", "history_played_at"),
        ("table", "listening_history"),
        ("table", "metadata_cache"),
        ("table", "podcast_progress"),
        ("table", "schema_migrations"),
        ("table", "session_state"),
    ];
    const V3_EXPECTED: &[(&str, &str)] = &[
        ("index", "favorites_newest_first"),
        ("index", "history_played_at"),
        ("table", "favorites"),
        ("table", "listening_history"),
        ("table", "metadata_cache"),
        ("table", "podcast_progress"),
        ("table", "schema_migrations"),
        ("table", "session_state"),
    ];
    let expected = if includes_favorites {
        V3_EXPECTED
    } else {
        V2_EXPECTED
    };

    let mut statement = connection.prepare(
        "SELECT type, name
         FROM sqlite_schema
         WHERE name NOT LIKE 'sqlite_%'
         ORDER BY type, name",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let actual = rows.collect::<Result<Vec<_>, _>>()?;
    let matches = actual.len() == expected.len()
        && actual.iter().zip(expected).all(
            |((found_type, found_name), (expected_type, expected_name))| {
                found_type == expected_type && found_name == expected_name
            },
        );

    if !matches {
        return Err(StorageError::CorruptData {
            entity: error_entity,
            reason: format!("database has unexpected persistent objects: {actual:?}"),
        });
    }
    Ok(())
}

fn validate_favorites_index(
    connection: &Connection,
    error_entity: &'static str,
) -> Result<(), StorageError> {
    validate_object_definition(connection, "index", "favorites_newest_first", error_entity)?;

    let mut statement = connection.prepare("PRAGMA index_xinfo(favorites_newest_first)")?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, Option<String>>(2)?,
            row.get::<_, bool>(3)?,
            row.get::<_, bool>(5)?,
        ))
    })?;
    let key_columns = rows
        .filter_map(|row_result| match row_result {
            Ok((name, descending, true)) => Some(Ok((name, descending))),
            Ok((_, _, false)) => None,
            Err(error) => Some(Err(error)),
        })
        .collect::<Result<Vec<_>, _>>()?;

    if key_columns
        != [
            (Some("favorited_at".to_owned()), true),
            (Some("id".to_owned()), true),
        ]
    {
        return Err(StorageError::CorruptData {
            entity: error_entity,
            reason: format!(
                "index `favorites_newest_first` has unexpected key columns: {key_columns:?}"
            ),
        });
    }
    Ok(())
}

fn validate_table(
    connection: &Connection,
    table: &'static str,
    expected: &[ExpectedColumn],
    error_entity: &'static str,
) -> Result<(), StorageError> {
    let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
    let rows = statement.query_map([], |row| {
        Ok(ColumnShape {
            name: row.get(1)?,
            declared_type: row.get(2)?,
            not_null: row.get(3)?,
            default_value: row.get(4)?,
            primary_key_position: row.get(5)?,
        })
    })?;
    let actual = rows.collect::<Result<Vec<_>, _>>()?;
    let columns_match = actual.len() == expected.len()
        && actual.iter().zip(expected).all(|(found, wanted)| {
            found.name == wanted.name
                && found.declared_type == wanted.declared_type
                && found.not_null == wanted.not_null
                && found.default_value.as_deref() == wanted.default_value
                && found.primary_key_position == wanted.primary_key_position
        });

    if !columns_match {
        return Err(StorageError::CorruptData {
            entity: error_entity,
            reason: format!("table `{table}` has unexpected column shape: {actual:?}"),
        });
    }

    validate_object_definition(connection, "table", table, error_entity)
}

fn validate_history_index(
    connection: &Connection,
    error_entity: &'static str,
) -> Result<(), StorageError> {
    validate_object_definition(connection, "index", "history_played_at", error_entity)?;

    let mut statement = connection.prepare("PRAGMA index_xinfo(history_played_at)")?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, Option<String>>(2)?,
            row.get::<_, bool>(3)?,
            row.get::<_, bool>(5)?,
        ))
    })?;
    let key_columns = rows
        .filter_map(|row_result| match row_result {
            Ok((name, descending, true)) => Some(Ok((name, descending))),
            Ok((_, _, false)) => None,
            Err(error) => Some(Err(error)),
        })
        .collect::<Result<Vec<_>, _>>()?;

    if key_columns != [(Some("played_at".to_owned()), true)] {
        return Err(StorageError::CorruptData {
            entity: error_entity,
            reason: format!(
                "index `history_played_at` has unexpected key columns: {key_columns:?}"
            ),
        });
    }

    Ok(())
}

fn validate_object_definition(
    connection: &Connection,
    object_type: &'static str,
    name: &'static str,
    error_entity: &'static str,
) -> Result<(), StorageError> {
    let actual = connection
        .query_row(
            "SELECT sql
             FROM sqlite_schema
             WHERE type = ?1 AND name = ?2",
            params![object_type, name],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    let Some(actual) = actual else {
        return Err(StorageError::CorruptData {
            entity: error_entity,
            reason: format!("required {object_type} `{name}` is missing"),
        });
    };
    let expected =
        expected_object_definition(object_type, name, error_entity).ok_or_else(|| {
            StorageError::CorruptData {
                entity: error_entity,
                reason: format!("owned migration has no definition for {object_type} `{name}`"),
            }
        })?;

    if normalize_sql(&actual) != normalize_sql(expected) {
        return Err(StorageError::CorruptData {
            entity: error_entity,
            reason: format!("required {object_type} `{name}` has an unexpected definition"),
        });
    }
    Ok(())
}

fn expected_object_definition(
    object_type: &str,
    name: &str,
    schema_entity: &str,
) -> Option<&'static str> {
    let prefix = match object_type {
        "table" => format!("createtable{name}("),
        "index" => format!("createindex{name}on"),
        _ => return None,
    };
    let schema = if matches!(name, "favorites" | "favorites_newest_first") {
        SCHEMA_V3
    } else if object_type == "table"
        && name == "podcast_progress"
        && matches!(schema_entity, "schema_v2" | "schema_v3")
    {
        SCHEMA_V2
    } else {
        SCHEMA_V1
    };
    schema
        .split(';')
        .find(|statement| normalize_sql(statement).starts_with(&prefix))
}

fn normalize_sql(sql: &str) -> String {
    sql.chars()
        .filter(|character| !character.is_ascii_whitespace() && *character != ';')
        .map(|character| character.to_ascii_lowercase())
        .collect()
}
