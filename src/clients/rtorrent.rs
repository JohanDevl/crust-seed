//! rTorrent (XML-RPC).
//!
//! Ported from `clients/RTorrent.ts`.
//!
//! rTorrent has no bulk query endpoint: every field of every torrent is a
//! separate RPC call. `system.multicall` batches them, and the responses come
//! back as a flat array in call order — so the reader indexes by
//! `torrent_index * methods_per_torrent + field_offset`, and a length mismatch
//! is treated as a hard failure rather than silently misaligning fields.
//!
//! Injecting also differs from the other clients: rTorrent will not accept a
//! torrent as already-complete unless the `.torrent` carries a
//! `libtorrent_resume` structure describing which pieces exist, so crust-seed
//! synthesises one from the files on disk.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use async_trait::async_trait;

use super::xmlrpc::{XmlRpcResponse, XmlRpcValue, build_request, multicall_param, parse_response};
use super::{
    ClientSearcheeResult, ClientType, DownloadDirError, GetSearcheesOptions, InjectOptions,
    RESUME_ERR_SLEEP_MS, RESUME_SLEEP_MS, TorrentClient, TorrentMetadataInClient, Tracker,
    client_searchee_modified, get_max_remaining_bytes, get_resume_stop_time, organize_trackers,
    should_resume_from_non_relevant_files,
};
use crate::config::runtime::get_runtime_config;
use crate::constants::{Decision, InjectionResult, TORRENT_TAG};
use crate::db::{ClientSearcheeRow, db};
use crate::errors::CrustSeedError;
use crate::http::client;
use crate::searchee::{File, Searchee, parse_title, searchee_from_db_row};
use crate::torrent::Metafile;
use crate::torrent::bencode;
use crate::utils::{
    UrlCredentials, extract_credentials_from_url, human_readable_size, now_ms, sanitize_info_hash,
    wait,
};

const COULD_NOT_FIND_INFO_HASH: &str = "Could not find info-hash.";
/// rTorrent chokes on very large multicalls; the original batched at 500.
const BATCH_SIZE: usize = 500;

#[derive(Debug, Clone)]
struct OriginalTorrent {
    name: String,
    directory_base: String,
    bytes_left: i64,
    hashing: i64,
    is_multi_file: bool,
    is_active: bool,
    is_complete: bool,
}

pub struct RTorrent {
    url: UrlCredentials,
    client_host: String,
    client_priority: usize,
    readonly: bool,
    label: String,
}

impl RTorrent {
    pub fn new(
        url: &str,
        client_host: String,
        priority: usize,
        readonly: bool,
    ) -> Result<Self, CrustSeedError> {
        let label = format!("rtorrent@{client_host}");
        let credentials = extract_credentials_from_url(url, None).map_err(|_| {
            CrustSeedError::new(format!("[{label}] rTorrent url must be percent-encoded"))
        })?;
        Ok(RTorrent {
            url: credentials,
            client_host,
            client_priority: priority,
            readonly,
            label,
        })
    }

    async fn method_call(
        &self,
        method: &str,
        params: Vec<XmlRpcValue>,
    ) -> Result<XmlRpcValue, String> {
        let body = build_request(method, &params);
        let mut request = client()
            .post(&self.url.href)
            .header(reqwest::header::CONTENT_TYPE, "text/xml")
            .timeout(std::time::Duration::from_secs(300))
            .body(body);
        if !self.url.username.is_empty() || !self.url.password.is_empty() {
            request = request.basic_auth(&self.url.username, Some(&self.url.password));
        }

        let response = request.send().await.map_err(|e| e.to_string())?;
        let text = response.text().await.map_err(|e| e.to_string())?;
        match parse_response(&text)? {
            XmlRpcResponse::Value(value) => Ok(value),
            XmlRpcResponse::Fault(fault) => Err(fault.string),
        }
    }

    async fn download_list(&self) -> Result<Vec<String>, String> {
        let value = self.method_call("download_list", vec![]).await?;
        Ok(value
            .as_array()
            .unwrap_or_default()
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect())
    }

