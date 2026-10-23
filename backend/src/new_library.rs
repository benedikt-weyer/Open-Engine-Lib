//! Creating an empty Engine library, for "sync to" a drive or folder that has none.
//!
//! Engine's database schema is not written down anywhere here, so a new library gets the
//! schema of the open one: every table, index, view and trigger is created again from the
//! `sqlite_master` of its databases (`m.db`, `hm.db`, `sm.db`, `stm.db`), without any
//! rows but a new `Information` row with a UUID of its own. Whether Engine DJ and the
//! players accept a library made this way has not been tried on real hardware.

use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};

use crate::cache::DATABASE_DIR;
use crate::library::new_uuid;

/// The folder an Engine library lives in.
pub const LIBRARY_DIR: &str = "Engine Library";
/// The databases a library has, when the library the schema is taken from has them.
const DATABASES: [&str; 4] = ["m.db", "hm.db", "sm.db", "stm.db"];

/// The `Engine Library` folder a new library at `at` gets: `at` itself if it is called
/// that, else a folder of that name in it.
pub fn library_dir_for(at: &Path) -> PathBuf {
    if at.file_name().is_some_and(|name| name == LIBRARY_DIR) {
        at.to_path_buf()
    } else {
        at.join(LIBRARY_DIR)
    }
}

/// Makes empty versions of the databases in `from` (the `Database2` folder of a library)
/// in the `Database2` folder of `to`, which is made. Returns the names of the databases
/// that were made.
pub fn create_empty_databases(from: &Path, to: &Path) -> Result<Vec<String>, String> {
    let target = to.join(DATABASE_DIR);
    fs::create_dir_all(&target).map_err(|error| format!("{}: {error}", target.display()))?;
    let mut made = Vec::new();
    for name in DATABASES {
        if !from.join(name).is_file() {
            continue;
        }
        empty_copy(&from.join(name), &target.join(name))
            .map_err(|error| format!("could not create {name}: {error}"))?;
        made.push(name.to_owned());
    }
    if made.is_empty() {
        return Err("the open library has no databases to take the schema from".into());
    }
    Ok(made)
}

