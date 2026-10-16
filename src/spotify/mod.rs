//! Spotify: sign-in, the Web API client, the player endpoints, Spotify ids
//! and the shape every document gives a track, artist or album.

pub mod auth;
pub mod client;
pub mod ids;
pub mod item;
pub mod playback;

#[cfg(test)]
pub(crate) mod test_support;