    /// Runs `system.multicall` over `hashes` in batches, returning the flat
    /// result array. `methods_per_hash` is checked against the response length
    /// so a partial response cannot be misread as shifted data.
    async fn multicall_per_hash(
        &self,
        hashes: &[String],
        build: impl Fn(&str) -> Vec<(&'static str, Vec<XmlRpcValue>)>,
        methods_per_hash: usize,
    ) -> Result<Vec<XmlRpcValue>, String> {
        let mut results: Vec<XmlRpcValue> = Vec::with_capacity(hashes.len() * methods_per_hash);
        for batch in hashes.chunks(BATCH_SIZE) {
            let calls: Vec<(&'static str, Vec<XmlRpcValue>)> =
                batch.iter().flat_map(|hash| build(hash)).collect();
            let value = self
                .method_call("system.multicall", vec![multicall_param(calls)])
                .await?;
            results.extend(value.as_array().unwrap_or_default().iter().cloned());
        }
        if results.len() != hashes.len() * methods_per_hash {
            return Err(format!(
                "Unexpected number of results: {} for {} hashes",
                results.len(),
                hashes.len()
            ));
        }
        Ok(results)
    }

    async fn check_original_torrent(
        &self,
        info_hash: &str,
    ) -> Result<OriginalTorrent, DownloadDirError> {
        // rTorrent matches info hashes case-sensitively, in upper case.
        let hash = info_hash.to_uppercase();
        let calls: Vec<(&str, Vec<XmlRpcValue>)> = [
            "d.name",
            "d.directory",
            "d.left_bytes",
            "d.hashing",
            "d.complete",
            "d.is_multi_file",
            "d.is_active",
        ]
        .into_iter()
        .map(|method| (method, vec![XmlRpcValue::Str(hash.clone())]))
        .collect();

        let response = self
            .method_call("system.multicall", vec![multicall_param(calls)])
            .await;
        let value = match response {
            Ok(value) => value,
            Err(message) if message == COULD_NOT_FIND_INFO_HASH => {
                return Err(DownloadDirError::NotFound);
            }
            Err(_) => return Err(DownloadDirError::UnknownError),
        };
        let items = value.as_array().ok_or(DownloadDirError::UnknownError)?;
        if items.len() < 7 {
            return Err(DownloadDirError::UnknownError);
        }
        // A per-call fault comes back as a struct in place of the value.
        if items[0].get("faultString").is_some() {
            let fault = items[0]
                .get("faultString")
                .and_then(XmlRpcValue::as_str)
                .unwrap_or_default();
            return Err(if fault == COULD_NOT_FIND_INFO_HASH {
                DownloadDirError::NotFound
            } else {
                DownloadDirError::UnknownError
            });
        }

        let scalar = |index: usize| items[index].unwrap_singleton();
        Ok(OriginalTorrent {
            name: scalar(0).as_str().unwrap_or_default().to_string(),
            directory_base: scalar(1).as_str().unwrap_or_default().to_string(),
            bytes_left: scalar(2).as_i64().unwrap_or(0),
            hashing: scalar(3).as_i64().unwrap_or(0),
            is_complete: scalar(4).as_i64().unwrap_or(0) != 0,
            is_multi_file: scalar(5).as_i64().unwrap_or(0) != 0,
            is_active: scalar(6).as_i64().unwrap_or(0) != 0,
        })
    }

    /// rTorrent's `d.custom1` holds a comma-separated, percent-encoded tag list.
    fn decode_tags(raw: &str) -> Vec<String> {
        if raw.is_empty() {
            return Vec::new();
        }
        percent_encoding::percent_decode_str(raw)
            .decode_utf8_lossy()
            .split(',')
            .map(|tag| tag.trim().to_string())
            .filter(|tag| !tag.is_empty())
            .collect()
    }
}

/// `Math.ceil(a / b)` on integers — `i64::div_ceil` is still unstable.
fn ceil_div(a: i64, b: i64) -> i64 {
    let b = b.max(1);
    (a + b - 1) / b
}

/// Builds the `libtorrent_resume` dictionary rTorrent needs to accept a torrent
/// as already downloaded.
///
/// A file is only listed when it exists on disk *and* its size matches; a
/// mismatch would make rTorrent claim complete pieces it does not have.
async fn create_libtorrent_resume_tree(meta: &Metafile, base_path: &Path) -> bencode::Value {
    let mut files = Vec::new();
    for file in &meta.files {
        // The torrent's paths include its own name as the first segment; the
        // base path already points at that directory.
        let relative: PathBuf = Path::new(&file.path).components().skip(1).collect();
        let resolved = base_path.join(&relative);

        let Ok(metadata) = tokio::fs::metadata(&resolved).await else {
            continue;
        };
        if !metadata.is_file() || metadata.len() as i64 != file.length {
            continue;
        }
        let mtime = metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        let mut entry = BTreeMap::new();
        entry.insert(
            b"completed".to_vec(),
            bencode::Value::Int(ceil_div(file.length, meta.piece_length)),
        );
        entry.insert(b"mtime".to_vec(), bencode::Value::Int(mtime));
        entry.insert(b"priority".to_vec(), bencode::Value::Int(1));
        files.push(bencode::Value::Dict(entry));
    }

    let mut resume = BTreeMap::new();
    resume.insert(
        b"bitfield".to_vec(),
        bencode::Value::Int(ceil_div(meta.length, meta.piece_length)),
    );
    resume.insert(b"files".to_vec(), bencode::Value::List(files));
    bencode::Value::Dict(resume)
}

/// Re-encodes a torrent with the resume data attached.
async fn encode_with_resume(meta: &Metafile, base_path: &Path) -> Vec<u8> {
    let resume = create_libtorrent_resume_tree(meta, base_path).await;
    match &meta.raw {
        bencode::Value::Dict(map) => {
            let mut with_resume = map.clone();
            with_resume.insert(b"libtorrent_resume".to_vec(), resume);
            bencode::encode(&bencode::Value::Dict(with_resume))
        }
        _ => meta.encode(),
    }
}

fn dirname(path: &str) -> String {
    Path::new(path)
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string())
}

fn basename(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string())
}

