//! HTTP API（/reader3/*，兼容 legacy）

pub mod book_asset;
pub mod files;
pub mod opds;
pub mod range;
pub mod pro_export;
pub mod router;
pub mod webdav;

pub use router::router;
