// Copyright 2026 Tree xie.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use super::{
    async_connection::{
        AuthSource, RedisAsyncConn, clear_pool_connections_batch, is_auth_error, open_single_connection,
        query_async_masters, try_open_with_preset_credentials,
    },
    config::{RedisServer, get_config},
    ssh_cluster_connection::SshMultiplexedConnection,
};
use crate::error::Error;
use crate::states::PresetCredential;
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use chrono::Utc;
use dashmap::DashMap;
use futures::channel::mpsc::UnboundedSender;
use gpui::SharedString;
use redis::{Cmd, FromRedisValue, InfoDict, Role, aio::MultiplexedConnection, cluster, cmd};
use semver::Version;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    fs::File,
    io::{BufRead, BufReader, BufWriter, Write},
    path::{Path, PathBuf},
    sync::LazyLock,
    time::Duration,
};
use tracing::{debug, error, info};

type Result<T, E = Error> = std::result::Result<T, E>;

// Global singleton for ConnectionManager
static CONNECTION_MANAGER: LazyLock<ConnectionManager> = LazyLock::new(ConnectionManager::new);

// Enum representing the type of Redis server
#[derive(Debug, Clone, PartialEq)]
enum ServerType {
    Standalone,
    Cluster,
    Sentinel,
}

// Wrapper for the underlying Redis client
#[derive(Clone)]
enum RClient {
    Single(RedisServer),
    Cluster(cluster::ClusterClient),
    SshCluster(cluster::ClusterClient),
}

// Node roles in a Redis setup
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum NodeRole {
    #[default]
    Master,
    Slave,
    Fail,
    Unknown, // e.g. "handshake", "noaddr"
}

// Represents a single Redis node
#[derive(Debug, Clone, Default)]
struct RedisNode {
    server: RedisServer,
    // connection_url: String,
    role: NodeRole,
    master_name: Option<String>,
}

impl RedisNode {
    pub fn host_port(&self) -> String {
        format!("{}:{}", self.server.host, self.server.port)
    }
}

// Information parsed from `CLUSTER NODES` command
#[derive(Debug, Clone)]
pub struct ClusterNodeInfo {
    pub ip: String,
    pub port: u16,
    pub role: NodeRole,
}

/// Parses a Redis address string like "ip:port@cport" or just "ip:port".
fn parse_address(address_str: &str) -> Result<(String, u16, Option<u16>)> {
    // Split into address part and optional cluster bus port part
    let (addr_part, cport_part) = address_str
        .split_once('@')
        .map(|(a, c)| (a, Some(c)))
        .unwrap_or((address_str, None));

    // Parse IP and Port
    let (ip, port_str) = addr_part.split_once(':').ok_or_else(|| Error::Invalid {
        message: format!("Invalid address format: {}", addr_part),
    })?;

    let port = port_str.parse::<u16>().map_err(|e| Error::Invalid {
        message: format!("Invalid port '{}': {}", port_str, e),
    })?;

    // Parse cluster bus port if present
    let cport = cport_part
        .map(|s| {
            s.parse::<u16>().map_err(|e| Error::Invalid {
                message: format!("Invalid cluster bus port '{}': {}", s, e),
            })
        })
        .transpose()?;

    Ok((ip.to_string(), port, cport))
}

/// Parses the output of the `CLUSTER NODES` command.
fn parse_cluster_nodes(raw_data: &str) -> Result<Vec<ClusterNodeInfo>> {
    let mut nodes = Vec::new();

    for line in raw_data.trim().lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();

        // Basic validation: ensure enough columns exist
        if parts.len() < 8 {
            continue;
        }

        let (ip, port, _) = parse_address(parts[1])?;

        // Parse flags to determine role
        let flags: HashSet<String> = parts[2].split(',').map(String::from).collect();
        let role = if flags.contains("master") {
            NodeRole::Master
        } else if flags.contains("slave") {
            NodeRole::Slave
        } else if flags.contains("fail") {
            NodeRole::Fail
        } else {
            NodeRole::Unknown
        };

        nodes.push(ClusterNodeInfo { ip, port, role });
    }

    Ok(nodes)
}

const CONNECTION_TIMEOUT: Duration = Duration::from_secs(30);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(60);

