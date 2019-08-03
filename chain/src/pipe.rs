// Copyright 2018 The Kepler Developers
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Implementation of the chain block acceptance (or refusal) pipeline.

use crate::core::consensus;
use crate::core::core::hash::Hashed;
use crate::core::core::verifier_cache::VerifierCache;
use crate::core::core::Committed;
use crate::core::core::{Block, BlockHeader, BlockSums};
use crate::core::pow;
use crate::error::{Error, ErrorKind};
use crate::store;
use crate::txhashset;
use crate::types::{Options, Tip};
use crate::util::RwLock;
use chrono::prelude::Utc;
use chrono::Duration;
use kepler_store;
use std::sync::Arc;

/// Contextual information required to process a new block and either reject or
/// accept it.
pub struct BlockContext<'a> {
	/// The options
	pub opts: Options,
	/// The pow verifier to use when processing a block.
	pub pow_verifier: fn(&BlockHeader) -> Result<(), pow::Error>,
	/// The active txhashset (rewindable MMRs) to use for block processing.
	pub txhashset: &'a mut txhashset::TxHashSet,
	/// The active batch to use for block processing.
	pub batch: store::Batch<'a>,
	/// The verifier cache (caching verifier for rangeproofs and kernel signatures)
	pub verifier_cache: Arc<RwLock<dyn VerifierCache>>,
}

// Check if we already know about this block for various reasons
// from cheapest to most expensive (delay hitting the db until last).
fn check_known(header: &BlockHeader, ctx: &mut BlockContext<'_>) -> Result<(), Error> {
	check_known_head(header, ctx)?;
	check_known_store(header, ctx)?;
	Ok(())
}

// Validate only the proof of work in a block header.
// Used to cheaply validate orphans in process_block before adding them to OrphanBlockPool.
fn validate_pow_only(header: &BlockHeader, ctx: &mut BlockContext<'_>) -> Result<(), Error> {
	if !header.pow.is_primary() && !header.pow.is_secondary() {
		return Err(ErrorKind::LowEdgebits.into());
	}
	let edge_bits = header.pow.edge_bits();
	if !(ctx.pow_verifier)(header).is_ok() {
		error!(
			"pipe: error validating header with cuckoo edge_bits {}",
			edge_bits
		);
		return Err(ErrorKind::InvalidPow.into());
	}
	Ok(())
}

/// Runs the block processing pipeline, including validation and finding a
/// place for the new block in the chain.
/// Returns new head if chain head updated.
pub fn process_block(b: &Block, ctx: &mut BlockContext<'_>) -> Result<Option<Tip>, Error> {
	debug!(
		"pipe: process_block {} at {} [in/out/kern: {}/{}/{}]",
		b.hash(),
		b.header.height,
		b.inputs().len(),
		b.outputs().len(),
		b.kernels().len(),
	);

	// Check if we have already processed this block previously.
	check_known(&b.header, ctx)?;

	let head = ctx.batch.head()?;
	let header_head = ctx.batch.header_head()?;

	let is_next = b.header.prev_hash == head.last_block_h;

	// Block is an orphan if we do not know about the previous full block.
	// Skip this check if we have just processed the previous block
	// or the full txhashset state (fast sync) at the previous block height.
	let prev = prev_header_store(&b.header, &mut ctx.batch)?;
	if !is_next && !ctx.batch.block_exists(&prev.hash())? {
		// Validate the proof of work of the orphan block to prevent adding
		// invalid blocks to OrphanBlockPool.
		validate_pow_only(&b.header, ctx)?;

		return Err(ErrorKind::Orphan.into());
	}

	// Process the header for the block.
	// Note: We still want to process the full block if we have seen this header before
	// as we may have processed it "header first" and not yet processed the full block.
	process_block_header(&b.header, ctx)?;

	// Validate the block itself, make sure it is internally consistent.
	// Use the verifier_cache for verifying rangeproofs and kernel signatures.
	validate_block(b, ctx)?;

	// Start a chain extension unit of work dependent on the success of the
	// internal validation and saving operations
	let block_sums = txhashset::extending(&mut ctx.txhashset, &mut ctx.batch, |mut extension| {
		rewind_and_apply_fork(&prev, &header_head, extension)?;

		// Check any coinbase being spent have matured sufficiently.
		// This needs to be done within the context of a potentially
		// rewound txhashset extension to reflect chain state prior
		// to applying the new block.
		verify_coinbase_maturity(b, &mut extension)?;

		// Validate the block against the UTXO set.
		validate_utxo(b, &mut extension)?;

		// Using block_sums (utxo_sum, kernel_sum) for the previous block from the db
		// we can verify_kernel_sums across the full UTXO sum and full kernel sum
		// accounting for inputs/outputs/kernels in this new block.
		// We know there are no double-spends etc. if this verifies successfully.
		// Remember to save these to the db later on (regardless of extension rollback)
		let block_sums = verify_block_sums(b, &extension.batch)?;

		// Apply the block to the txhashset state.
		// Validate the txhashset roots and sizes against the block header.
		// Block is invalid if there are any discrepencies.
		apply_block_to_txhashset(b, &mut extension)?;

		// If applying this block does not increase the work on the chain then
		// we know we have not yet updated the chain to produce a new chain head.
		let head = extension.batch.head()?;
		if !has_more_work(&b.header, &head) {
			extension.force_rollback();
		}

		Ok(block_sums)
	})?;

	// Add the validated block to the db along with the corresponding block_sums.
	// We do this even if we have not increased the total cumulative work
	// so we can maintain multiple (in progress) forks.
	add_block(b, &block_sums, &ctx.batch)?;

	// If we have no "tail" then set it now.
	if ctx.batch.tail().is_err() {
		update_body_tail(&b.header, &ctx.batch)?;
	}

	if has_more_work(&b.header, &head) {
		let head = Tip::from_header(&b.header);
		update_head(&head, &mut ctx.batch)?;
		Ok(Some(head))
	} else {
		Ok(None)
	}
}

