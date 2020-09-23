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

//! Module for checking Grandpa Finality Proofs.
//!
//! Adapted copy of substrate/client/finality-grandpa/src/justification.rs. If origin
//! will ever be moved to the sp_finality_grandpa, we should reuse that implementation.

use codec::Decode;
use finality_grandpa::{voter_set::VoterSet, Chain, Error as GrandpaError};
use frame_support::RuntimeDebug;
use sp_finality_grandpa::{AuthorityId, AuthoritySignature, SetId};
use sp_runtime::traits::Header as HeaderT;
use sp_std::collections::{btree_map::BTreeMap, btree_set::BTreeSet};
use sp_std::prelude::Vec;

/// Justification verification error.
#[derive(RuntimeDebug, PartialEq)]
pub enum Error {
	/// Failed to decode justification.
	JustificationDecode,
	/// Justification is finalizing unexpected header.
	InvalidJustificationTarget,
	/// Invalid commit in justification.
	InvalidJustificationCommit,
	/// Justification has invalid authority singature.
	InvalidAuthoritySignature,
	/// The justification has precommit for the header that has no route from the target header.
	InvalidPrecommitAncestryProof,
	/// The justification has 'unused' headers in its precommit ancestries.
	InvalidPrecommitAncestries,
}

/// Verify that justification, that is generated by given authority set, finalizes given header.
pub fn verify_justification<Header: HeaderT>(
	finalized_target: (Header::Hash, Header::Number),
	authorities_set_id: SetId,
	authorities_set: VoterSet<AuthorityId>,
	raw_justification: &[u8],
) -> Result<(), Error>
where
	Header::Number: finality_grandpa::BlockNumberOps,
{
	// decode justification first
	let justification =
		GrandpaJustification::<Header>::decode(&mut &raw_justification[..]).map_err(|_| Error::JustificationDecode)?;

	// ensure that it is justification for the expected header
	if (justification.commit.target_hash, justification.commit.target_number) != finalized_target {
		return Err(Error::InvalidJustificationTarget);
	}

	// validate commit of the justification (it just assumes all signatures are valid)
	let ancestry_chain = AncestryChain::new(&justification.votes_ancestries);
	match finality_grandpa::validate_commit(&justification.commit, &authorities_set, &ancestry_chain) {
		Ok(ref result) if result.ghost().is_some() => {}
		_ => return Err(Error::InvalidJustificationCommit),
	}

	// now that we know that the commit is correct, check authorities signatures
	let mut buf = Vec::new();
	let mut visited_hashes = BTreeSet::new();
	for signed in &justification.commit.precommits {
		if !sp_finality_grandpa::check_message_signature_with_buffer(
			&finality_grandpa::Message::Precommit(signed.precommit.clone()),
			&signed.id,
			&signed.signature,
			justification.round,
			authorities_set_id,
			&mut buf,
		) {
			return Err(Error::InvalidAuthoritySignature);
		}

		if justification.commit.target_hash == signed.precommit.target_hash {
			continue;
		}

		match ancestry_chain.ancestry(justification.commit.target_hash, signed.precommit.target_hash) {
			Ok(route) => {
				// ancestry starts from parent hash but the precommit target hash has been visited
				visited_hashes.insert(signed.precommit.target_hash);
				visited_hashes.extend(route);
			}
			_ => {
				// could this happen in practice? I don't think so, but original code has this check
				return Err(Error::InvalidPrecommitAncestryProof);
			}
		}
	}

	let ancestry_hashes = justification
		.votes_ancestries
		.iter()
		.map(|h: &Header| h.hash())
		.collect();
	if visited_hashes != ancestry_hashes {
		return Err(Error::InvalidPrecommitAncestries);
	}

	Ok(())
}

/// GRANDPA justification of the bridged chain
#[derive(Decode, RuntimeDebug)]
#[cfg_attr(test, derive(codec::Encode))]
struct GrandpaJustification<Header: HeaderT> {
	round: u64,
	commit: finality_grandpa::Commit<Header::Hash, Header::Number, AuthoritySignature, AuthorityId>,
	votes_ancestries: Vec<Header>,
}

/// A utility trait implementing `finality_grandpa::Chain` using a given set of headers.
struct AncestryChain<Header: HeaderT> {
	ancestry: BTreeMap<Header::Hash, Header::Hash>,
}

impl<Header: HeaderT> AncestryChain<Header> {
	fn new(ancestry: &[Header]) -> AncestryChain<Header> {
		AncestryChain {
			ancestry: ancestry
				.iter()
				.map(|header| (header.hash(), *header.parent_hash()))
				.collect(),
		}
	}
}

