use std::{
    collections::BTreeMap,
    fmt,
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use anyhow::anyhow;
use base64::Engine;
use bitcoin::{Block, BlockHash, Txid};
use hex::FromHexError;
use log::{error, info};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use threadpool::ThreadPool;
use tokio::time::Instant;
use wallet::{bdk_wallet::chain::ConfirmationTime, bitcoin, bitcoin::Transaction};

use crate::node::BlockSource;

const BITCOIN_RPC_IN_WARMUP: i32 = -28; // Client still warming up
const BITCOIN_RPC_CLIENT_NOT_CONNECTED: i32 = -9; // Bitcoin is not connected
const BITCOIN_RPC_CLIENT_IN_INITIAL_DOWNLOAD: i32 = -10; // Still downloading initial blocks

#[derive(Clone)]
pub struct BitcoinRpc {
    id: Arc<AtomicU64>,
    auth_token: Option<String>,
    url: String,
}

pub struct BlockFetcher {
    client: reqwest::blocking::Client,
    rpc: Arc<BitcoinRpc>,
    job_id: Arc<AtomicUsize>,
    sender: std::sync::mpsc::SyncSender<BlockEvent>,
}

pub enum BlockEvent {
    Block(RpcBlockId, Block),
    Error(BlockFetchError),
}

pub enum BitcoinRpcAuth {
    UserPass(String, String),
    Cookie(String),
    None,
}

#[derive(Debug)]
pub enum BitcoinRpcError {
    Rpc(JsonRpcError),
    Transport(reqwest::Error),
    Other(String),
}

#[derive(Debug)]
pub enum BlockFetchError {
    RpcError(BitcoinRpcError),
    BlockMismatch,
    ChannelClosed,
}

impl fmt::Display for BlockFetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BlockFetchError::RpcError(e) => write!(f, "RPC error: {}", e),
            BlockFetchError::BlockMismatch => write!(f, "Block mismatch detected"),
            BlockFetchError::ChannelClosed => write!(f, "Channel closed"),
        }
    }
}

impl std::error::Error for BlockFetchError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            BlockFetchError::RpcError(e) => Some(e),
            _ => None,
        }
    }
}

impl From<BitcoinRpcError> for BlockFetchError {
    fn from(err: BitcoinRpcError) -> Self {
        BlockFetchError::RpcError(err)
    }
}

#[derive(Serialize, Deserialize)]
pub struct JsonRpcResponse<T> {
    pub result: Option<T>,
    pub error: Option<JsonRpcError>,
    pub id: String,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
}

pub struct BitcoinRpcRequest {
    id: u64,
    body: serde_json::Value,
}

trait ErrorForRpc {
    async fn error_for_rpc<T: DeserializeOwned>(self) -> Result<T, BitcoinRpcError>;
}

trait ErrorForRpcBlocking {
    fn error_for_rpc<T: DeserializeOwned>(self) -> Result<T, BitcoinRpcError>;
}

impl BitcoinRpc {
    pub fn new(url: &str, auth: BitcoinRpcAuth) -> Self {
        Self {
            id: Default::default(),
            auth_token: auth.to_token(),
            url: url.to_string(),
        }
    }