/// Sync a chunk of block headers.
/// This is only used during header sync.
pub fn sync_block_headers(
	headers: &[BlockHeader],
	ctx: &mut BlockContext<'_>,
) -> Result<(), Error> {
	if headers.is_empty() {
		return Ok(());
	}

	let first_header = headers.first().expect("first header");
	let last_header = headers.last().expect("last header");
	let prev_header = ctx.batch.get_previous_header(&first_header)?;

	// Check if we know about all these headers. If so we can accept them quickly.
	// If they *do not* increase total work on the sync chain we are done.
	// If they *do* increase total work then we should process them to update sync_head.
	let sync_head = ctx.batch.get_sync_head()?;
	if let Ok(existing) = ctx.batch.get_block_header(&last_header.hash()) {
		if !has_more_work(&existing, &sync_head) {
			return Ok(());
		}
	}

	txhashset::sync_extending(&mut ctx.txhashset, &mut ctx.batch, |extension| {
		rewind_and_apply_header_fork(&prev_header, extension)?;
		for header in headers {
			extension.validate_root(header)?;
			extension.apply_header(header)?;
			add_block_header(header, &extension.batch)?;
		}
		Ok(())
	})?;

	// Validate all our headers now that we have added each "previous"
	// header to the db in this batch above.
	for header in headers {
		validate_header(header, ctx)?;
	}

	if has_more_work(&last_header, &sync_head) {
		update_sync_head(&Tip::from_header(&last_header), &mut ctx.batch)?;
	}

	Ok(())
}

/// Process a block header. Update the header MMR and corresponding header_head if this header
/// increases the total work relative to header_head.
/// Note: In contrast to processing a full block we treat "already known" as success
/// to allow processing to continue (for header itself).
pub fn process_block_header(header: &BlockHeader, ctx: &mut BlockContext<'_>) -> Result<(), Error> {
	// Check this header is not an orphan, we must know about the previous header to continue.
	let prev_header = ctx.batch.get_previous_header(&header)?;

	// If this header is "known" then stop processing the header.
	// Do not stop processing with an error though.
	if check_known(header, ctx).is_err() {
		return Ok(());
	}

	// If we have not yet seen the full block then check if we have seen this header.
	// If it does not increase total_difficulty beyond our current header_head
	// then we can (re)accept this header and process the full block (or request it).
	// This header is on a fork and we should still accept it as the fork may eventually win.
	let header_head = ctx.batch.header_head()?;
	if let Ok(existing) = ctx.batch.get_block_header(&header.hash()) {
		if !has_more_work(&existing, &header_head) {
			return Ok(());
		}
	}

	txhashset::header_extending(&mut ctx.txhashset, &mut ctx.batch, |extension| {
		rewind_and_apply_header_fork(&prev_header, extension)?;
		extension.validate_root(header)?;
		extension.apply_header(header)?;
		if !has_more_work(&header, &header_head) {
			extension.force_rollback();
		}
		Ok(())
	})?;

	validate_header(header, ctx)?;
	add_block_header(header, &ctx.batch)?;

	// Update header_head independently of chain head (full blocks).
	// If/when we process the corresponding full block we will update the
	// chain head to match. This allows our header chain to extend safely beyond
	// the full chain in a fork scenario without needing excessive rewinds to handle
	// the temporarily divergent chains.
	if has_more_work(&header, &header_head) {
		update_header_head(&Tip::from_header(&header), &mut ctx.batch)?;
	}

	Ok(())
}

