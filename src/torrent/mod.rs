//! Torrent files: bencode, metainfo, and the on-disk torrent cache.

pub mod bencode;
pub mod metafile;

pub use metafile::{Metafile, MetafileError, TorrentMetadata, sanitize_tracker_url};