    pub fn make_request(&self, method: &str, params: serde_json::Value) -> BitcoinRpcRequest {
        let id = self.id.fetch_add(1, Ordering::Relaxed);
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id.to_string(),
            "method": method,
            "params": params,
        });
        return BitcoinRpcRequest { id, body };
    }

    pub fn get_block_count(&self) -> BitcoinRpcRequest {
        let params = serde_json::json!([]);

        self.make_request("getblockcount", params)
    }

    pub fn get_block_hash(&self, height: u32) -> BitcoinRpcRequest {
        let params = serde_json::json!([height]);

        self.make_request("getblockhash", params)
    }

    pub fn get_block(&self, hash: &BlockHash) -> BitcoinRpcRequest {
        let params = serde_json::json!([hash, /* verbosity */ 0]);

        self.make_request("getblock", params)
    }

    pub fn get_blockchain_info(&self) -> BitcoinRpcRequest {
        let params = serde_json::json!([]);
        self.make_request("getblockchaininfo", params)
    }

    pub fn get_mempool_entry(&self, txid: Txid) -> BitcoinRpcRequest {
        let params = serde_json::json!([txid]);

        self.make_request("getmempoolentry", params)
    }

    pub fn send_raw_transaction(&self, tx: &Transaction) -> BitcoinRpcRequest {
        let raw_hex = bitcoin::consensus::encode::serialize_hex(&tx);
        let params =
            serde_json::json!([raw_hex, /* max fee rate */ 0, /* max burn amount */ 21_000_000]);

        self.make_request("sendrawtransaction", params)
    }

    pub async fn send_json<T: DeserializeOwned>(
        &self,
        client: &reqwest::Client,
        request: &BitcoinRpcRequest,
    ) -> Result<T, BitcoinRpcError> {
        self.send(client, request).await?.error_for_rpc().await
    }

    pub async fn send(
        &self,
        client: &reqwest::Client,
        request: &BitcoinRpcRequest,
    ) -> Result<reqwest::Response, BitcoinRpcError> {
        let mut delay = Duration::from_millis(20);
        let max_retries = 5;

        let mut last_error = None;
        for attempt in 0..max_retries {
            match self.send_request(client, &request).await {
                Ok(res) => return Ok(res),
                Err(e) if e.is_temporary() && attempt < max_retries - 1 => {
                    error!("Rpc request: {} - retrying in {:?}...", e, delay);
                    last_error = Some(e);
                    tokio::time::sleep(delay).await;
                    delay *= 2;
                }
                Err(err) => return Err(err),
            }
        }
        Err(last_error.expect("an error"))
    }

    pub fn send_json_blocking<T: DeserializeOwned>(
        &self,
        client: &reqwest::blocking::Client,
        request: &BitcoinRpcRequest,
    ) -> Result<T, BitcoinRpcError> {
        let mut delay = Duration::from_millis(20);
        let max_retries = 5;

        let mut last_error = None;
        for attempt in 0..max_retries {
            match self.send_request_blocking(client, &request) {
                Ok(res) => return res.error_for_rpc(),
                Err(e) if e.is_temporary() && attempt < max_retries - 1 => {
                    error!("Rpc request: {} - retrying in {:?}...", e, delay);
                    last_error = Some(e);
                    std::thread::sleep(delay);
                    delay *= 2;
                }
                Err(err) => return Err(err),
            }
        }
        Err(last_error.expect("an error"))
    }

    pub fn broadcast_tx(
        &self,
        client: &reqwest::blocking::Client,
        tx: &Transaction,
    ) -> Result<ConfirmationTime, BitcoinRpcError> {
        let txid: String = self.send_json_blocking(client, &self.send_raw_transaction(tx))?;

        const MAX_RETRIES: usize = 10;
        let mut retry_count = 0;
        let mut last_error = None;
        while retry_count < MAX_RETRIES {
            let params = serde_json::json!([txid]);
            let res: Result<serde_json::Value, _> =
                self.send_json_blocking(client, &self.make_request("getmempoolentry", params));
            match res {
                Ok(mem) => {
                    if let Some(time) = mem.get("time").and_then(|t| t.as_u64()) {
                        return Ok(ConfirmationTime::Unconfirmed { last_seen: time });
                    }
                }
                Err(e) => last_error = Some(e),
            }
            std::thread::sleep(Duration::from_millis(100));
            retry_count += 1;
        }

        Err(last_error.expect("an error"))
    }

    async fn send_request(
        &self,
        client: &reqwest::Client,
        request: &BitcoinRpcRequest,
    ) -> Result<reqwest::Response, BitcoinRpcError> {
        let mut builder = client.post(&self.url);
        if let Some(auth) = self.auth_token.as_ref() {
            builder = builder.header("Authorization", format!("Basic {}", auth));
        }
        builder
            .json(&request.body)
            .send()
            .await
            .map_err(|e| e.into())
    }

    fn send_request_blocking(
        &self,
        client: &reqwest::blocking::Client,
        request: &BitcoinRpcRequest,
    ) -> Result<reqwest::blocking::Response, BitcoinRpcError> {
        let mut builder = client.post(&self.url);
        if let Some(auth) = self.auth_token.as_ref() {
            builder = builder.header("Authorization", format!("Basic {}", auth));
        }

        builder.json(&request.body).send().map_err(|e| e.into())
    }
}