#[async_trait]
impl TorrentClient for RTorrent {
    fn client_host(&self) -> &str {
        &self.client_host
    }
    fn client_priority(&self) -> usize {
        self.client_priority
    }
    fn client_type(&self) -> ClientType {
        ClientType::RTorrent
    }
    fn readonly(&self) -> bool {
        self.readonly
    }
    fn label(&self) -> &str {
        &self.label
    }

    async fn is_torrent_in_client(&self, info_hash: &str) -> Result<bool, String> {
        let needle = info_hash.to_lowercase();
        Ok(self
            .download_list()
            .await?
            .iter()
            .any(|hash| hash.to_lowercase() == needle))
    }

    async fn is_torrent_complete(&self, info_hash: &str) -> Result<bool, DownloadDirError> {
        match self
            .method_call("d.complete", vec![XmlRpcValue::Str(info_hash.to_string())])
            .await
        {
            Ok(value) => Ok(value.unwrap_singleton().as_i64().unwrap_or(0) != 0),
            Err(_) => Err(DownloadDirError::NotFound),
        }
    }

    async fn is_torrent_checking(&self, info_hash: &str) -> Result<bool, DownloadDirError> {
        match self
            .method_call("d.hashing", vec![XmlRpcValue::Str(info_hash.to_string())])
            .await
        {
            Ok(value) => Ok(value.unwrap_singleton().as_i64().unwrap_or(0) != 0),
            Err(_) => Err(DownloadDirError::NotFound),
        }
    }

    async fn get_all_torrents(&self) -> Result<Vec<TorrentMetadataInClient>, String> {
        let hashes = self.download_list().await?;
        if hashes.is_empty() {
            return Ok(Vec::new());
        }
        let results = self
            .multicall_per_hash(
                &hashes,
                |hash| vec![("d.custom1", vec![XmlRpcValue::Str(hash.to_string())])],
                1,
            )
            .await?;

        Ok(hashes
            .iter()
            .enumerate()
            .map(|(index, hash)| TorrentMetadataInClient {
                info_hash: hash.to_lowercase(),
                tags: Some(RTorrent::decode_tags(
                    results[index]
                        .unwrap_singleton()
                        .as_str()
                        .unwrap_or_default(),
                )),
                ..Default::default()
            })
            .collect())
    }