impl<Header: HeaderT> finality_grandpa::Chain<Header::Hash, Header::Number> for AncestryChain<Header>
where
	Header::Number: finality_grandpa::BlockNumberOps,
{
	fn ancestry(&self, base: Header::Hash, block: Header::Hash) -> Result<Vec<Header::Hash>, GrandpaError> {
		let mut route = Vec::new();
		let mut current_hash = block;
		loop {
			if current_hash == base {
				break;
			}
			match self.ancestry.get(&current_hash).cloned() {
				Some(parent_hash) => {
					current_hash = parent_hash;
					route.push(current_hash);
				}
				_ => return Err(GrandpaError::NotDescendent),
			}
		}
		route.pop(); // remove the base

		Ok(route)
	}

	fn best_chain_containing(&self, _block: Header::Hash) -> Option<(Header::Hash, Header::Number)> {
		unreachable!("is only used during voting; qed")
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use codec::Encode;
	use sp_core::H256;
	use sp_keyring::Ed25519Keyring;
	use sp_runtime::traits::BlakeTwo256;

	const TEST_GRANDPA_ROUND: u64 = 1;
	const TEST_GRANDPA_SET_ID: SetId = 1;

	type TestHeader = sp_runtime::generic::Header<u64, BlakeTwo256>;

	fn header(index: u8) -> TestHeader {
		TestHeader::new(
			index as _,
			Default::default(),
			Default::default(),
			if index == 0 {
				Default::default()
			} else {
				header(index - 1).hash()
			},
			Default::default(),
		)
	}

	fn header_id(index: u8) -> (H256, u64) {
		(header(index).hash(), index as _)
	}

	fn authorities_set() -> VoterSet<AuthorityId> {
		VoterSet::new(vec![
			(Ed25519Keyring::Alice.public().into(), 1),
			(Ed25519Keyring::Bob.public().into(), 1),
			(Ed25519Keyring::Charlie.public().into(), 1),
		])
		.unwrap()
	}

	fn signed_precommit(
		signer: Ed25519Keyring,
		target: (H256, u64),
	) -> finality_grandpa::SignedPrecommit<H256, u64, AuthoritySignature, AuthorityId> {
		let precommit = finality_grandpa::Precommit {
			target_hash: target.0,
			target_number: target.1,
		};
		let encoded = sp_finality_grandpa::localized_payload(
			TEST_GRANDPA_ROUND,
			TEST_GRANDPA_SET_ID,
			&finality_grandpa::Message::Precommit(precommit.clone()),
		);
		let signature = signer.sign(&encoded[..]).into();
		finality_grandpa::SignedPrecommit {
			precommit,
			signature,
			id: signer.public().into(),
		}
	}

	fn make_justification_for_header_1() -> GrandpaJustification<TestHeader> {
		GrandpaJustification {
			round: TEST_GRANDPA_ROUND,
			commit: finality_grandpa::Commit {
				target_hash: header_id(1).0,
				target_number: header_id(1).1,
				precommits: vec![
					signed_precommit(Ed25519Keyring::Alice, header_id(2)),
					signed_precommit(Ed25519Keyring::Bob, header_id(3)),
					signed_precommit(Ed25519Keyring::Charlie, header_id(4)),
				],
			},
			votes_ancestries: vec![header(2), header(3), header(4)],
		}
	}

	#[test]
	fn justification_with_invalid_encoding_rejected() {
		assert_eq!(
			verify_justification::<TestHeader>(header_id(1), TEST_GRANDPA_SET_ID, authorities_set(), &[],),
			Err(Error::JustificationDecode),
		);
	}

	#[test]
	fn justification_with_invalid_target_rejected() {
		assert_eq!(
			verify_justification::<TestHeader>(
				header_id(2),
				TEST_GRANDPA_SET_ID,
				authorities_set(),
				&make_justification_for_header_1().encode(),
			),
			Err(Error::InvalidJustificationTarget),
		);
	}

	#[test]
	fn justification_with_invalid_commit_rejected() {
		let mut justification = make_justification_for_header_1();
		justification.commit.precommits.clear();

		assert_eq!(
			verify_justification::<TestHeader>(
				header_id(1),
				TEST_GRANDPA_SET_ID,
				authorities_set(),
				&justification.encode(),
			),
			Err(Error::InvalidJustificationCommit),
		);
	}

	#[test]
	fn justification_with_invalid_authority_signature_rejected() {
		let mut justification = make_justification_for_header_1();
		justification.commit.precommits[0].signature = Default::default();

		assert_eq!(
			verify_justification::<TestHeader>(
				header_id(1),
				TEST_GRANDPA_SET_ID,
				authorities_set(),
				&justification.encode(),
			),
			Err(Error::InvalidAuthoritySignature),
		);
	}

	#[test]
	fn justification_with_invalid_precommit_ancestry() {
		let mut justification = make_justification_for_header_1();
		justification.votes_ancestries.push(header(10));

		assert_eq!(
			verify_justification::<TestHeader>(
				header_id(1),
				TEST_GRANDPA_SET_ID,
				authorities_set(),
				&justification.encode(),
			),
			Err(Error::InvalidPrecommitAncestries),
		);
	}

	#[test]
	fn valid_justification_accepted() {
		assert_eq!(
			verify_justification::<TestHeader>(
				header_id(1),
				TEST_GRANDPA_SET_ID,
				authorities_set(),
				&make_justification_for_header_1().encode(),
			),
			Ok(()),
		);
	}
}
