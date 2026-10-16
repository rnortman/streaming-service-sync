//! streaming-service-sync: export a Pandora account to a local data
//! repository and migrate it to Spotify. The `sss` binary is a thin shell
//! over [`cli::run`].

pub mod cli;
pub mod config;
pub mod error;
pub mod guide;
pub mod http;
pub mod input;
pub mod lazy;
pub mod matching;
pub mod pandora;
pub mod player;
pub mod profiles;
pub mod push;
pub mod repo;
pub mod report;
pub mod search;
pub mod secrets;
pub mod spotify;
pub mod status;