    async fn get_client_searchees(
        &self,
        options: GetSearcheesOptions,
    ) -> Result<ClientSearcheeResult, String> {
        let mut result = ClientSearcheeResult::default();
        let hashes = self.download_list().await?;
        if hashes.is_empty() {
            tracing::debug!(label = self.label.as_str(), "No torrents found in client");
            return Ok(result);
        }

        const METHODS_PER_HASH: usize = 7;
        let results = match self
            .multicall_per_hash(
                &hashes,
                |hash| {
                    let h = XmlRpcValue::Str(hash.to_string());
                    vec![
                        ("d.name", vec![h.clone()]),
                        ("d.size_bytes", vec![h.clone()]),
                        ("d.directory", vec![h.clone()]),
                        ("d.is_multi_file", vec![h.clone()]),
                        ("d.custom1", vec![h.clone()]),
                        (
                            "f.multicall",
                            vec![
                                h.clone(),
                                XmlRpcValue::Str(String::new()),
                                XmlRpcValue::Str("f.path=".into()),
                                XmlRpcValue::Str("f.size_bytes=".into()),
                            ],
                        ),
                        (
                            "t.multicall",
                            vec![
                                h,
                                XmlRpcValue::Str(String::new()),
                                XmlRpcValue::Str("t.url=".into()),
                                XmlRpcValue::Str("t.group=".into()),
                            ],
                        ),
                    ]
                },
                METHODS_PER_HASH,
            )
            .await
        {
            Ok(results) => results,
            Err(message) => {
                tracing::error!(
                    label = self.label.as_str(),
                    "Failed to get client torrents: {message}"
                );
                return Ok(result);
            }
        };

        let mut info_hashes: HashSet<String> = HashSet::new();
        for (index, hash) in hashes.iter().enumerate() {
            let info_hash = hash.to_lowercase();
            info_hashes.insert(info_hash.clone());
            let at = |offset: usize| results[index * METHODS_PER_HASH + offset].unwrap_singleton();

            let name = at(0).as_str().unwrap_or_default().to_string();
            let length = at(1).as_i64().unwrap_or(0);
            let directory = at(2).as_str().unwrap_or_default().to_string();
            let is_multi_file = at(3).as_i64().unwrap_or(0) != 0;
            let tags = RTorrent::decode_tags(at(4).as_str().unwrap_or_default());
            // A multi-file torrent's `d.directory` is its own root, so the save
            // path is one level up; a single-file torrent's already is.
            let save_path = if is_multi_file {
                dirname(&directory)
            } else {
                directory.clone()
            };

            let db_torrent: Option<ClientSearcheeRow> = sqlx::query_as(
                "SELECT * FROM client_searchee WHERE info_hash = ? AND client_host = ?",
            )
            .bind(&info_hash)
            .bind(&self.client_host)
            .fetch_optional(db())
            .await
            .ok()
            .flatten();

            let modified =
                client_searchee_modified(db_torrent.as_ref(), &name, &save_path, None, &tags);
            let refresh = match &options.refresh {
                None => false,
                Some(list) if list.is_empty() => true,
                Some(list) => list.contains(&info_hash),
            };
            if !modified && !refresh {
                if !options.new_searchees_only
                    && let Some(row) = &db_torrent
                {
                    result.searchees.push(searchee_from_db_row(row));
                }
                continue;
            }

            let files: Vec<File> = results[index * METHODS_PER_HASH + 5]
                .unwrap_singleton()
                .as_array()
                .unwrap_or_default()
                .iter()
                .filter_map(|entry| {
                    let fields = entry.as_array()?;
                    let path = fields.first()?.as_str()?.to_string();
                    let size = fields.get(1)?.as_i64().unwrap_or(0);
                    Some(File {
                        name: basename(&path),
                        path: if is_multi_file {
                            format!("{}/{path}", basename(&directory))
                        } else {
                            path
                        },
                        length: size,
                    })
                })
                .collect();
            if files.is_empty() {
                tracing::debug!(
                    label = self.label.as_str(),
                    "No files found for {name} [{}]: skipping",
                    sanitize_info_hash(&info_hash)
                );
                continue;
            }

            let trackers = organize_trackers(
                &results[index * METHODS_PER_HASH + 6]
                    .unwrap_singleton()
                    .as_array()
                    .unwrap_or_default()
                    .iter()
                    .filter_map(|entry| {
                        let fields = entry.as_array()?;
                        Some(Tracker {
                            url: fields.first()?.as_str()?.to_string(),
                            tier: fields.get(1).and_then(XmlRpcValue::as_i64).unwrap_or(0),
                        })
                    })
                    .collect::<Vec<_>>(),
            );

            let title = parse_title(&name, &files, None).unwrap_or_else(|| name.clone());
            let searchee = Searchee {
                info_hash: Some(info_hash),
                name: name.clone(),
                title,
                files,
                length,
                client_host: Some(self.client_host.clone()),
                save_path: Some(save_path),
                tags: Some(tags),
                trackers: Some(trackers),
                ..Default::default()
            };
            result.new_searchees.push(searchee.clone());
            result.searchees.push(searchee);
        }

        super::persist_client_searchees(&self.client_host, &result.new_searchees, &info_hashes)
            .await
            .map_err(|e| e.to_string())?;
        Ok(result)
    }

