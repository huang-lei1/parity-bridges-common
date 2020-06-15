// Copyright 2019-2020 Parity Technologies (UK) Ltd.
// This file is part of Parity Bridges Common.

// Parity Bridges Common is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Parity Bridges Common is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Parity Bridges Common.  If not, see <http://www.gnu.org/licenses/>.

//! Ethereum PoA -> Substrate synchronization.

use crate::ethereum_client::{self, EthereumConnectionParams, EthereumRpcClient, HigherLevelCalls};
use crate::ethereum_types::{EthereumHeaderId, EthereumHeadersSyncPipeline, Header, QueuedEthereumHeader, Receipt};
use crate::rpc::{EthereumRpc, SubstrateRpc};
use crate::rpc_errors::RpcError;
use crate::substrate_client::{
	self, AlsoHigherLevelCalls, SubstrateConnectionParams, SubstrateRpcClient, SubstrateSigningParams,
};
use crate::sync::{HeadersSyncParams, TargetTransactionMode};
use crate::sync_loop::{OwnedSourceFutureOutput, OwnedTargetFutureOutput, SourceClient, TargetClient};

use async_trait::async_trait;
use std::{collections::HashSet, time::Duration};
use web3::types::H256;

use parking_lot::Mutex;
use std::sync::Arc;

/// Interval at which we check new Ethereum headers when we are synced/almost synced.
const ETHEREUM_TICK_INTERVAL: Duration = Duration::from_secs(10);
/// Interval at which we check new Substrate blocks.
const SUBSTRATE_TICK_INTERVAL: Duration = Duration::from_secs(5);
/// Max number of headers in single submit transaction.
const MAX_HEADERS_IN_SINGLE_SUBMIT: usize = 32;
/// Max total size of headers in single submit transaction. This only affects signed
/// submissions, when several headers are submitted at once. 4096 is the maximal **expected**
/// size of the Ethereum header + transactions receipts (if they're required).
const MAX_HEADERS_SIZE_IN_SINGLE_SUBMIT: usize = MAX_HEADERS_IN_SINGLE_SUBMIT * 4096;
/// Max Ethereum headers we want to have in all 'before-submitted' states.
const MAX_FUTURE_HEADERS_TO_DOWNLOAD: usize = 128;
/// Max Ethereum headers count we want to have in 'submitted' state.
const MAX_SUBMITTED_HEADERS: usize = 128;
/// Max depth of in-memory headers in all states. Past this depth they will be forgotten (pruned).
const PRUNE_DEPTH: u32 = 4096;

/// Ethereum synchronization parameters.
pub struct EthereumSyncParams {
	/// Ethereum connection params.
	pub eth: EthereumConnectionParams,
	/// Substrate connection params.
	pub sub: SubstrateConnectionParams,
	/// Substrate signing params.
	pub sub_sign: SubstrateSigningParams,
	/// Synchronization parameters.
	pub sync_params: HeadersSyncParams,
}

impl Default for EthereumSyncParams {
	fn default() -> Self {
		EthereumSyncParams {
			eth: Default::default(),
			sub: Default::default(),
			sub_sign: Default::default(),
			sync_params: HeadersSyncParams {
				max_future_headers_to_download: MAX_FUTURE_HEADERS_TO_DOWNLOAD,
				max_headers_in_submitted_status: MAX_SUBMITTED_HEADERS,
				max_headers_in_single_submit: MAX_HEADERS_IN_SINGLE_SUBMIT,
				max_headers_size_in_single_submit: MAX_HEADERS_SIZE_IN_SINGLE_SUBMIT,
				prune_depth: PRUNE_DEPTH,
				target_tx_mode: TargetTransactionMode::Signed,
			},
		}
	}
}

/// Ethereum client as headers source.
struct EthereumHeadersSource {
	/// Ethereum node client.
	client: EthereumRpcClient,
}

// pub type OwnedSourceFutureOutput<Client, P, T> = (Client, Result<T, <Client as SourceClient<P>>::Error>);
type EthereumFutureOutput<T> = OwnedSourceFutureOutput<EthereumHeadersSource, EthereumHeadersSyncPipeline, T>;
// type EthereumFutureOutput<T> = OwnedSourceFutureOutput<EthereumHeadersSyncPipeline, T>;

#[async_trait]
impl SourceClient<EthereumHeadersSyncPipeline> for EthereumHeadersSource {
	type Error = RpcError;

	async fn best_block_number(&mut self) -> Result<u64, Self::Error> {
		Ok(self.client.best_block_number().await?)
	}

