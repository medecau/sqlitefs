use sqlite_fs::db_module::{sqlite, DbModule};

mod helpers;
#[test]
fn sqlite_create_db() {
    sqlite::Sqlite::new_in_memory().expect("failed to create db in memory");
}

#[test]
fn sqlite_create_db_file() {
    let mut dbf = helpers::DBWithTempFile::new();
    dbf.db.init().expect("failed to create db file");
}

#[test]
fn sqlite_init_db() {
    let mut db = sqlite::Sqlite::new_in_memory().expect("failed to create db in memory");
    db.init().expect("failed to init db");
}
