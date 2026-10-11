use clap::{command, crate_version, Arg, ArgAction};
use fuser::{Config, MountOption};
use sqlite_fs::db_module::sqlite::Sqlite;
use sqlite_fs::db_module::DbModule;
use sqlite_fs::filesystem::SqliteFs;

fn main() {
    // warn by default: failed requests are logged at warn! with their cause.
    // fuser stays at error: it warns on every call to an op we leave unimplemented
    // (cp tries copy_file_range first) and after every external umount.
    // Millisecond timestamps show stalls between requests under RUST_LOG=debug.
    let filter = env_logger::Env::default().default_filter_or("warn,fuser=error");
    env_logger::Builder::from_env(filter)
        .format_timestamp_millis()
        .init();

    let mount_option_arg = Arg::new("mount_option")
        .short('o')
        .long("option")
        .help("Additional mount option for this filesystem (repeatable)")
        .value_parser(clap::builder::NonEmptyStringValueParser::new())
        .action(ArgAction::Append);

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
    let db_path = matches.get_one::<String>("db_path");
    // macFUSE calls an unnamed volume "macFUSE Volume N (sqlitefs)": name it after
    // the database file unless -o volname=... was given. Commas would split the option.
    #[cfg(target_os = "macos")]
    if !mount_options
        .iter()
        .any(|o| matches!(o, MountOption::CUSTOM(s) if s.starts_with("volname=")))
    {
        let name = db_path
            .and_then(|p| std::path::Path::new(p).file_stem())
            .map_or("sqlitefs".into(), |s| s.to_string_lossy().replace(',', "_"));
        mount_options.push(MountOption::CUSTOM(format!("volname={name}")));
    }
    let volume = mount_options
        .iter()
        .find_map(|o| match o {
            MountOption::CUSTOM(s) => s.strip_prefix("volname="),
            _ => None,
        })
        .map(|v| format!(" as volume \"{v}\""))
        .unwrap_or_default();
    let mut config = Config::default();
    config.mount_options = mount_options;

    let mountpoint = matches
        .get_one::<String>("mount_point")
        .expect("Mount point path is missing.");
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
    // Mount and serve separately, so a session that ends after mounting is not
    // reported as a mount failure, and a clean unmount is reported at all.
    let session = match fuser::spawn_mount2(fs, mountpoint, &config) {
        Ok(n) => n,
        Err(err) => {
            eprintln!("Mount failed: {}", err);
            std::process::exit(1);
        }
    };
    let db = db_path.map_or("in-memory database", |p| p.as_str());
    // SQLITEFS_COMMIT, if set at build time, tells copied binaries apart.
    let build = option_env!("SQLITEFS_COMMIT").unwrap_or("");
    eprintln!(
        "sqlite-fs {}{}: mounted {} on {}{}",
        crate_version!(),
        build,
        db,
        mountpoint,
        volume
    );
    match session.join() {
        Ok(()) => eprintln!("sqlite-fs: {} unmounted", mountpoint),
        Err(err) => {
            eprintln!("sqlite-fs: {} stopped: {}", mountpoint, err);
            std::process::exit(1);
        }
    }
}