	async fn header_by_hash(&mut self, hash: H256) -> Result<Header, Self::Error> {
		Ok(self.client.header_by_hash(hash).await?)
	}

	async fn header_by_number(&mut self, number: u64) -> Result<Header, Self::Error> {
		Ok(self.client.header_by_number(number).await?)
	}

	async fn header_completion(&mut self, id: EthereumHeaderId) -> Result<(EthereumHeaderId, Option<()>), Self::Error> {
		Ok((id, None))
	}

	async fn header_extra(
		&mut self,
		id: EthereumHeaderId,
		header: QueuedEthereumHeader,
	) -> Result<(EthereumHeaderId, Vec<Receipt>), Self::Error> {
		Ok(self
			.client
			.transactions_receipts(id, header.header().transactions.clone())
			.await?)
	}
}

struct SubstrateTargetData {
	/// Substrate node client.
	client: SubstrateRpcClient, // substrate_client::Client,
	/// Whether we want to submit signed (true), or unsigned (false) transactions.
	sign_transactions: bool,
	/// Substrate signing params.
	sign_params: SubstrateSigningParams,
}

struct SubstrateHeadersTarget(Arc<Mutex<SubstrateTargetData>>);
impl SubstrateHeadersTarget {
	fn new(client: SubstrateRpcClient, sign_transactions: bool, sign_params: SubstrateSigningParams) -> Self {
		Self(Arc::new(Mutex::new(SubstrateTargetData {
			client,
			sign_transactions,
			sign_params,
		})))
	}
}

type SubstrateFutureOutput<T> = OwnedTargetFutureOutput<SubstrateHeadersTarget, EthereumHeadersSyncPipeline, T>;

#[async_trait]
impl TargetClient<EthereumHeadersSyncPipeline> for SubstrateHeadersTarget {
	type Error = RpcError;

	async fn best_header_id(&mut self) -> Result<EthereumHeaderId, Self::Error> {
		Ok(self.0.lock().client.best_ethereum_block().await?)
	}

	async fn is_known_header(&mut self, id: EthereumHeaderId) -> Result<(EthereumHeaderId, bool), Self::Error> {
		// TODO: Fix naming
		Ok(self.0.lock().client.ethereum_header_known_high(id).await?)
	}

	async fn submit_headers(
		&mut self,
		headers: Vec<QueuedEthereumHeader>,
	) -> Result<Vec<EthereumHeaderId>, Self::Error> {
		let mut data = self.0.lock();

		let (sign_params, sign_transactions) = (data.sign_params.clone(), data.sign_transactions.clone());
		Ok(data
			.client
			.submit_ethereum_headers(sign_params, headers, sign_transactions)
			.await?)
	}

	async fn incomplete_headers_ids(&mut self) -> Result<HashSet<EthereumHeaderId>, Self::Error> {
		Ok(HashSet::new())
	}

	async fn complete_header(
		&mut self,
		id: EthereumHeaderId,
		_completion: (),
	) -> Result<EthereumHeaderId, Self::Error> {
		Ok(id)
	}

	async fn requires_extra(&mut self, header: QueuedEthereumHeader) -> Result<(EthereumHeaderId, bool), Self::Error> {
		// we can minimize number of receipts_check calls by checking header
		// logs bloom here, but it may give us false positives (when authorities
		// source is contract, we never need any logs)
		// TODO: Fix name
		Ok(self.0.lock().client.ethereum_receipts_required_high(header).await?)
	}
}

/// Run Ethereum headers synchronization.
pub fn run(params: EthereumSyncParams) {
	let mut eth_client = EthereumRpcClient::new(params.eth);
	let mut sub_client = SubstrateRpcClient::new(params.sub);

	let sign_sub_transactions = match params.sync_params.target_tx_mode {
		TargetTransactionMode::Signed | TargetTransactionMode::Backup => true,
		TargetTransactionMode::Unsigned => false,
	};

	// let source = EthereumHeadersSource::new(eth_client);
	let target = SubstrateHeadersTarget::new(sub_client, sign_sub_transactions, params.sub_sign);

	crate::sync_loop::run(
		EthereumHeadersSource { client: eth_client },
		ETHEREUM_TICK_INTERVAL,
		target,
		// SubstrateHeadersTarget {
		// 	client: sub_client,
		// 	sign_transactions: sign_sub_transactions,
		// 	sign_params: params.sub_sign,
		// },
		SUBSTRATE_TICK_INTERVAL,
		params.sync_params,
	);
}