/// Quick in-memory check to fast-reject any block handled recently.
/// Keeps duplicates from the network in check.
/// Checks against the last_block_h and prev_block_h of the chain head.
fn check_known_head(header: &BlockHeader, ctx: &mut BlockContext<'_>) -> Result<(), Error> {
	let head = ctx.batch.head()?;
	let bh = header.hash();
	if bh == head.last_block_h || bh == head.prev_block_h {
		return Err(ErrorKind::Unfit("already known in head".to_string()).into());
	}
	Ok(())
}

// Check if this block is in the store already.
fn check_known_store(header: &BlockHeader, ctx: &mut BlockContext<'_>) -> Result<(), Error> {
	match ctx.batch.block_exists(&header.hash()) {
		Ok(true) => {
			let head = ctx.batch.head()?;
			if header.height < head.height.saturating_sub(50) {
				// TODO - we flag this as an "abusive peer" but only in the case
				// where we have the full block in our store.
				// So this is not a particularly exhaustive check.
				Err(ErrorKind::OldBlock.into())
			} else {
				Err(ErrorKind::Unfit("already known in store".to_string()).into())
			}
		}
		Ok(false) => {
			// Not yet processed this block, we can proceed.
			Ok(())
		}
		Err(e) => {
			return Err(ErrorKind::StoreErr(e, "pipe get this block".to_owned()).into());
		}
	}
}

// Find the previous header from the store.
// Return an Orphan error if we cannot find the previous header.
fn prev_header_store(
	header: &BlockHeader,
	batch: &mut store::Batch<'_>,
) -> Result<BlockHeader, Error> {
	let prev = batch.get_previous_header(&header).map_err(|e| match e {
		kepler_store::Error::NotFoundErr(_) => ErrorKind::Orphan,
		_ => ErrorKind::StoreErr(e, "check prev header".into()),
	})?;
	Ok(prev)
}

/// First level of block validation that only needs to act on the block header
/// to make it as cheap as possible. The different validations are also
/// arranged by order of cost to have as little DoS surface as possible.
fn validate_header(header: &BlockHeader, ctx: &mut BlockContext<'_>) -> Result<(), Error> {
	// check version, enforces scheduled hard fork
	if !consensus::valid_header_version(header.height, header.version) {
		error!(
			"Invalid block header version received ({:?}), maybe update Kepler?",
			header.version
		);
		return Err(ErrorKind::InvalidBlockVersion(header.version).into());
	}

	if header.timestamp > Utc::now() + Duration::seconds(12 * (consensus::BLOCK_TIME_SEC as i64)) {
		// refuse blocks more than 12 blocks intervals in future (as in bitcoin)
		// TODO add warning in p2p code if local time is too different from peers
		return Err(ErrorKind::InvalidBlockTime.into());
	}

	if !ctx.opts.contains(Options::SKIP_POW) {
		if !header.pow.is_primary() && !header.pow.is_secondary() {
			return Err(ErrorKind::LowEdgebits.into());
		}
		let edge_bits = header.pow.edge_bits();
		if !(ctx.pow_verifier)(header).is_ok() {
			error!(
				"pipe: error validating header with cuckoo edge_bits {}",
				edge_bits
			);
			return Err(ErrorKind::InvalidPow.into());
		}
	}

	// First I/O cost, delayed as late as possible.
	let prev = prev_header_store(header, &mut ctx.batch)?;

	// make sure this header has a height exactly one higher than the previous
	// header
	if header.height != prev.height + 1 {
		return Err(ErrorKind::InvalidBlockHeight.into());
	}

	if header.timestamp <= prev.timestamp {
		// prevent time warp attacks and some timestamp manipulations by forcing strict
		// time progression
		return Err(ErrorKind::InvalidBlockTime.into());
	}

	// verify the proof of work and related parameters
	// at this point we have a previous block header
	// we know the height increased by one
	// so now we can check the total_difficulty increase is also valid
	// check the pow hash shows a difficulty at least as large
	// as the target difficulty
	if !ctx.opts.contains(Options::SKIP_POW) {
		if header.total_difficulty() <= prev.total_difficulty() {
			return Err(ErrorKind::DifficultyTooLow.into());
		}

		let target_difficulty = header.total_difficulty() - prev.total_difficulty();

		if header.pow.to_difficulty(header.height) < target_difficulty {
			return Err(ErrorKind::DifficultyTooLow.into());
		}

		// explicit check to ensure total_difficulty has increased by exactly
		// the _network_ difficulty of the previous block
		// (during testnet1 we use _block_ difficulty here)
		let child_batch = ctx.batch.child()?;
		let diff_iter = store::DifficultyIter::from_batch(prev.hash(), child_batch);
		let next_header_info = consensus::next_difficulty(header.height, diff_iter);
		if target_difficulty != next_header_info.difficulty {
			info!(
				"validate_header: header target difficulty {} != {}",
				target_difficulty.to_num(),
				next_header_info.difficulty.to_num()
			);
			return Err(ErrorKind::WrongTotalDifficulty.into());
		}
		// check the secondary PoW scaling factor if applicable
		if header.pow.secondary_scaling != next_header_info.secondary_scaling {
			info!(
				"validate_header: header secondary scaling {} != {}",
				header.pow.secondary_scaling, next_header_info.secondary_scaling
			);
			return Err(ErrorKind::InvalidScaling.into());
		}
	}

	Ok(())
}