    async fn get_download_dir(
        &self,
        meta: &Searchee,
        only_completed: bool,
    ) -> Result<String, DownloadDirError> {
        let info_hash = meta
            .info_hash
            .as_deref()
            .ok_or(DownloadDirError::NotFound)?;
        if !self
            .is_torrent_in_client(info_hash)
            .await
            .map_err(|_| DownloadDirError::UnknownError)?
        {
            return Err(DownloadDirError::NotFound);
        }
        let torrent = self.check_original_torrent(info_hash).await?;
        if only_completed && !torrent.is_complete {
            return Err(DownloadDirError::TorrentNotComplete);
        }
        Ok(if torrent.is_multi_file {
            dirname(&torrent.directory_base)
        } else {
            torrent.directory_base
        })
    }

    async fn get_all_download_dirs(
        &self,
        _metas: &[Searchee],
        only_completed: bool,
        _v1_hash_only: bool,
    ) -> Result<HashMap<String, String>, String> {
        let hashes = self.download_list().await?;
        if hashes.is_empty() {
            return Ok(HashMap::new());
        }
        const METHODS_PER_HASH: usize = 3;
        let results = self
            .multicall_per_hash(
                &hashes,
                |hash| {
                    let h = XmlRpcValue::Str(hash.to_string());
                    vec![
                        ("d.directory", vec![h.clone()]),
                        ("d.is_multi_file", vec![h.clone()]),
                        ("d.complete", vec![h]),
                    ]
                },
                METHODS_PER_HASH,
            )
            .await?;

        let mut dirs = HashMap::new();
        for (index, hash) in hashes.iter().enumerate() {
            let at = |offset: usize| results[index * METHODS_PER_HASH + offset].unwrap_singleton();
            let directory = at(0).as_str().unwrap_or_default().to_string();
            let is_multi_file = at(1).as_i64().unwrap_or(0) != 0;
            let is_complete = at(2).as_i64().unwrap_or(0) != 0;
            if only_completed && !is_complete {
                continue;
            }
            dirs.insert(
                hash.clone(),
                if is_multi_file {
                    dirname(&directory)
                } else {
                    directory
                },
            );
        }
        Ok(dirs)
    }

    async fn recheck_torrent(&self, info_hash: &str) -> Result<(), String> {
        // Pause first: rTorrent may resume automatically after a hash check.
        let _ = self
            .method_call("d.pause", vec![XmlRpcValue::Str(info_hash.to_string())])
            .await;
        let _ = self
            .method_call(
                "d.check_hash",
                vec![XmlRpcValue::Str(info_hash.to_string())],
            )
            .await;
        Ok(())
    }

    async fn resume_injection(&self, meta: &Metafile, decision: Decision, check_once: bool) {
        let config = get_runtime_config();
        let info_hash = &meta.info_hash;
        let mut sleep_time = RESUME_SLEEP_MS;
        let stop_time = get_resume_stop_time();
        let mut stop = false;

        while now_ms() < stop_time {
            if check_once {
                if stop {
                    return;
                }
                stop = true;
            }
            wait(sleep_time).await;

            let Ok(torrent) = self.check_original_torrent(info_hash).await else {
                sleep_time = RESUME_ERR_SLEEP_MS;
                continue;
            };
            if torrent.hashing != 0 {
                continue;
            }
            let torrent_log = format!("{} [{}]", torrent.name, sanitize_info_hash(info_hash));
            if torrent.is_active {
                tracing::warn!(
                    label = self.label.as_str(),
                    "Will not resume torrent {torrent_log}: active"
                );
                return;
            }

            let max_remaining =
                get_max_remaining_bytes(meta, decision, &config, Some((&torrent_log, &self.label)));
            if torrent.bytes_left > max_remaining
                && !should_resume_from_non_relevant_files(
                    meta,
                    torrent.bytes_left,
                    decision,
                    &config,
                    Some((&torrent_log, &self.label)),
                )
            {
                tracing::warn!(
                    label = self.label.as_str(),
                    "autoResumeMaxDownload will not resume {torrent_log}: remainingSize {} > {} limit",
                    human_readable_size(torrent.bytes_left, true),
                    human_readable_size(max_remaining, true)
                );
                return;
            }

            tracing::info!(
                label = self.label.as_str(),
                "Resuming torrent {torrent_log}"
            );
            let _ = self
                .method_call("d.resume", vec![XmlRpcValue::Str(info_hash.clone())])
                .await;
            return;
        }

        tracing::warn!(
            label = self.label.as_str(),
            "Will not resume torrent {info_hash}: timeout"
        );
    }