/// A database with the schema of `from` and one `Information` row.
fn empty_copy(from: &Path, to: &Path) -> rusqlite::Result<()> {
    let _ = fs::remove_file(to);
    let source = Connection::open_with_flags(
        from,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let new = Connection::open(to)?;

    // Tables first, then what refers to them. (AUTOINCREMENT makes `sqlite_sequence`.)
    for kind in ["table", "index", "view", "trigger"] {
        let mut statement = source.prepare(
            "SELECT sql FROM sqlite_master WHERE type = ?1 AND sql IS NOT NULL \
             AND name NOT LIKE 'sqlite_%' ORDER BY rowid",
        )?;
        let statements: Vec<String> = statement
            .query_map([kind], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        for sql in statements {
            new.execute_batch(&sql)?;
        }
    }
    let version: i64 = source.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    new.execute_batch(&format!("PRAGMA user_version = {version};"))?;

    // The library's own identity: the row of the source, with a UUID of its own.
    let columns: Vec<String> = {
        let mut statement = new.prepare("SELECT name FROM pragma_table_info('Information')")?;
        statement
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?
    };
    if !columns.is_empty() {
        let uuid = new_uuid(&new)?;
        let names = columns.join(", ");
        let values: Vec<String> = columns
            .iter()
            .map(|name| match name.as_str() {
                "id" => "1".to_owned(),
                "uuid" => "?1".to_owned(),
                // What the source has for the versions of the schema, and nothing for the
                // counters that only mean something to the library they belong to.
                "currentPlayedIndiciator" => "abs(random() % 9007199254740991)".to_owned(),
                "lastRekordBoxLibraryImportReadCounter" => "NULL".to_owned(),
                other => format!("(SELECT {other} FROM src.Information LIMIT 1)"),
            })
            .collect();
        new.execute("ATTACH DATABASE ?1 AS src", [from.to_string_lossy()])?;
        new.execute(
            &format!(
                "INSERT INTO Information ({names}) VALUES ({})",
                values.join(", ")
            ),
            [uuid],
        )?;
        new.execute("DETACH DATABASE src", [])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_library_goes_in_an_engine_library_folder() {
        assert_eq!(
            library_dir_for(Path::new("/media/STICK")),
            Path::new("/media/STICK/Engine Library")
        );
        assert_eq!(
            library_dir_for(Path::new("/media/STICK/Engine Library")),
            Path::new("/media/STICK/Engine Library")
        );
    }

    #[test]
    fn the_schema_is_copied_and_the_rows_are_not() {
        let folder = std::env::temp_dir().join(format!(
            "open-engine-lib-new-library-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&folder);
        let from = folder.join("from");
        fs::create_dir_all(&from).unwrap();
        let source = Connection::open(from.join("m.db")).unwrap();
        source
            .execute_batch(
                "CREATE TABLE Information (id INTEGER PRIMARY KEY AUTOINCREMENT, uuid TEXT,
                    schemaVersionMajor INTEGER, schemaVersionMinor INTEGER, schemaVersionPatch INTEGER,
                    currentPlayedIndiciator INTEGER, lastRekordBoxLibraryImportReadCounter INTEGER);
                 INSERT INTO Information VALUES (1, 'old-uuid', 3, 0, 2, 7, 9);
                 CREATE TABLE Track (id INTEGER PRIMARY KEY AUTOINCREMENT, title TEXT UNIQUE, rating INTEGER);
                 CREATE TABLE PerformanceData (trackId INTEGER PRIMARY KEY, beatData BLOB);
                 CREATE INDEX index_Track_title ON Track(title);
                 CREATE VIEW Titles AS SELECT title FROM Track;
                 CREATE TRIGGER add_performance AFTER INSERT ON Track
                    BEGIN INSERT INTO PerformanceData(trackId) VALUES (NEW.id); END;
                 INSERT INTO Track (title, rating) VALUES ('One', 60), ('Two', 80);
                 PRAGMA user_version = 4194305;",
            )
            .unwrap();
        drop(source);
        // A history database too, and no sm.db.
        Connection::open(from.join("hm.db"))
            .unwrap()
            .execute_batch(
                "CREATE TABLE Information (id INTEGER PRIMARY KEY AUTOINCREMENT, uuid TEXT,
                    schemaVersionMajor INTEGER, schemaVersionMinor INTEGER, schemaVersionPatch INTEGER);
                 INSERT INTO Information VALUES (1, 'old-history', 3, 0, 2);
                 CREATE TABLE Historylist (id INTEGER PRIMARY KEY, title TEXT);
                 INSERT INTO Historylist VALUES (1, 'A night');",
            )
            .unwrap();

        let made = create_empty_databases(&from, &folder.join("to")).unwrap();
        assert_eq!(made, ["m.db", "hm.db"]);

        let new = Connection::open(folder.join("to/Database2/m.db")).unwrap();
        let count = |sql: &str| -> i64 { new.query_row(sql, [], |row| row.get(0)).unwrap() };
        assert_eq!(count("SELECT count(*) FROM Track"), 0);
        assert_eq!(count("SELECT count(*) FROM PerformanceData"), 0);
        assert_eq!(count("PRAGMA user_version"), 4194305);
        assert_eq!(
            count(
                "SELECT count(*) FROM sqlite_master WHERE type = 'index' AND name = 'index_Track_title'"
            ),
            1
        );
        assert_eq!(
            count("SELECT count(*) FROM sqlite_master WHERE type = 'view' AND name = 'Titles'"),
            1
        );
        // Its own identity, with the versions of the schema.
        let (uuid, major, minor, patch, counter): (String, i64, i64, i64, Option<i64>) = new
            .query_row(
                "SELECT uuid, schemaVersionMajor, schemaVersionMinor, schemaVersionPatch, \
                        lastRekordBoxLibraryImportReadCounter FROM Information",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .unwrap();
        assert_ne!(uuid, "old-uuid");
        assert_eq!(uuid.len(), 36);
        assert_eq!((major, minor, patch, counter), (3, 0, 2, None));
        // The triggers work, and ids start at 1.
        new.execute("INSERT INTO Track (title) VALUES ('New')", [])
            .unwrap();
        assert_eq!(count("SELECT id FROM Track"), 1);
        assert_eq!(count("SELECT count(*) FROM PerformanceData"), 1);

        // The source is as it was.
        let source = Connection::open(from.join("m.db")).unwrap();
        let rows: i64 = source
            .query_row("SELECT count(*) FROM Track", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 2);

        assert!(create_empty_databases(&folder.join("nowhere"), &folder.join("x")).is_err());
        let _ = fs::remove_dir_all(&folder);
    }
}