/// Establishes an asynchronous connection based on the client type.
async fn get_async_connection(client: &RClient, db: usize) -> Result<RedisAsyncConn> {
    match client {
        RClient::Single(config) => {
            let conn = open_single_connection(config, db).await?;
            Ok(RedisAsyncConn::Single(conn))
        }
        RClient::Cluster(client) => {
            let cfg = cluster::ClusterConfig::default()
                .set_connection_timeout(CONNECTION_TIMEOUT)
                .set_response_timeout(RESPONSE_TIMEOUT);
            let conn = client.get_async_connection_with_config(cfg).await?;
            Ok(RedisAsyncConn::Cluster(conn))
        }
        RClient::SshCluster(client) => {
            let conn: redis::cluster_async::ClusterConnection<SshMultiplexedConnection> =
                client.get_async_generic_connection().await?;
            Ok(RedisAsyncConn::SshCluster(conn))
        }
    }
}

// TODO 是否在client中保存connection
#[derive(Clone)]
pub struct RedisClient {
    db: usize,
    server_type: ServerType,
    nodes: Vec<RedisNode>,
    master_nodes: Vec<RedisNode>,
    version: Version,
    connection: RedisAsyncConn,
}
#[derive(Debug, Clone, Default)]
pub struct RedisClientDescription {
    pub server_type: SharedString,
    pub master_nodes: SharedString,
    pub slave_nodes: SharedString,
}

#[derive(Debug, Clone)]
pub struct KeyBackupSummary {
    pub file_path: String,
    pub key_count: usize,
    pub bytes: u64,
}

#[derive(Debug, Clone)]
pub struct KeyRestoreSummary {
    pub file_path: String,
    pub restored_count: usize,
    pub failed_count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyBackupProgressPhase {
    Export,
    Restore,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyBackupProgress {
    pub phase: KeyBackupProgressPhase,
    pub processed: usize,
    pub total: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize)]
struct KeyBackupHeader {
    format: String,
    version: u8,
    exported_at: String,
    server_id: String,
    db: usize,
    server_type: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct KeyBackupRecord {
    key: String,
    dump: String,
    ttl_ms: i64,
}

const KEY_BACKUP_FORMAT: &str = "zedis-key-backup";
const KEY_BACKUP_VERSION: u8 = 1;
const KEY_BACKUP_SCAN_COUNT: u64 = 500;
const KEY_BACKUP_PROGRESS_INTERVAL: usize = 100;
impl RedisClient {
    pub fn nodes(&self) -> (usize, usize) {
        (self.master_nodes.len(), self.nodes.len())
    }
    pub fn version(&self) -> String {
        self.version.to_string()
    }
    pub fn supports_db_selection(&self) -> bool {
        self.server_type != ServerType::Cluster
    }

    pub fn nodes_description(&self) -> RedisClientDescription {
        let master_nodes: Vec<String> = self.master_nodes.iter().map(|node| node.host_port()).collect();
        let slave_nodes: Vec<String> = self
            .nodes
            .iter()
            .filter(|node| !master_nodes.contains(&node.host_port()))
            .map(|node| node.host_port().clone())
            .collect();
        RedisClientDescription {
            server_type: format!("{:?}", self.server_type).into(),
            master_nodes: master_nodes.join(",").into(),
            slave_nodes: slave_nodes.join(",").into(),
        }
    }
    /// Returns the connection to the Redis server.
    /// # Returns
    /// * `RedisAsyncConn` - The connection to the Redis server.
    pub fn connection(&self) -> RedisAsyncConn {
        self.connection.clone()
    }
    /// Checks if the client version is at least the given version.
    /// # Arguments
    /// * `version` - The version to check.
    /// # Returns
    /// * `bool` - True if the client version is at least the given version, false otherwise.
    pub fn is_at_least_version(&self, version: &str) -> bool {
        self.version >= Version::parse(version).unwrap_or(Version::new(0, 0, 0))
    }