impl BitcoinRpcAuth {
    fn to_token(&self) -> Option<String> {
        match self {
            BitcoinRpcAuth::UserPass(user, pass) => {
                Some(base64::prelude::BASE64_STANDARD.encode(format!("{user}:{pass}")))
            }
            BitcoinRpcAuth::Cookie(cookie) => Some(cookie.clone()),
            BitcoinRpcAuth::None => None,
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct RpcBlockId {
    pub height: u32,
    pub hash: BlockHash,
}

impl Ord for RpcBlockId {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.height.cmp(&other.height)
    }
}

impl PartialOrd for RpcBlockId {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl BlockFetcher {
    pub fn new(
        rpc: BitcoinRpc,
        client: reqwest::blocking::Client,
    ) -> (Self, std::sync::mpsc::Receiver<BlockEvent>) {
        let (tx, rx) = std::sync::mpsc::sync_channel(12);
        (
            Self {
                client,
                rpc: Arc::new(rpc),
                job_id: Arc::new(AtomicUsize::new(0)),
                sender: tx,
            },
            rx,
        )
    }

    pub fn stop(&self) {
        self.job_id.fetch_add(1, Ordering::SeqCst);
    }

    pub fn start(&self, mut start_block: RpcBlockId) {
        self.stop();

        let task_client = self.client.clone();
        let task_rpc = self.rpc.clone();
        let current_task = self.job_id.clone();
        let task_sender = self.sender.clone();

        _ = std::thread::spawn(move || {
            let mut last_check = Instant::now() - Duration::from_secs(2);
            let job_id = current_task.load(Ordering::SeqCst);

            loop {
                if current_task.load(Ordering::SeqCst) != job_id {
                    info!("Shutting down block fetcher");
                    return;
                }
                if last_check.elapsed() < Duration::from_secs(1) {
                    std::thread::sleep(Duration::from_millis(1));
                    continue;
                }
                last_check = Instant::now();

                let tip: u32 =
                    match task_rpc.send_json_blocking(&task_client, &task_rpc.get_block_count()) {
                        Ok(t) => t,
                        Err(e) => {
                            _ = task_sender.send(BlockEvent::Error(BlockFetchError::RpcError(e)));
                            return;
                        }
                    };

                if tip > start_block.height {
                    let concurrency = std::cmp::min(tip - start_block.height, 8);

                    let res = Self::run_workers(
                        job_id,
                        current_task.clone(),
                        task_rpc.clone(),
                        task_sender.clone(),
                        start_block,
                        tip,
                        concurrency as usize,
                    );

                    match res {
                        Ok(new_tip) => {
                            start_block = new_tip;
                        }
                        Err(e) => {
                            _ = task_sender.send(BlockEvent::Error(e));
                            current_task.fetch_add(1, Ordering::SeqCst);
                            return;
                        }
                    }
                }
            }
        });
    }

    fn run_workers(
        job_id: usize,
        current_task: Arc<AtomicUsize>,
        rpc: Arc<BitcoinRpc>,
        sender: std::sync::mpsc::SyncSender<BlockEvent>,
        start_block: RpcBlockId,
        end_height: u32,
        concurrency: usize,
    ) -> Result<RpcBlockId, BlockFetchError> {
        let pool = ThreadPool::new(concurrency);
        let client = reqwest::blocking::Client::new();

        let (tx, rx) = std::sync::mpsc::sync_channel(1);

        let mut queued_height = start_block.height + 1;

        let mut parsed_blocks = BTreeMap::new();
        let mut previous_hash = start_block.hash;
        let mut next_emit_height = queued_height;

        while queued_height <= end_height || pool.active_count() > 0 || !parsed_blocks.is_empty() {
            if current_task.load(Ordering::SeqCst) != job_id {
                return Err(BlockFetchError::ChannelClosed);
            }

            while pool.queued_count() < concurrency && queued_height <= end_height {
                let tx = tx.clone();
                let rpc = rpc.clone();
                let task_client = client.clone();
                let task_sigterm = current_task.clone();

                pool.execute(move || {
                    if task_sigterm.load(Ordering::SeqCst) != job_id {
                        return;
                    }
                    let result: Result<_, BitcoinRpcError> = (move || {
                        let hash: BlockHash = rpc
                            .send_json_blocking(&task_client, &rpc.get_block_hash(queued_height))?;
                        let block = Self::fetch_block(&rpc, &task_client, &hash)?;
                        Ok((
                            queued_height,
                            RpcBlockId {
                                height: queued_height,
                                hash,
                            },
                            block,
                        ))
                    })();

                    _ = tx.send(result);
                });

                queued_height += 1;
            }

            // Check if any blocks are ready to emit
            if let Ok(result) = rx.try_recv() {
                if current_task.load(Ordering::SeqCst) != job_id {
                    return Err(BlockFetchError::ChannelClosed);
                }

                let (height, id, block) = result?;
                parsed_blocks.insert(height, (id, block));

                // Emit blocks in order
                while let Some((id, block)) = parsed_blocks.remove(&next_emit_height) {
                    if current_task.load(Ordering::SeqCst) != job_id {
                        return Err(BlockFetchError::ChannelClosed);
                    }
                    if block.header.prev_blockhash != previous_hash {
                        return Err(BlockFetchError::BlockMismatch);
                    }
                    sender
                        .send(BlockEvent::Block(id, block))
                        .map_err(|_| BlockFetchError::ChannelClosed)?;
                    previous_hash = id.hash;
                    next_emit_height += 1;
                }
            }
        }

        Ok(RpcBlockId {
            height: next_emit_height - 1,
            hash: previous_hash,
        })
    }

    pub fn fetch_block(
        rpc: &BitcoinRpc,
        client: &reqwest::blocking::Client,
        hash: &BlockHash,
    ) -> Result<Block, BitcoinRpcError> {
        let block_req = rpc.get_block(&hash);
        let id = block_req.id;
        let response = rpc
            .send_request_blocking(client, &block_req)?
            .error_for_status()?;
        let mut raw = response.bytes()?.to_vec();

        let start_needle = "{\"result\":\"";
        let end_needle = format!("\",\"error\":null,\"id\":\"{}\"}}\n", id.to_string());

        // Check if we can quickly extract block
        let hex_block =
            if raw.starts_with(start_needle.as_bytes()) && raw.ends_with(end_needle.as_bytes()) {
                raw.drain(0..start_needle.len());
                raw.truncate(raw.len() - end_needle.len());
                raw
            } else {
                // fallback to decoding json
                let hex_block: JsonRpcResponse<String> = serde_json::from_slice(raw.as_slice())
                    .map_err(|e| BitcoinRpcError::Other(e.to_string()))?;
                if let Some(e) = hex_block.error {
                    return Err(BitcoinRpcError::Rpc(e));
                }
                hex_block.result.unwrap().into_bytes()
            };

        if hex_block.len() % 2 != 0 {
            return Err(BitcoinRpcError::Other(
                "Parse error: could not hex decode block".to_string(),
            ));
        }

        let raw_block = hex_to_bytes(hex_block).map_err(|e| {
            BitcoinRpcError::Other(format!("Hex deserialize error: {}", e.to_string()))
        })?;

        let block: Block =
            bitcoin::consensus::encode::deserialize(raw_block.as_slice()).map_err(|e| {
                BitcoinRpcError::Other(format!("Block Deserialize error: {}", e.to_string()))
            })?;
        Ok(block)
    }
}

// From hex crate
pub(crate) fn hex_to_bytes(mut hex_data: Vec<u8>) -> Result<Vec<u8>, FromHexError> {
    let len = hex_data.len() / 2;
    for i in 0..len {
        let byte = val(hex_data[2 * i], 2 * i)? << 4 | val(hex_data[2 * i + 1], 2 * i + 1)?;
        hex_data[i] = byte;
    }
    hex_data.truncate(len);
    Ok(hex_data)
}

// From hex crate
fn val(c: u8, idx: usize) -> Result<u8, FromHexError> {
    match c {
        b'A'..=b'F' => Ok(c - b'A' + 10),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'0'..=b'9' => Ok(c - b'0'),
        _ => Err(hex::FromHexError::InvalidHexCharacter {
            c: c as char,
            index: idx,
        }),
    }
}

impl BitcoinRpcError {
    fn is_temporary(&self) -> bool {
        return match self {
            BitcoinRpcError::Transport(e) => {
                if e.is_timeout() || e.is_connect() {
                    return true;
                }
                if let Some(status) = e.status() {
                    match status {
                        reqwest::StatusCode::REQUEST_TIMEOUT
                        | reqwest::StatusCode::TOO_MANY_REQUESTS
                        | reqwest::StatusCode::INTERNAL_SERVER_ERROR
                        | reqwest::StatusCode::BAD_GATEWAY
                        | reqwest::StatusCode::SERVICE_UNAVAILABLE
                        | reqwest::StatusCode::GATEWAY_TIMEOUT => return true,
                        _ => {}
                    }
                }
                false
            }
            BitcoinRpcError::Rpc(e) => {
                matches!(
                    e.code,
                    BITCOIN_RPC_IN_WARMUP
                        | BITCOIN_RPC_CLIENT_IN_INITIAL_DOWNLOAD
                        | BITCOIN_RPC_CLIENT_NOT_CONNECTED
                )
            }
            _ => false,
        };
    }
}

impl From<reqwest::Error> for BitcoinRpcError {
    fn from(value: reqwest::Error) -> Self {
        Self::Transport(value)
    }
}

impl fmt::Display for BitcoinRpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BitcoinRpcError::Rpc(rpc_error) => {
                write!(f, "RPC: {}:{}", rpc_error.code, rpc_error.message)
            }
            BitcoinRpcError::Transport(transport_error) => {
                write!(f, "Transport: {}", transport_error)
            }
            BitcoinRpcError::Other(message) => write!(f, "{}", message),
        }
    }
}