fn validate_block(block: &Block, ctx: &mut BlockContext<'_>) -> Result<(), Error> {
	let prev = ctx.batch.get_previous_header(&block.header)?;
	block
		.validate(&prev.total_kernel_offset, ctx.verifier_cache.clone())
		.map_err(|e| ErrorKind::InvalidBlockProof(e))?;
	Ok(())
}

/// Verify the block is not spending coinbase outputs before they have sufficiently matured.
fn verify_coinbase_maturity(block: &Block, ext: &txhashset::Extension<'_>) -> Result<(), Error> {
	ext.utxo_view()
		.verify_coinbase_maturity(&block.inputs(), block.header.height)
}

/// Verify kernel sums across the full utxo and kernel sets based on block_sums
/// of previous block accounting for the inputs|outputs|kernels of the new block.
fn verify_block_sums(b: &Block, batch: &store::Batch<'_>) -> Result<BlockSums, Error> {
	// Retrieve the block_sums for the previous block.
	let block_sums = batch.get_block_sums(&b.header.prev_hash)?;

	// Overage is based purely on the new block.
	// Previous block_sums have taken all previous overage into account.
	let overage = b.header.overage();

	// Offset on the other hand is the total kernel offset from the new block.
	let offset = b.header.total_kernel_offset();

	// Verify the kernel sums for the block_sums with the new block applied.
	let (utxo_sum, kernel_sum) =
		(block_sums, b as &dyn Committed).verify_kernel_sums(overage, offset)?;

	Ok(BlockSums {
		utxo_sum,
		kernel_sum,
	})
}

/// Fully validate the block by applying it to the txhashset extension.
/// Check both the txhashset roots and sizes are correct after applying the block.
fn apply_block_to_txhashset(
	block: &Block,
	ext: &mut txhashset::Extension<'_>,
) -> Result<(), Error> {
	ext.validate_header_root(&block.header)?;
	ext.apply_block(block)?;
	ext.validate_roots()?;
	ext.validate_sizes()?;
	Ok(())
}

/// Officially adds the block to our chain.
/// Header must be added separately (assume this has been done previously).
fn add_block(b: &Block, block_sums: &BlockSums, batch: &store::Batch<'_>) -> Result<(), Error> {
	batch
		.save_block(b)
		.map_err(|e| ErrorKind::StoreErr(e, "pipe save block".to_owned()))?;
	batch.save_block_sums(&b.hash(), block_sums)?;
	Ok(())
}

/// Update the block chain tail so we can know the exact tail of full blocks in this node
fn update_body_tail(bh: &BlockHeader, batch: &store::Batch<'_>) -> Result<(), Error> {
	let tip = Tip::from_header(bh);
	batch
		.save_body_tail(&tip)
		.map_err(|e| ErrorKind::StoreErr(e, "pipe save body tail".to_owned()))?;
	debug!("body tail {} @ {}", bh.hash(), bh.height);
	Ok(())
}

/// Officially adds the block header to our header chain.
fn add_block_header(bh: &BlockHeader, batch: &store::Batch<'_>) -> Result<(), Error> {
	batch
		.save_block_header(bh)
		.map_err(|e| ErrorKind::StoreErr(e, "pipe save header".to_owned()))?;
	Ok(())
}

fn update_head(head: &Tip, batch: &mut store::Batch<'_>) -> Result<(), Error> {
	batch
		.save_body_head(&head)
		.map_err(|e| ErrorKind::StoreErr(e, "pipe save body".to_owned()))?;

	debug!("head updated to {} at {}", head.last_block_h, head.height);

	Ok(())
}