    /// Executes commands on all master nodes concurrently.
    /// # Arguments
    /// * `cmds` - A vector of commands to execute.
    /// # Returns
    /// * `Vec<T>` - A vector of results from the commands.
    pub async fn query_async_masters<T: FromRedisValue>(&self, cmds: Vec<Cmd>) -> Result<Vec<T>> {
        let addrs: Vec<_> = self.master_nodes.iter().map(|item| item.server.clone()).collect();
        let values = query_async_masters(addrs, self.db, cmds).await?;
        Ok(values)
    }
    /// Calculates the total DB size across all masters.
    /// # Returns
    /// * `u64` - The total DB size.
    pub async fn dbsize(&self) -> Result<u64> {
        let list = self.query_async_masters(vec![cmd("DBSIZE")]).await?;
        Ok(list.iter().sum())
    }
    /// Pings the server to check connectivity.
    pub async fn ping(&self) -> Result<()> {
        let mut conn = self.connection.clone();
        let _: () = cmd("PING").query_async(&mut conn).await?;
        Ok(())
    }
    /// Returns the number of master nodes.
    /// # Returns
    /// * `usize` - The number of master nodes.
    pub fn count_masters(&self) -> Result<usize> {
        Ok(self.master_nodes.len())
    }
    /// Initiates a SCAN operation across all masters.
    /// # Arguments
    /// * `pattern` - The pattern to match keys.
    /// * `count` - The count of keys to return.
    /// # Returns
    /// * `(Vec<u64>, Vec<SharedString>)` - A tuple containing the new cursors and the keys.
    pub async fn first_scan(&self, pattern: &str, count: u64) -> Result<(Vec<u64>, Vec<SharedString>)> {
        let master_count = self.count_masters()?;
        let cursors = vec![0; master_count];

        let (cursors, keys) = self.scan(cursors, pattern, count).await?;
        Ok((cursors, keys))
    }
    /// Continues a SCAN operation.
    /// # Arguments
    /// * `cursors` - A vector of cursors for each master.
    /// * `pattern` - The pattern to match keys.
    /// * `count` - The count of keys to return.
    /// # Returns
    /// * `(Vec<u64>, Vec<SharedString>)` - A tuple containing the new cursors and the keys.
    pub async fn scan(&self, cursors: Vec<u64>, pattern: &str, count: u64) -> Result<(Vec<u64>, Vec<SharedString>)> {
        debug!("scan, cursors: {cursors:?}, pattern: {pattern}, count: {count}");
        let cmds: Vec<Cmd> = cursors
            .iter()
            .map(|cursor| {
                cmd("SCAN")
                    .cursor_arg(*cursor)
                    .arg("MATCH")
                    .arg(pattern)
                    .arg("COUNT")
                    .arg(count)
                    .clone()
            })
            .collect();
        let values: Vec<(u64, Vec<Vec<u8>>)> = self.query_async_masters(cmds).await?;
        let mut cursors = Vec::with_capacity(values.len());
        let mut keys = Vec::with_capacity(values[0].1.len() * values.len());
        for (cursor, keys_in_node) in values {
            cursors.push(cursor);
            keys.extend(
                keys_in_node
                    .iter()
                    .map(|k| String::from_utf8_lossy(k).to_string().into()),
            );
        }
        keys.sort_unstable();
        Ok((cursors, keys))
    }
}

fn key_backup_error(message: impl Into<String>) -> Error {
    Error::Invalid {
        message: message.into(),
    }
}

fn send_key_backup_progress(
    tx: &Option<UnboundedSender<KeyBackupProgress>>,
    phase: KeyBackupProgressPhase,
    processed: usize,
    total: Option<usize>,
) {
    if let Some(tx) = tx {
        let _ = tx.unbounded_send(KeyBackupProgress {
            phase,
            processed,
            total,
        });
    }
}

fn should_report_key_backup_progress(processed: usize) -> bool {
    processed == 0 || processed.is_multiple_of(KEY_BACKUP_PROGRESS_INTERVAL)
}

fn count_key_backup_records(path: &str) -> Result<usize> {
    let file = File::open(path)?;
    let mut lines = BufReader::new(file).lines();
    let _ = lines
        .next()
        .transpose()?
        .ok_or_else(|| key_backup_error("Backup file is empty"))?;
    let mut count = 0_usize;
    for line in lines {
        if !line?.trim().is_empty() {
            count += 1;
        }
    }
    Ok(count)
}

fn key_backup_temp_path(target: &Path) -> PathBuf {
    let file_name = target
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("zedis-backup");
    let suffix = Utc::now()
        .timestamp_nanos_opt()
        .map(|value| value.to_string())
        .unwrap_or_else(|| Utc::now().timestamp_millis().to_string());
    target.with_file_name(format!(".{file_name}.{suffix}.tmp"))
}

fn key_backup_publish_path(target: &Path, attempt: usize) -> PathBuf {
    if attempt == 0 {
        return target.to_path_buf();
    }

    let file_stem = target
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or("zedis-backup");
    let file_name = match target.extension().and_then(|name| name.to_str()) {
        Some(extension) => format!("{file_stem}-{attempt}.{extension}"),
        None => format!("{file_stem}-{attempt}"),
    };
    target.with_file_name(file_name)
}

fn next_available_key_backup_path(target: &Path) -> Result<PathBuf> {
    const MAX_BACKUP_PATH_ATTEMPTS: usize = 1000;
    for attempt in 0..MAX_BACKUP_PATH_ATTEMPTS {
        let path = key_backup_publish_path(target, attempt);
        if !path.exists() {
            return Ok(path);
        }
    }
    Err(key_backup_error(format!(
        "Unable to find an available backup file name near {}",
        target.display()
    )))
}

struct PendingKeyBackupFile {
    path: PathBuf,
    persisted: bool,
}

impl PendingKeyBackupFile {
    fn new(target: &Path) -> Self {
        Self {
            path: key_backup_temp_path(target),
            persisted: false,
        }
    }