impl std::error::Error for BitcoinRpcError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            BitcoinRpcError::Transport(e) => Some(e),
            _ => None,
        }
    }
}

impl ErrorForRpc for reqwest::Response {
    async fn error_for_rpc<T: DeserializeOwned>(self) -> Result<T, BitcoinRpcError> {
        let rpc_res: JsonRpcResponse<T> = self.json().await?;
        if let Some(e) = rpc_res.error {
            return Err(BitcoinRpcError::Rpc(e));
        }

        return Ok(rpc_res.result.unwrap());
    }
}

impl ErrorForRpcBlocking for reqwest::blocking::Response {
    fn error_for_rpc<T: DeserializeOwned>(self) -> Result<T, BitcoinRpcError> {
        let rpc_res: JsonRpcResponse<T> = self.json()?;
        if let Some(e) = rpc_res.error {
            return Err(BitcoinRpcError::Rpc(e));
        }

        return Ok(rpc_res.result.unwrap());
    }
}

#[derive(Clone)]
pub struct BitcoinBlockSource {
    pub client: reqwest::blocking::Client,
    pub rpc: BitcoinRpc,
}

impl BitcoinBlockSource {
    pub fn new(rpc: BitcoinRpc) -> Self {
        let client = reqwest::blocking::Client::new();
        Self { client, rpc }
    }
}

impl BlockSource for BitcoinBlockSource {
    fn get_block_hash(&self, height: u32) -> anyhow::Result<BlockHash> {
        Ok(self
            .rpc
            .send_json_blocking(&self.client, &self.rpc.get_block_hash(height))?)
    }

    fn get_block(&self, hash: &BlockHash) -> anyhow::Result<Block> {
        Ok(self
            .rpc
            .send_json_blocking(&self.client, &self.rpc.get_block(hash))?)
    }

    fn get_median_time(&self) -> anyhow::Result<u64> {
        let info: serde_json::Value = self
            .rpc
            .send_json_blocking(&self.client, &self.rpc.get_blockchain_info())?;
        if let Some(time) = info.get("mediantime").and_then(|t| t.as_u64()) {
            return Ok(time);
        }
        return Err(anyhow!("Could not fetch median time"));
    }

    fn get_block_count(&self) -> anyhow::Result<u64> {
        Ok(self
            .rpc
            .send_json_blocking(&self.client, &self.rpc.get_block_count())?)
    }
}