    async fn inject(
        &self,
        new_torrent: &Metafile,
        searchee: &Searchee,
        decision: Decision,
        options: InjectOptions,
    ) -> InjectionResult {
        let config = get_runtime_config();

        match self.is_torrent_in_client(&new_torrent.info_hash).await {
            Err(_) => return InjectionResult::Failure,
            Ok(true) => return InjectionResult::AlreadyExists,
            Ok(false) => {}
        }

        // `directory_base` is what rTorrent is told; `base_path` is where the
        // data actually is, used to build the resume tree.
        let (directory_base, base_path) = match &options.destination_dir {
            Some(destination_dir) => {
                let base_path = Path::new(destination_dir).join(&new_torrent.name);
                let directory_base = if new_torrent.is_single_file_torrent {
                    PathBuf::from(destination_dir)
                } else {
                    base_path.clone()
                };
                (directory_base, base_path)
            }
            None => {
                let Some(info_hash) = searchee.info_hash.as_deref() else {
                    return InjectionResult::Failure;
                };
                match self.check_original_torrent(info_hash).await {
                    Err(DownloadDirError::TorrentNotComplete) => {
                        return InjectionResult::TorrentNotComplete;
                    }
                    Err(_) => return InjectionResult::Failure,
                    Ok(torrent) => {
                        if options.only_completed && !torrent.is_complete {
                            return InjectionResult::TorrentNotComplete;
                        }
                        let directory_base = PathBuf::from(&torrent.directory_base);
                        let base_path = if new_torrent.is_single_file_torrent {
                            directory_base.join(&searchee.name)
                        } else {
                            directory_base.clone()
                        };
                        (directory_base, base_path)
                    }
                }
            }
        };

        let payload = encode_with_resume(new_torrent, &base_path).await;
        let to_recheck = super::should_recheck(new_torrent, decision, &config);
        let load_method = if to_recheck {
            "load.raw"
        } else {
            "load.raw_start"
        };

        let mut params = vec![
            XmlRpcValue::Str(String::new()),
            XmlRpcValue::Base64(payload),
            XmlRpcValue::Str(format!(
                "d.directory_base.set=\"{}\"",
                directory_base.to_string_lossy()
            )),
            XmlRpcValue::Str(format!("d.custom1.set=\"{TORRENT_TAG}\"")),
            XmlRpcValue::Str(format!("d.custom.set=addtime,{}", now_ms() / 1000)),
        ];
        if to_recheck {
            params.push(XmlRpcValue::Str(format!(
                "d.check_hash={}",
                new_torrent.info_hash.to_uppercase()
            )));
        }

        for attempt in 0..5u32 {
            match self.method_call(load_method, params.clone()).await {
                Ok(_) => {
                    if to_recheck {
                        self.resume_injection(new_torrent, decision, false).await;
                    }
                    break;
                }
                Err(message) => {
                    tracing::debug!(
                        label = self.label.as_str(),
                        "Failed to inject torrent {} on attempt {}/5: {message}",
                        new_torrent.name,
                        attempt + 1
                    );
                    wait(1000u64 << attempt).await;
                }
            }
        }

        // rTorrent's load is asynchronous; poll until the torrent appears.
        for attempt in 0..5u32 {
            if self
                .is_torrent_in_client(&new_torrent.info_hash)
                .await
                .unwrap_or(false)
            {
                return InjectionResult::Success;
            }
            wait(100u64 << attempt).await;
        }
        InjectionResult::Failure
    }

