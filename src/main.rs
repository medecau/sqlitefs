use clap::{command, crate_version, Arg};
use fuser::{Config, MountOption};
use sqlite_fs::db_module::sqlite::Sqlite;
use sqlite_fs::db_module::DbModule;
use sqlite_fs::filesystem::SqliteFs;

fn main() {
    env_logger::init();

    let mount_option_arg = Arg::new("mount_option")
        .short('o')
        .long("option")
        .help("Additional mount option for this filesystem")
        .value_parser(clap::builder::NonEmptyStringValueParser::new())
        .num_args(1..);

    let mount_point_arg = Arg::new("mount_point")
        .help("Target mountpoint path")
        .index(1)
        .required(true);

    let db_path_arg = Arg::new("db_path")
        .help("Sqlite database file path. If not set, open database in memory.")
        .index(2);

    let matches = command!()
        .about("Sqlite database as a filesystem.")
        .version(crate_version!())
        .arg(mount_option_arg)
        .arg(mount_point_arg)
        .arg(db_path_arg)
        .get_matches();

    let mut mount_options = vec![
        MountOption::FSName("sqlitefs".to_string()),
        MountOption::DefaultPermissions,
        MountOption::CUSTOM("allow_other".to_string()),
    ];
    if let Some(v) = matches.get_many::<String>("mount_option") {
        for i in v {
            mount_options.push(MountOption::CUSTOM(i.clone()));
        }
    }
    let mut config = Config::default();
    config.mount_options = mount_options;

    let mountpoint = matches
        .get_one::<String>("mount_point")
        .expect("Mount point path is missing.");
    let db_path = matches.get_one::<String>("db_path");
    let fs: SqliteFs = match db_path {
        Some(path) => match SqliteFs::new(path) {
            Ok(n) => n,
            Err(err) => {
                eprintln!("Error: {}", err);
                std::process::exit(1);
            }
        },
        None => {
            let mut db = match Sqlite::new_in_memory() {
                Ok(n) => n,
                Err(err) => {
                    eprintln!("Error: {}", err);
                    std::process::exit(1);
                }
            };
            match db.init() {
                Ok(n) => n,
                Err(err) => {
                    eprintln!("Error: {}", err);
                    std::process::exit(1);
                }
            };
            match SqliteFs::new_with_db(db) {
                Ok(n) => n,
                Err(err) => {
                    eprintln!("Error: {}", err);
                    std::process::exit(1);
                }
            }
        }
    };
    if let Err(err) = fuser::mount2(fs, mountpoint, &config) {
        eprintln!("Mount failed: {}", err);
        std::process::exit(1);
    }
}
