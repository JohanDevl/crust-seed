//! Torrent files: bencode, metainfo, and the on-disk torrent cache.

pub mod bencode;
pub mod cache;
pub mod index;
pub mod lookup;
pub mod metafile;
pub mod snatch;

pub use metafile::{Metafile, MetafileError, TorrentMetadata, sanitize_tracker_url};
pub use snatch::{SnatchError, SnatchOptions, snatch};