    async fn validate_config(&self) -> Result<(), CrustSeedError> {
        let config = get_runtime_config();
        self.download_list().await.map_err(|e| {
            CrustSeedError::new(format!(
                "[{}] Failed to reach rTorrent at {}: {e}",
                self.label, self.client_host
            ))
        })?;
        tracing::info!(
            label = self.label.as_str(),
            "Logged in successfully{}",
            if self.readonly { " (readonly)" } else { "" }
        );

        let Some(torrent_dir) = &config.torrent_dir else {
            return Ok(());
        };
        let mut entries = tokio::fs::read_dir(torrent_dir)
            .await
            .map_err(|e| CrustSeedError::new(format!("[{}] {torrent_dir}: {e}", self.label)))?;
        while let Ok(Some(entry)) = entries.next_entry().await {
            if entry.file_name().to_string_lossy().ends_with("_resume") {
                return Ok(());
            }
        }
        Err(CrustSeedError::new(format!(
            "[{}] Invalid torrentDir, if no torrents are in client set to null for now",
            self.label
        )))
    }
}

impl RTorrent {
    /// Lighter-weight probe used by the web UI's "test connection" button.
    pub async fn validate_connection(&self) -> Result<(), CrustSeedError> {
        self.method_call("session.name", vec![])
            .await
            .map_err(|e| {
                CrustSeedError::new(format!(
                    "[{}] Failed to reach rTorrent at {}: {e}",
                    self.label, self.client_host
                ))
            })?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::torrent::metafile::fixtures::multi_file_torrent;

    fn client() -> RTorrent {
        RTorrent::new(
            "http://user:pass@localhost/RPC2",
            "localhost".into(),
            0,
            false,
        )
        .unwrap()
    }

    #[test]
    fn credentials_are_split_from_the_rpc_url() {
        let r = client();
        assert_eq!(r.url.href, "http://localhost/RPC2");
        assert_eq!(r.url.username, "user");
        assert_eq!(r.label, "rtorrent@localhost");
    }

    #[test]
    fn tags_are_percent_decoded_and_split() {
        assert_eq!(RTorrent::decode_tags(""), Vec::<String>::new());
        assert_eq!(
            RTorrent::decode_tags("cross-seed,my%20label"),
            vec!["cross-seed", "my label"]
        );
    }

    #[test]
    fn multi_file_save_paths_are_one_level_above_the_directory() {
        assert_eq!(dirname("/downloads/Pack"), "/downloads");
        assert_eq!(basename("/downloads/Pack"), "Pack");
    }

    /// A resume entry is only emitted for a file that exists at the right size;
    /// otherwise rTorrent would claim pieces it does not have.
    #[tokio::test]
    async fn resume_tree_only_lists_files_that_exist_at_the_right_size() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("Pack");
        tokio::fs::create_dir_all(&base).await.unwrap();
        tokio::fs::write(base.join("a.mkv"), vec![0u8; 100])
            .await
            .unwrap();
        // b.mkv is written at the WRONG size; c.mkv is missing entirely.
        tokio::fs::write(base.join("b.mkv"), vec![0u8; 5])
            .await
            .unwrap();

        let meta = Metafile::decode(&multi_file_torrent(
            "Pack",
            &[(&["a.mkv"], 100), (&["b.mkv"], 200), (&["c.mkv"], 300)],
        ))
        .unwrap();

        let resume = create_libtorrent_resume_tree(&meta, &base).await;
        let files = resume.get("files").unwrap().as_list().unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].get("priority").unwrap().as_int(), Some(1));
    }

    #[tokio::test]
    async fn encoding_with_resume_adds_the_key_to_the_root_dict() {
        let dir = tempfile::tempdir().unwrap();
        let meta = Metafile::decode(&multi_file_torrent("Pack", &[(&["a.mkv"], 100)])).unwrap();
        let encoded = encode_with_resume(&meta, dir.path()).await;
        let decoded = bencode::decode(&encoded).unwrap();
        assert!(decoded.get("libtorrent_resume").is_some());
        // The info dict must survive untouched, or the info hash changes.
        assert_eq!(decoded.get("info"), meta.raw.get("info"));
    }
}
