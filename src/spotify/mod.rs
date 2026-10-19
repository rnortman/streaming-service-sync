//! Spotify: sign-in, the Web API client, the player endpoints, the
//! account reads (history, top items, playlists), the playlist writes,
//! the library reads and removal, Spotify ids and the shape every document
//! gives a track, artist or album.

pub mod account;
pub mod auth;
pub mod client;
pub mod edit;
pub mod ids;
pub mod item;
pub mod library;
pub mod playback;

#[cfg(test)]
pub(crate) mod test_support;