// Whether the provided block totals more work than the chain tip
fn has_more_work(header: &BlockHeader, head: &Tip) -> bool {
	header.total_difficulty() > head.total_difficulty
}

/// Update the sync head so we can keep syncing from where we left off.
fn update_sync_head(head: &Tip, batch: &mut store::Batch<'_>) -> Result<(), Error> {
	batch
		.save_sync_head(&head)
		.map_err(|e| ErrorKind::StoreErr(e, "pipe save sync head".to_owned()))?;
	debug!(
		"sync_head updated to {} at {}",
		head.last_block_h, head.height
	);
	Ok(())
}

/// Update the header_head.
fn update_header_head(head: &Tip, batch: &mut store::Batch<'_>) -> Result<(), Error> {
	batch
		.save_header_head(&head)
		.map_err(|e| ErrorKind::StoreErr(e, "pipe save header head".to_owned()))?;
	debug!(
		"header_head updated to {} at {}",
		head.last_block_h, head.height
	);
	Ok(())
}

/// Rewind the header chain and reapply headers on a fork.
pub fn rewind_and_apply_header_fork(
	header: &BlockHeader,
	ext: &mut txhashset::HeaderExtension<'_>,
) -> Result<(), Error> {
	let head = ext.head();
	if header.hash() == head.last_block_h {
		// Nothing to rewind and nothing to reapply. Done.
		return Ok(());
	}

	let mut fork_hashes = vec![];
	let mut current = header.clone();
	while current.height > 0 && !ext.is_on_current_chain(&current).is_ok() {
		fork_hashes.push(current.hash());
		current = ext.batch.get_previous_header(&current)?;
	}
	fork_hashes.reverse();

	let forked_header = current;

	// Rewind the txhashset state back to the block where we forked from the most work chain.
	ext.rewind(&forked_header)?;

	// Re-apply all headers on this fork.
	for h in fork_hashes {
		let header = ext
			.batch
			.get_block_header(&h)
			.map_err(|e| ErrorKind::StoreErr(e, format!("getting forked headers")))?;
		ext.apply_header(&header)?;
	}

	Ok(())
}

/// Utility function to handle forks. From the forked block, jump backward
/// to find to fork point. Rewind the txhashset to the fork point and apply all
/// necessary blocks prior to the one being processed to set the txhashset in
/// the expected state.
pub fn rewind_and_apply_fork(
	header: &BlockHeader,
	header_head: &Tip,
	ext: &mut txhashset::Extension<'_>,
) -> Result<(), Error> {
	// TODO - Skip the "rewind and reapply" if everything is aligned and this is the "next" block.
	// This will be significantly easier once we break out the header extension.

	// Find the fork point where head and header_head diverge.
	// We may need to rewind back to this fork point if they diverged
	// prior to the fork point for the provided header.
	let header_forked_header = {
		let mut current = ext.batch.get_block_header(&header_head.last_block_h)?;
		while current.height > 0 && !ext.is_on_current_chain(&current).is_ok() {
			current = ext.batch.get_previous_header(&current)?;
		}
		current
	};

	// Find the fork point where the provided header diverges from our main chain.
	// Account for the header fork point. Use the earliest fork point to determine
	// where we need to rewind to. We need to do this
	let (forked_header, fork_hashes) = {
		let mut fork_hashes = vec![];
		let mut current = header.clone();
		while current.height > 0
			&& (!ext.is_on_current_chain(&current).is_ok()
				|| current.height > header_forked_header.height)
		{
			fork_hashes.push(current.hash());
			current = ext.batch.get_previous_header(&current)?;
		}
		fork_hashes.reverse();

		(current, fork_hashes)
	};

	// Rewind the txhashset state back to the block where we forked from the most work chain.
	ext.rewind(&forked_header)?;

	// Now re-apply all blocks on this fork.
	for h in fork_hashes {
		let fb = ext
			.batch
			.get_block(&h)
			.map_err(|e| ErrorKind::StoreErr(e, format!("getting forked blocks")))?;

		// Re-verify coinbase maturity along this fork.
		verify_coinbase_maturity(&fb, ext)?;
		// Validate the block against the UTXO set.
		validate_utxo(&fb, ext)?;
		// Re-verify block_sums to set the block_sums up on this fork correctly.
		verify_block_sums(&fb, &ext.batch)?;
		// Re-apply the blocks.
		apply_block_to_txhashset(&fb, ext)?;
	}

	Ok(())
}

fn validate_utxo(block: &Block, ext: &txhashset::Extension<'_>) -> Result<(), Error> {
	ext.utxo_view().validate_block(block)
}
