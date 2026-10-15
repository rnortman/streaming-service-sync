//! Matching Pandora tracks, artists and albums to Spotify: the match
//! entries, string normalization, deterministic scoring, candidate
//! retrieval, the LLM matcher and the `match` commands.

pub mod candidates;
pub mod llm;
pub mod model;
pub mod normalize;
pub mod run;
pub mod score;
