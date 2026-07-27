//! psxtui — a terminal client for Pakistan Stock Exchange market data.
//!
//! Layering:
//! - [`psx`] talks to the PSX data portal (JSON feeds + HTML scrapes)
//! - [`cache`] persists what it fetches to SQLite for fast, offline analysis
//! - [`analysis`] computes indicators and risk statistics over [`model::Bar`]s
//! - [`data`] runs those two off the render thread
//! - [`app`] holds all state; [`ui`] is a pure function of it

pub mod analysis;
pub mod app;
pub mod cache;
pub mod data;
pub mod ext;
pub mod model;
pub mod psx;
pub mod ui;
