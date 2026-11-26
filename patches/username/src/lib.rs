//! Stand-in for `username` 0.2, used by `hdbconnect` to tell SAP HANA the client's OS user.
//! The original links `winapi` 0.2, which doesn't build for Windows on ARM64; the environment
//! has the same name.

#[derive(Debug)]
pub enum Error {
    IO(std::io::Error),
    Var(std::env::VarError),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Error::IO(e) => e.fmt(f),
            Error::Var(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::env::VarError> for Error {
    fn from(e: std::env::VarError) -> Error {
        Error::Var(e)
    }
}

/// The name of the user running the process.
pub fn get_user_name() -> Result<String, Error> {
    let var = if cfg!(windows) { "USERNAME" } else { "USER" };
    Ok(std::env::var(var)?)
}
