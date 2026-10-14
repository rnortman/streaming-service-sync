//! streaming-service-sync: export a Pandora account to a local data
//! repository and migrate it to Spotify. The `sss` binary is a thin shell
//! over [`cli::run`].

pub mod cli;
pub mod config;
pub mod error;
pub mod input;
pub mod pandora;
pub mod profiles;
pub mod repo;
pub mod secrets;
pub mod status;
