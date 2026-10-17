//! YouTube and YouTube Music: the Google Takeout import, the Data API's
//! video metadata, the normalized model, the music-or-video classification
//! and the items matched.

pub mod api;
pub mod classify;
pub mod enrich;
pub mod model;
pub mod report;
pub mod select;
pub mod takeout;
