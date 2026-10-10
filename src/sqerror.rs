pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("A sqlite error occurred: {description}")]
    SqliteError { description: String },
    #[error("Target is directory: {description}")]
    FsIsDir { description: String },
    #[error("Target is not directory: {description}")]
    FsIsNotDir { description: String },
    #[error("Target is not found: {description}")]
    FsNoEnt { description: String },
    #[error("Target is not empty: {description}")]
    FsNotEmpty { description: String },
    #[error("Target file already exists: {description}")]
    FsFileExist { description: String },
    #[error("Invalid argument: {description}")]
    FsParm { description: String },
    #[error("File too large: {description}")]
    FsFileTooBig { description: String },
    #[error("Undefined error: {description}")]
    Undefined { description: String },
}

impl From<rusqlite::Error> for Error {
    fn from(err: rusqlite::Error) -> Error {
        Error::SqliteError {
            description: format!("{err} {err:?}"),
        }
    }
}

impl Error {
    pub fn to_errno(&self) -> fuser::Errno {
        match self {
            Error::SqliteError { .. } => fuser::Errno::EIO,
            Error::FsIsDir { .. } => fuser::Errno::EISDIR,
            Error::FsIsNotDir { .. } => fuser::Errno::ENOTDIR,
            Error::FsNoEnt { .. } => fuser::Errno::ENOENT,
            Error::FsNotEmpty { .. } => fuser::Errno::ENOTEMPTY,
            Error::FsFileExist { .. } => fuser::Errno::EEXIST,
            Error::FsParm { .. } => fuser::Errno::EPERM,
            Error::FsFileTooBig { .. } => fuser::Errno::EFBIG,
            Error::Undefined { .. } => fuser::Errno::EIO,
        }
    }
}
