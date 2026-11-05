//! A connection to one lightwalletd server.
//!
//! Construction checks that the server is on the requested network.
//! The session stays on that server. Choosing another server, and
//! swapping which session new work uses, is the caller's job.

use tonic::transport::Channel;
use zcash_client_backend::data_api::{chain, AccountBirthday};
use zcash_client_backend::proto::service::compact_tx_streamer_client::CompactTxStreamerClient;
use zcash_primitives::block::BlockHash;
use zcash_protocol::consensus::BlockHeight;

use crate::network::WalletNetwork;

use super::lwd::{get_latest_block, get_tree_state, open_lwd_channel, sapling_activation_height};
use super::{should_use_empty_chain_state, SyncProgressEvent};

/// A connection to one lightwalletd server that passed the network check.
///
/// Each operation clones the underlying client, so sync and send can run
/// at the same time. The session does not switch servers.
pub struct LwdSession {
    runtime: tokio::runtime::Runtime,
    client: CompactTxStreamerClient<Channel>,
}

impl LwdSession {
    /// Connect to `server_url` and reject it unless it serves `network`.
    pub fn connect(network: WalletNetwork, server_url: &str) -> Result<Self, String> {
        let runtime = tokio::runtime::Runtime::new().map_err(|e| format!("tokio: {e}"))?;
        let server_url = server_url.to_string();
        let client = runtime.block_on(async {
            let mut client = open_lwd_channel(&server_url)
                .await
                .map_err(|e| e.to_string())?;
            let height = sapling_activation_height(&mut client)
                .await
                .map_err(|e| e.to_string())?;
            let served = WalletNetwork::from_sapling_activation_height(height);
            if served != Some(network) {
                let served = served.map_or_else(
                    || "an unknown network".to_string(),
                    |net| format!("the {} network", net.as_str()),
                );
                return Err(format!(
                    "{server_url} serves {served}, but this database is on {}",
                    network.as_str()
                ));
            }
            Ok(client)
        })?;
        Ok(Self { runtime, client })
    }

    /// A client handle for one operation. The handle shares this session's connection.
    pub fn client(&self) -> CompactTxStreamerClient<Channel> {
        self.client.clone()
    }

    /// Run `future` on this session's async machinery.
    pub fn block_on<F, T>(&self, future: F) -> T
    where
        F: std::future::Future<Output = T>,
    {
        self.runtime.block_on(future)
    }

    /// The server's current chain tip height.
    pub fn tip(&self) -> Result<u64, String> {
        let mut client = self.client.clone();
        self.runtime.block_on(async {
            get_latest_block(&mut client)
                .await
                .map(|tip| tip.height)
                .map_err(|e| e.to_string())
        })
    }

    /// The birthday of an account whose first block is `height`: empty note
    /// commitment trees at or below Sapling activation, otherwise the tree state
    /// this server reports for block `height - 1`.
    pub fn birthday(&self, network: WalletNetwork, height: u64) -> Result<AccountBirthday, String> {
        let start = BlockHeight::try_from(height)
            .map_err(|_| format!("Birthday {height} is not a valid block height"))?;
        if should_use_empty_chain_state(&network, start).map_err(|e| e.to_string())? {
            let prior = chain::ChainState::empty(start - 1, BlockHash([0u8; 32]));
            return Ok(AccountBirthday::from_parts(prior, None));
        }
        let mut client = self.client.clone();
        let tree_state = self.runtime.block_on(async {
            get_tree_state(&mut client, height - 1)
                .await
                .map_err(|e| e.to_string())
        })?;
        if tree_state.height != height - 1 {
            return Err(format!(
                "lightwalletd returned the tree state for height {}, not {}",
                tree_state.height,
                height - 1
            ));
        }
        AccountBirthday::from_treestate(tree_state, None)
            .map_err(|e| format!("Could not read the tree state before birthday {height}: {e}"))
    }

    /// Sync `db_path` through this session's server.
    pub fn sync(
        &self,
        db_path: &str,
        network: WalletNetwork,
        progress: impl Fn(SyncProgressEvent) + Send + Sync,
    ) -> Result<(), String> {
        crate::account::ensure_db_migrated_once(db_path, network)?;
        let mut client = self.client.clone();
        self.runtime.block_on(super::run_sync_inner(
            db_path,
            &mut client,
            network,
            progress,
        ))
    }
}

/// The servers a caller may use: `explicit`, or the network's defaults.
fn permitted_urls(network: WalletNetwork, explicit: Option<&str>) -> Vec<String> {
    match explicit {
        Some(url) => vec![url.to_string()],
        None => network
            .default_lightwalletd_urls()
            .iter()
            .map(|url| (*url).to_string())
            .collect(),
    }
}

/// Connect to the first permitted server that passes the network check.
pub fn connect_permitted(
    network: WalletNetwork,
    explicit: Option<&str>,
) -> Result<LwdSession, String> {
    connect_urls(network, &permitted_urls(network, explicit))
}

fn connect_urls(network: WalletNetwork, urls: &[String]) -> Result<LwdSession, String> {
    let mut last_err = String::from("no lightwalletd server is configured");
    for (i, url) in urls.iter().enumerate() {
        match LwdSession::connect(network, url) {
            Ok(session) => {
                if i > 0 {
                    log::warn!("failing over to {url}");
                }
                return Ok(session);
            }
            Err(error) => {
                if i + 1 < urls.len() {
                    log::warn!("{url} unreachable ({error}); trying next endpoint");
                } else {
                    log::warn!("{url} unreachable ({error})");
                }
                last_err = error;
            }
        }
    }
    Err(last_err)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_server_is_the_only_permitted_url() {
        let urls = permitted_urls(WalletNetwork::Main, Some("https://example.invalid:443"));
        assert_eq!(urls, vec!["https://example.invalid:443".to_string()]);
    }

    #[test]
    fn defaults_follow_the_network_list() {
        let urls = permitted_urls(WalletNetwork::Test, None);
        assert_eq!(urls, vec!["https://testnet.zec.rocks:443".to_string()]);
    }
}