    fn persist(mut self, target: &Path) -> Result<PathBuf> {
        let publish_path = next_available_key_backup_path(target)?;
        if publish_path != target {
            info!(
                temp_path = %self.path.display(),
                requested_path = %target.display(),
                publish_path = %publish_path.display(),
                "backup target already exists, publishing with a unique file name"
            );
        }
        std::fs::rename(&self.path, &publish_path)?;
        self.persisted = true;
        Ok(publish_path)
    }
}

impl Drop for PendingKeyBackupFile {
    fn drop(&mut self) {
        if !self.persisted {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

pub struct ConnectionManager {
    clients: DashMap<String, RedisClient>,
}

/// Detects the type of Redis server (Sentinel, Cluster, or Standalone).
/// This function checks the role of the Redis server and returns the server type.
/// # Arguments
/// * `client` - The Redis client to check the server type.
/// # Returns
/// * `ServerType` - The type of the Redis server.
async fn detect_server_type(mut conn: MultiplexedConnection) -> Result<ServerType> {
    // Check if it's a Sentinel
    // Note: `ROLE` command might not exist on old Redis versions, consider fallback if needed.
    // Assuming modern Redis here.
    let role: Role = cmd("ROLE").query_async(&mut conn).await?;

    if let Role::Sentinel { .. } = role {
        return Ok(ServerType::Sentinel);
    }

    // Check if Cluster mode is enabled via INFO command
    let info: InfoDict = cmd("INFO").arg("cluster").query_async(&mut conn).await?;
    let cluster_enabled = info.get("cluster_enabled").unwrap_or(0i64);

    if cluster_enabled == 1 {
        Ok(ServerType::Cluster)
    } else {
        Ok(ServerType::Standalone)
    }
}

impl ConnectionManager {
    pub fn new() -> Self {
        Self {
            clients: DashMap::new(),
        }
    }
    /// Discovers Redis nodes and server type based on initial configuration.
    async fn get_redis_nodes(
        &self,
        name: &str,
        preset_credentials: Vec<PresetCredential>,
    ) -> Result<(Vec<RedisNode>, ServerType, AuthSource)> {
        let config = get_config(name)?;
        let (mut conn, server_type, auth_source) = {
            let (conn, auth_source) = match try_open_with_preset_credentials(&config, 0, preset_credentials).await {
                Ok((conn, auth_source)) => (conn, auth_source),
                Err(e) => {
                    let is_auth = is_auth_error(&e);
                    if !is_auth {
                        error!("detect server type failed: {e:?}, use standalone mode");
                        return Ok((
                            vec![RedisNode {
                                server: config.clone(),
                                role: NodeRole::Master,
                                ..Default::default()
                            }],
                            ServerType::Standalone,
                            AuthSource::None,
                        ));
                    }
                    // sentinel without password
                    // detect server type again
                    let mut tmp_config = config.clone();
                    tmp_config.password = None;
                    let conn = open_single_connection(&tmp_config, 0).await?;
                    (conn, AuthSource::None)
                }
            };
            let server_type = detect_server_type(conn.clone()).await?;
            (conn, server_type, auth_source)
        };
        match server_type {
            ServerType::Cluster => {
                // Fetch cluster topology
                let nodes: String = cmd("CLUSTER").arg("NODES").query_async(&mut conn).await?;
                // Parse nodes and convert to RedisNode
                // For cluster, apply credential from auth_source if it's a preset
                let nodes = parse_cluster_nodes(&nodes)?
                    .iter()
                    .map(|item| {
                        let mut tmp_config = config.clone();
                        tmp_config.port = item.port;
                        tmp_config.host = item.ip.clone();
                        // Apply preset credential to cluster nodes
                        if let AuthSource::Preset(_, ref cred) = auth_source {
                            tmp_config.username = cred.username.clone();
                            tmp_config.password = Some(cred.password.clone());
                        }

                        RedisNode {
                            server: tmp_config,
                            role: item.role.clone(),
                            ..Default::default()
                        }
                    })
                    .collect();
                Ok((nodes, server_type, auth_source))
            }
            ServerType::Sentinel => {
                // let mut conn = client.get_multiplexed_async_connection().await?;
                // Fetch masters from Sentinel
                let masters_response: Vec<HashMap<String, String>> =
                    cmd("SENTINEL").arg("MASTERS").query_async(&mut conn).await?;
                let mut nodes = vec![];

                for item in masters_response {
                    let ip = item.get("ip").ok_or_else(|| Error::Invalid {
                        message: "ip is not found".to_string(),
                    })?;
                    let port: u16 = item
                        .get("port")
                        .ok_or_else(|| Error::Invalid {
                            message: "port is not found".to_string(),
                        })?
                        .parse()
                        .map_err(|e| Error::Invalid {
                            message: format!("Invalid port {e:?}"),
                        })?;
                    let name = item.get("name").ok_or_else(|| Error::Invalid {
                        message: "master_name is not found".to_string(),
                    })?;
                    // Filter by master name if configured
                    if let Some(master_name) = &config.master_name
                        && name != master_name
                    {
                        continue;
                    }
                    let mut tmp_config = config.clone();
                    tmp_config.host = ip.clone();
                    tmp_config.port = port;
                    // Apply preset credential to sentinel nodes
                    if let AuthSource::Preset(_, ref cred) = auth_source {
                        tmp_config.username = cred.username.clone();
                        tmp_config.password = Some(cred.password.clone());
                    }

                    nodes.push(RedisNode {
                        server: tmp_config,
                        role: NodeRole::Master,
                        master_name: Some(name.clone()),
                    });
                }
                // Check for ambiguous master configuration
                let unique_masters: HashSet<_> = nodes.iter().filter_map(|n| n.master_name.as_ref()).collect();
                if unique_masters.len() > 1 {
                    return Err(Error::Invalid {
                        message: "Multiple masters found in Sentinel, please specify master_name".into(),
                    });
                }

                Ok((nodes, server_type, auth_source))
            }
            _ => {
                // For standalone, apply preset credential
                let mut server = config.clone();
                if let AuthSource::Preset(_, ref cred) = auth_source {
                    server.username = cred.username.clone();
                    server.password = Some(cred.password.clone());
                }
                Ok((
                    vec![RedisNode {
                        server,
                        role: NodeRole::Master,
                        ..Default::default()
                    }],
                    server_type,
                    auth_source,
                ))
            }
        }
    }
    pub fn remove_client(&self, name: &str) {
        let prefix = format!("{}:", name);
        let mut hashes = HashSet::new();
        self.clients.retain(|key, client| {
            if key.starts_with(&prefix) {
                for node in &client.nodes {
                    hashes.insert(node.server.get_hash());
                }
                false
            } else {
                true
            }
        });
        if !hashes.is_empty() {
            clear_pool_connections_batch(&hashes);
        }
    }
    /// Retrieves or creates a RedisClient for the given configuration name.
    /// Returns the client and authentication source.
    pub async fn get_client(
        &self,
        server_id: &str,
        db: usize,
        preset_credentials: Vec<PresetCredential>,
    ) -> Result<(RedisClient, AuthSource)> {
        let key = format!("{}:{}", server_id, db);
        if let Some(client) = self.clients.get(&key) {
            return Ok((client.clone(), AuthSource::Config));
        }
        let (nodes, server_type, auth_source) = self.get_redis_nodes(server_id, preset_credentials).await?;
        let client = match server_type {
            ServerType::Cluster => {
                let addrs: Vec<String> = nodes.iter().map(|n| n.server.get_connection_url()).collect();
                let mut builder = cluster::ClusterClientBuilder::new(addrs);
                let node = &nodes[0];
                if let Some(certificates) = node.server.tls_certificates() {
                    builder = builder.certs(certificates);
                }
                if node.server.insecure.unwrap_or(false) {
                    builder = builder.danger_accept_invalid_hostnames(true);
                }
                if node.server.is_ssh_tunnel() {
                    builder = builder.username(server_id);

                    RClient::SshCluster(builder.build()?)
                } else {
                    RClient::Cluster(builder.build()?)
                }
            }
            _ => RClient::Single(nodes[0].server.clone()),
        };
        let master_nodes: Vec<RedisNode> = nodes
            .iter()
            .filter(|node| node.role == NodeRole::Master)
            .cloned()
            .collect();
        let master_nodes_description: Vec<String> = master_nodes.iter().map(|node| node.host_port()).collect();
        info!(master_nodes = ?master_nodes_description, "server master nodes");
        let connection = get_async_connection(&client, db).await?;

        let mut client = RedisClient {
            db,
            server_type: server_type.clone(),
            nodes,
            master_nodes,
            version: Version::new(0, 0, 0),
            connection,
        };
        let mut conn = client.connection.clone();
        client.version = match server_type {
            ServerType::Cluster => {
                let info: redis::Value = cmd("INFO").arg("server").query_async(&mut conn).await?;
                let mut version = "unknown".to_string();
                if let redis::Value::Map(items) = info {
                    for (_, node_info_val) in items {
                        if let Ok(info) = InfoDict::from_redis_value(node_info_val)
                            && let Some(v) = info.get::<String>("redis_version")
                        {
                            version = v;
                            break;
                        }
                    }
                }
                Version::parse(&version).unwrap_or(Version::new(0, 0, 0))
            }
            _ => {
                let info: InfoDict = cmd("INFO").arg("server").query_async(&mut conn).await?;
                let version = info.get::<String>("redis_version").unwrap_or_default();
                Version::parse(&version).unwrap_or(Version::new(0, 0, 0))
            }
        };

        // Cache the client
        self.clients.insert(key, client.clone());
        Ok((client, auth_source))
    }

    /// Exports a key-level backup using SCAN + DUMP + PTTL.
    ///
    /// The generated JSONL file can be restored online with RESTORE REPLACE.
    pub async fn export_key_backup(
        &self,
        server_id: &str,
        db: usize,
        preset_credentials: Vec<PresetCredential>,
        file_path: &str,
        progress_tx: Option<UnboundedSender<KeyBackupProgress>>,
    ) -> Result<KeyBackupSummary> {
        let (client, _) = self.get_client(server_id, db, preset_credentials).await?;
        let master_nodes = client.master_nodes.clone();
        if master_nodes.is_empty() {
            return Err(key_backup_error("No Redis master node found for key backup"));
        }

        let target = Path::new(file_path);
        let server_type = format!("{:?}", client.server_type);
        info!(
            server_id,
            db,
            server_type,
            master_count = master_nodes.len(),
            path = %target.display(),
            "start key backup export"
        );

        let total_keys = client.dbsize().await.ok().map(|value| value as usize);
        send_key_backup_progress(&progress_tx, KeyBackupProgressPhase::Export, 0, total_keys);

        if let Some(parent) = target.parent().filter(|path| !path.as_os_str().is_empty()) {
            info!(path = %parent.display(), "ensure key backup directory exists");
            std::fs::create_dir_all(parent)?;
        }

        let pending_file = PendingKeyBackupFile::new(target);
        info!(
            temp_path = %pending_file.path.display(),
            target_path = %target.display(),
            "write key backup to temp file"
        );
        let file = File::create(&pending_file.path)?;
        let mut writer = BufWriter::new(file);
        let header = KeyBackupHeader {
            format: KEY_BACKUP_FORMAT.to_string(),
            version: KEY_BACKUP_VERSION,
            exported_at: Utc::now().to_rfc3339(),
            server_id: server_id.to_string(),
            db,
            server_type,
        };
        serde_json::to_writer(&mut writer, &header)?;
        writer.write_all(b"\n")?;

        let mut key_count = 0_usize;
        for node in master_nodes {
            let node_id = node.host_port();
            let mut conn = open_single_connection(&node.server, db).await?;
            let mut cursor = 0_u64;
            info!(node = %node_id, "start scanning node for key backup");

            loop {
                let (next_cursor, keys): (u64, Vec<Vec<u8>>) = cmd("SCAN")
                    .cursor_arg(cursor)
                    .arg("COUNT")
                    .arg(KEY_BACKUP_SCAN_COUNT)
                    .query_async(&mut conn)
                    .await?;
                cursor = next_cursor;

                if keys.is_empty() {
                    if cursor == 0 {
                        break;
                    }
                    continue;
                }

                let mut pipeline = redis::pipe();
                for key in &keys {
                    pipeline.cmd("DUMP").arg(key);
                    pipeline.cmd("PTTL").arg(key);
                }
                let results: Vec<redis::Value> = pipeline.query_async(&mut conn).await?;
                let mut result_iter = results.into_iter();

                for key in keys {
                    let dump_value = result_iter
                        .next()
                        .ok_or_else(|| key_backup_error("Missing DUMP result while exporting backup"))?;
                    let ttl_value = result_iter
                        .next()
                        .ok_or_else(|| key_backup_error("Missing PTTL result while exporting backup"))?;
                    if matches!(dump_value, redis::Value::Nil) {
                        continue;
                    }
                    let dump: Vec<u8> = redis::from_redis_value(dump_value).map_err(|e| {
                        key_backup_error(format!("Failed to parse DUMP result while exporting backup: {}", e))
                    })?;
                    let ttl_ms: i64 = redis::from_redis_value(ttl_value).map_err(|e| {
                        key_backup_error(format!("Failed to parse PTTL result while exporting backup: {}", e))
                    })?;
                    if ttl_ms == -2 {
                        continue;
                    }
                    let record = KeyBackupRecord {
                        key: BASE64.encode(&key),
                        dump: BASE64.encode(&dump),
                        ttl_ms,
                    };
                    serde_json::to_writer(&mut writer, &record)?;
                    writer.write_all(b"\n")?;
                    key_count += 1;
                    if should_report_key_backup_progress(key_count) {
                        send_key_backup_progress(&progress_tx, KeyBackupProgressPhase::Export, key_count, total_keys);
                    }
                }

                if cursor == 0 {
                    break;
                }
            }
            info!(node = %node_id, key_count, "node key backup scan finished");
        }

        writer.flush()?;
        writer.get_ref().sync_all()?;
        let bytes = std::fs::metadata(&pending_file.path)?.len();
        drop(writer);
        let published_path = pending_file.persist(target)?;
        send_key_backup_progress(
            &progress_tx,
            KeyBackupProgressPhase::Export,
            key_count,
            total_keys.or(Some(key_count)),
        );

        info!(
            server_id,
            db,
            key_count,
            bytes,
            path = %published_path.display(),
            "key backup export finished"
        );
        Ok(KeyBackupSummary {
            file_path: published_path.to_string_lossy().to_string(),
            key_count,
            bytes,
        })
    }

    /// Restores a key-level backup generated by `export_key_backup`.
    pub async fn restore_key_backup(
        &self,
        server_id: &str,
        db: usize,
        preset_credentials: Vec<PresetCredential>,
        file_path: &str,
        progress_tx: Option<UnboundedSender<KeyBackupProgress>>,
    ) -> Result<KeyRestoreSummary> {
        let _ = self.get_client(server_id, db, preset_credentials).await?;
        let mut conn = self.get_connection(server_id, db).await?;
        let total_records = count_key_backup_records(file_path)?;
        send_key_backup_progress(&progress_tx, KeyBackupProgressPhase::Restore, 0, Some(total_records));
        let file = File::open(file_path)?;
        let mut lines = BufReader::new(file).lines();
        let header_line = lines
            .next()
            .transpose()?
            .ok_or_else(|| key_backup_error("Backup file is empty"))?;
        let header: KeyBackupHeader = serde_json::from_str(&header_line)?;
        if header.format != KEY_BACKUP_FORMAT || header.version != KEY_BACKUP_VERSION {
            return Err(key_backup_error("Unsupported Zedis key backup format"));
        }

        info!(
            server_id,
            db,
            source_db = header.db,
            path = %file_path,
            "start key backup restore"
        );

        let mut restored_count = 0_usize;
        let mut failed_count = 0_usize;
        for (line_index, line) in lines.enumerate() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let record: KeyBackupRecord = serde_json::from_str(&line)
                .map_err(|e| key_backup_error(format!("Invalid backup record at line {}: {}", line_index + 2, e)))?;
            let key = BASE64
                .decode(record.key)
                .map_err(|e| key_backup_error(format!("Invalid key encoding at line {}: {}", line_index + 2, e)))?;
            let dump = BASE64
                .decode(record.dump)
                .map_err(|e| key_backup_error(format!("Invalid dump encoding at line {}: {}", line_index + 2, e)))?;
            let ttl_ms = if record.ttl_ms > 0 { record.ttl_ms } else { 0 };
            let result: redis::RedisResult<()> = cmd("RESTORE")
                .arg(&key)
                .arg(ttl_ms)
                .arg(&dump)
                .arg("REPLACE")
                .query_async(&mut conn)
                .await;
            match result {
                Ok(()) => restored_count += 1,
                Err(e) => {
                    failed_count += 1;
                    error!(error = %e, line = line_index + 2, "failed to restore key backup record");
                }
            }
            let processed = restored_count + failed_count;
            if should_report_key_backup_progress(processed) {
                send_key_backup_progress(
                    &progress_tx,
                    KeyBackupProgressPhase::Restore,
                    processed,
                    Some(total_records),
                );
            }
        }

        send_key_backup_progress(
            &progress_tx,
            KeyBackupProgressPhase::Restore,
            restored_count + failed_count,
            Some(total_records),
        );

        info!(
            server_id,
            db,
            restored_count,
            failed_count,
            path = %file_path,
            "key backup restore finished"
        );
        Ok(KeyRestoreSummary {
            file_path: file_path.to_string(),
            restored_count,
            failed_count,
        })
    }

    /// Shorthand to get an async connection directly.
    /// Uses empty preset credentials since connection should already be cached.
    pub async fn get_connection(&self, server_id: &str, db: usize) -> Result<RedisAsyncConn> {
        let (client, _) = self.get_client(server_id, db, vec![]).await?;
        Ok(client.connection.clone())
    }
}

/// Global accessor for the connection manager.
pub fn get_connection_manager() -> &'static ConnectionManager {
    &CONNECTION_MANAGER
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_key_backup_record() {
        let record = KeyBackupRecord {
            key: BASE64.encode(b"hello"),
            dump: BASE64.encode(b"serialized"),
            ttl_ms: -1,
        };
        let line = serde_json::to_string(&record).expect("serialize backup record");
        let parsed: KeyBackupRecord = serde_json::from_str(&line).expect("parse backup record");
        assert_eq!(BASE64.decode(parsed.key).expect("decode key"), b"hello");
        assert_eq!(BASE64.decode(parsed.dump).expect("decode dump"), b"serialized");
        assert_eq!(parsed.ttl_ms, -1);
    }

    #[test]
    fn key_backup_temp_path_stays_next_to_target() {
        let target = Path::new("/tmp/redis.zedis-backup.jsonl");
        let temp_path = key_backup_temp_path(target);
        assert_eq!(temp_path.parent(), target.parent());
        assert!(
            temp_path
                .file_name()
                .and_then(|name| name.to_str())
                .expect("temp file name")
                .starts_with(".redis.zedis-backup.jsonl.")
        );
    }

    #[test]
    fn pending_key_backup_file_uses_unique_path_when_target_exists() {
        let suffix = Utc::now()
            .timestamp_nanos_opt()
            .map(|value| value.to_string())
            .unwrap_or_else(|| Utc::now().timestamp_millis().to_string());
        let dir = std::env::temp_dir().join(format!("zedis-backup-test-{}-{suffix}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");

        let target = dir.join("backup.zedis-backup.jsonl");
        std::fs::write(&target, b"existing").expect("write existing target");
        let pending = PendingKeyBackupFile::new(&target);
        let temp_path = pending.path.clone();
        std::fs::write(&temp_path, b"new").expect("write temp backup");

        let published_path = pending.persist(&target).expect("publish temp backup");
        assert_ne!(published_path, target);
        assert_eq!(std::fs::read(&target).expect("read existing target"), b"existing");
        assert_eq!(std::fs::read(&published_path).expect("read published backup"), b"new");
        assert!(!temp_path.exists());

        std::fs::remove_dir_all(&dir).expect("remove temp dir");
    }
}
