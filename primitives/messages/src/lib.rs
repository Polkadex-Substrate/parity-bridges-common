// Copyright 2019-2021 Parity Technologies (UK) Ltd.
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

//! Primitives of messages module.

#![cfg_attr(not(feature = "std"), no_std)]
// RuntimeApi generated functions
#![allow(clippy::too_many_arguments)]

use bp_header_chain::HeaderChainError;
use bp_runtime::{
	messages::MessageDispatchResult, AccountIdOf, BasicOperatingMode, Chain, HashOf, OperatingMode,
	RangeInclusiveExt, StorageProofError, UnderlyingChainOf, UnderlyingChainProvider,
};
use codec::{Decode, Encode, MaxEncodedLen};
use frame_support::{PalletError, RuntimeDebug};
// Weight is reexported to avoid additional frame-support dependencies in related crates.
pub use frame_support::weights::Weight;
use scale_info::TypeInfo;
use serde::{Deserialize, Serialize};
use source_chain::RelayersRewards;
use sp_core::{TypeId, H256};
use sp_io::hashing::blake2_256;
use sp_std::{collections::vec_deque::VecDeque, ops::RangeInclusive, prelude::*};

pub mod source_chain;
pub mod storage_keys;
pub mod target_chain;

/// Hard limit on message size that can be sent over the bridge.
pub const HARD_MESSAGE_SIZE_LIMIT: u32 = 64 * 1024;

/// Substrate-based chain with messaging support.
pub trait ChainWithMessages: Chain {
	/// Name of the bridge messages pallet (used in `construct_runtime` macro call) that is
	/// deployed at some other chain to bridge with this `ChainWithMessages`.
	///
	/// We assume that all chains that are bridging with this `ChainWithMessages` are using
	/// the same name.
	const WITH_CHAIN_MESSAGES_PALLET_NAME: &'static str;

	/// Maximal number of unrewarded relayers in a single confirmation transaction at this
	/// `ChainWithMessages`. Unrewarded means that the relayer has delivered messages, but
	/// either confirmations haven't been delivered back to the source chain, or we haven't
	/// received reward confirmations yet.
	///
	/// This constant limits maximal number of entries in the `InboundLaneData::relayers`. Keep
	/// in mind that the same relayer account may take several (non-consecutive) entries in this
	/// set.
	const MAX_UNREWARDED_RELAYERS_IN_CONFIRMATION_TX: MessageNonce;
	/// Maximal number of unconfirmed messages in a single confirmation transaction at this
	/// `ChainWithMessages`. Unconfirmed means that the
	/// message has been delivered, but either confirmations haven't been delivered back to the
	/// source chain, or we haven't received reward confirmations for these messages yet.
	///
	/// This constant limits difference between last message from last entry of the
	/// `InboundLaneData::relayers` and first message at the first entry.
	///
	/// There is no point of making this parameter lesser than
	/// `MAX_UNREWARDED_RELAYERS_IN_CONFIRMATION_TX`, because then maximal number of relayer entries
	/// will be limited by maximal number of messages.
	///
	/// This value also represents maximal number of messages in single delivery transaction.
	/// Transaction that is declaring more messages than this value, will be rejected. Even if
	/// these messages are from different lanes.
	const MAX_UNCONFIRMED_MESSAGES_IN_CONFIRMATION_TX: MessageNonce;

	/// Return maximal dispatch weight of the message we're able to receive.
	fn maximal_incoming_message_dispatch_weight() -> Weight {
		// we leave 1/2 of `max_extrinsic_weight` for the delivery transaction itself
		Self::max_extrinsic_weight() / 2
	}

	/// Return maximal size of the message we're able to receive.
	fn maximal_incoming_message_size() -> u32 {
		maximal_incoming_message_size(Self::max_extrinsic_size())
	}
}

/// Return maximal size of the message the chain with `max_extrinsic_size` is able to receive.
pub fn maximal_incoming_message_size(max_extrinsic_size: u32) -> u32 {
	// The maximal size of extrinsic at Substrate-based chain depends on the
	// `frame_system::Config::MaximumBlockLength` and
	// `frame_system::Config::AvailableBlockRatio` constants. This check is here to be sure that
	// the lane won't stuck because message is too large to fit into delivery transaction.
	//
	// **IMPORTANT NOTE**: the delivery transaction contains storage proof of the message, not
	// the message itself. The proof is always larger than the message. But unless chain state
	// is enormously large, it should be several dozens/hundreds of bytes. The delivery
	// transaction also contains signatures and signed extensions. Because of this, we reserve
	// 1/3 of the the maximal extrinsic size for this data.
	//
	// **ANOTHER IMPORTANT NOTE**: large message means not only larger proofs and heavier
	// proof verification, but also heavier message decoding and dispatch. So we have a hard
	// limit of `64Kb`, which in practice limits the message size on all chains. Without this
	// limit the **weight** (not the size) of the message will be higher than the
	// `Self::maximal_incoming_message_dispatch_weight()`.

	sp_std::cmp::min(max_extrinsic_size / 3 * 2, HARD_MESSAGE_SIZE_LIMIT)
}

impl<T> ChainWithMessages for T
where
	T: Chain + UnderlyingChainProvider,
	UnderlyingChainOf<T>: ChainWithMessages,
{
	const WITH_CHAIN_MESSAGES_PALLET_NAME: &'static str =
		UnderlyingChainOf::<T>::WITH_CHAIN_MESSAGES_PALLET_NAME;
	const MAX_UNREWARDED_RELAYERS_IN_CONFIRMATION_TX: MessageNonce =
		UnderlyingChainOf::<T>::MAX_UNREWARDED_RELAYERS_IN_CONFIRMATION_TX;
	const MAX_UNCONFIRMED_MESSAGES_IN_CONFIRMATION_TX: MessageNonce =
		UnderlyingChainOf::<T>::MAX_UNCONFIRMED_MESSAGES_IN_CONFIRMATION_TX;
}

/// Messages pallet operating mode.
#[derive(
	Encode,
	Decode,
	Clone,
	Copy,
	PartialEq,
	Eq,
	RuntimeDebug,
	TypeInfo,
	MaxEncodedLen,
	Serialize,
	Deserialize,
)]
pub enum MessagesOperatingMode {
	/// Basic operating mode (Normal/Halted)
	Basic(BasicOperatingMode),
	/// The pallet is not accepting outbound messages. Inbound messages and receiving proofs
	/// are still accepted.
	///
	/// This mode may be used e.g. when bridged chain expects upgrade. Then to avoid dispatch
	/// failures, the pallet owner may stop accepting new messages, while continuing to deliver
	/// queued messages to the bridged chain. Once upgrade is completed, the mode may be switched
	/// back to `Normal`.
	RejectingOutboundMessages,
}

impl Default for MessagesOperatingMode {
	fn default() -> Self {
		MessagesOperatingMode::Basic(BasicOperatingMode::Normal)
	}
}

impl OperatingMode for MessagesOperatingMode {
	fn is_halted(&self) -> bool {
		match self {
			Self::Basic(operating_mode) => operating_mode.is_halted(),
			_ => false,
		}
	}
}

/// Bridge lane identifier.
///
/// Lane connects two endpoints at both sides of the bridge. We assume that every endpoint
/// has its own unique identifier. We want lane identifiers to be the same on the both sides
/// of the bridge (and naturally unique across global consensus if endpoints have unique
/// identifiers). So lane id is the hash (`blake2_256`) of **ordered** encoded locations
/// concatenation (separated by some binary data). I.e.:
///
/// ```nocompile
/// let endpoint1 = X2(GlobalConsensus(NetworkId::Rococo), Parachain(42));
/// let endpoint2 = X2(GlobalConsensus(NetworkId::Wococo), Parachain(777));
///
/// let final_lane_key = if endpoint1 < endpoint2 {
///     (endpoint1, VALUES_SEPARATOR, endpoint2)
/// } else {
///     (endpoint2, VALUES_SEPARATOR, endpoint1)
/// }.using_encoded(blake2_256);
/// ```
#[derive(
	Clone,
	Copy,
	Decode,
	Default,
	Encode,
	Eq,
	Ord,
	PartialOrd,
	PartialEq,
	TypeInfo,
	MaxEncodedLen,
	Serialize,
	Deserialize,
)]
pub struct LaneId(H256);

impl LaneId {
	/// Create lane identifier from two locations.
	pub fn new<T: Ord + Encode>(endpoint1: T, endpoint2: T) -> Self {
		const VALUES_SEPARATOR: [u8; 31] = *b"bridges-lane-id-value-separator";

		LaneId(
			if endpoint1 < endpoint2 {
				(endpoint1, VALUES_SEPARATOR, endpoint2)
			} else {
				(endpoint2, VALUES_SEPARATOR, endpoint1)
			}
			.using_encoded(blake2_256)
			.into(),
		)
	}

	/// Create lane identifier from given hash.
	///
	/// There's no `From<H256>` implementation for the `LaneId`, because using this conversion
	/// in a wrong way (i.e. computing hash of endpoints manually) may lead to issues. So we
	/// want the call to be explicit.
	pub const fn from_inner(hash: H256) -> Self {
		LaneId(hash)
	}
}

impl core::fmt::Display for LaneId {
	fn fmt(&self, fmt: &mut core::fmt::Formatter) -> core::fmt::Result {
		self.0.fmt(fmt)
	}
}

impl core::fmt::Debug for LaneId {
	fn fmt(&self, fmt: &mut core::fmt::Formatter) -> core::fmt::Result {
		self.0.fmt(fmt)
	}
}

impl AsRef<H256> for LaneId {
	fn as_ref(&self) -> &H256 {
		&self.0
	}
}

impl TypeId for LaneId {
	const TYPE_ID: [u8; 4] = *b"blan";
}

/// Lane state.
#[derive(Clone, Copy, Decode, Encode, Eq, PartialEq, TypeInfo, MaxEncodedLen, RuntimeDebug)]
pub enum LaneState {
	/// Lane is opened and messages may be sent/received over it.
	Opened,
	/// Lane is closing. It is equal to the `Opened` state, but it will switch to
	/// the `Closed` state and then vanish after some period. This state is here
	/// to give bridged chain ability to know that the lane is going to be closed.
	Closing,
	/// Lane is closed and all attempts to send/receive messages to/from this lane
	/// will fail.
	///
	/// Keep in mind that the lane has two ends and the state of the same lane at
	/// its ends may be different. Those who are controlling/serving the lane
	/// and/or sending messages over the lane, have to coordinate their actions on
	/// both ends to make sure that lane is operating smoothly on both ends.
	Closed,
}

impl LaneState {
	/// Returns true if lane state allows sending/receiving messages.
	pub fn is_active(&self) -> bool {
		matches!(*self, LaneState::Opened | LaneState::Closing)
	}
}

/// Message nonce. Valid messages will never have 0 nonce.
pub type MessageNonce = u64;

/// Message id as a tuple.
pub type BridgeMessageId = (LaneId, MessageNonce);

/// Opaque message payload. We only decode this payload when it is dispatched.
pub type MessagePayload = Vec<u8>;

/// Message key (unique message identifier) as it is stored in the storage.
#[derive(Encode, Decode, Clone, PartialEq, Eq, RuntimeDebug, TypeInfo, MaxEncodedLen)]
pub struct MessageKey {
	/// ID of the message lane.
	pub lane_id: LaneId,
	/// Message nonce.
	pub nonce: MessageNonce,
}

/// Message as it is stored in the storage.
#[derive(Encode, Decode, Clone, PartialEq, Eq, RuntimeDebug, TypeInfo)]
pub struct Message {
	/// Message key.
	pub key: MessageKey,
	/// Message payload.
	pub payload: MessagePayload,
}

/// Inbound lane data.
#[derive(Encode, Decode, Clone, RuntimeDebug, PartialEq, Eq, TypeInfo)]
pub struct InboundLaneData<RelayerId> {
	/// Inbound lane state.
	///
	/// If state is `Closed`, then all attempts to deliver messages to this end will fail.
	pub state: LaneState,

	/// Identifiers of relayers and messages that they have delivered to this lane (ordered by
	/// message nonce).
	///
	/// This serves as a helper storage item, to allow the source chain to easily pay rewards
	/// to the relayers who successfully delivered messages to the target chain (inbound lane).
	///
	/// It is guaranteed to have at most N entries, where N is configured at the module level.
	/// If there are N entries in this vec, then:
	/// 1) all incoming messages are rejected if they're missing corresponding
	/// `proof-of(outbound-lane.state)`; 2) all incoming messages are rejected if
	/// `proof-of(outbound-lane.state).last_delivered_nonce` is    equal to
	/// `self.last_confirmed_nonce`. Given what is said above, all nonces in this queue are in
	/// range: `(self.last_confirmed_nonce; self.last_delivered_nonce()]`.
	///
	/// When a relayer sends a single message, both of MessageNonces are the same.
	/// When relayer sends messages in a batch, the first arg is the lowest nonce, second arg the
	/// highest nonce. Multiple dispatches from the same relayer are allowed.
	pub relayers: VecDeque<UnrewardedRelayer<RelayerId>>,

	/// Nonce of the last message that
	/// a) has been delivered to the target (this) chain and
	/// b) the delivery has been confirmed on the source chain
	///
	/// that the target chain knows of.
	///
	/// This value is updated indirectly when an `OutboundLane` state of the source
	/// chain is received alongside with new messages delivery.
	pub last_confirmed_nonce: MessageNonce,
}

impl<RelayerId> Default for InboundLaneData<RelayerId> {
	fn default() -> Self {
		InboundLaneData {
			state: LaneState::Closed,
			relayers: VecDeque::new(),
			last_confirmed_nonce: 0,
		}
	}
}

impl<RelayerId> InboundLaneData<RelayerId> {
	/// Returns default inbound lane data with opened state.
	pub fn opened() -> Self {
		InboundLaneData { state: LaneState::Opened, ..Default::default() }
	}

	/// Returns approximate size of the struct, given a number of entries in the `relayers` set and
	/// size of each entry.
	///
	/// Returns `None` if size overflows `usize` limits.
	pub fn encoded_size_hint(relayers_entries: usize) -> Option<usize>
	where
		RelayerId: MaxEncodedLen,
	{
		relayers_entries
			.checked_mul(UnrewardedRelayer::<RelayerId>::max_encoded_len())?
			.checked_add(MessageNonce::max_encoded_len())
	}

	/// Returns the approximate size of the struct as u32, given a number of entries in the
	/// `relayers` set and the size of each entry.
	///
	/// Returns `u32::MAX` if size overflows `u32` limits.
	pub fn encoded_size_hint_u32(relayers_entries: usize) -> u32
	where
		RelayerId: MaxEncodedLen,
	{
		Self::encoded_size_hint(relayers_entries)
			.and_then(|x| u32::try_from(x).ok())
			.unwrap_or(u32::MAX)
	}

	/// Nonce of the last message that has been delivered to this (target) chain.
	pub fn last_delivered_nonce(&self) -> MessageNonce {
		self.relayers
			.back()
			.map(|entry| entry.messages.end)
			.unwrap_or(self.last_confirmed_nonce)
	}

	/// Returns the total number of messages in the `relayers` vector,
	/// saturating in case of underflow or overflow.
	pub fn total_unrewarded_messages(&self) -> MessageNonce {
		let relayers = &self.relayers;
		match (relayers.front(), relayers.back()) {
			(Some(front), Some(back)) =>
				(front.messages.begin..=back.messages.end).saturating_len(),
			_ => 0,
		}
	}
}

/// Outbound message details, returned by runtime APIs.
#[derive(Clone, Encode, Decode, RuntimeDebug, PartialEq, Eq, TypeInfo)]
pub struct OutboundMessageDetails {
	/// Nonce assigned to the message.
	pub nonce: MessageNonce,
	/// Message dispatch weight.
	///
	/// Depending on messages pallet configuration, it may be declared by the message submitter,
	/// computed automatically or just be zero if dispatch fee is paid at the target chain.
	pub dispatch_weight: Weight,
	/// Size of the encoded message.
	pub size: u32,
}

/// Inbound message details, returned by runtime APIs.
#[derive(Clone, Encode, Decode, RuntimeDebug, PartialEq, Eq, TypeInfo)]
pub struct InboundMessageDetails {
	/// Computed message dispatch weight.
	///
	/// Runtime API guarantees that it will match the value, returned by
	/// `target_chain::MessageDispatch::dispatch_weight`. This means that if the runtime
	/// has failed to decode the message, it will be zero - that's because `undecodable`
	/// message cannot be dispatched.
	pub dispatch_weight: Weight,
}

/// Unrewarded relayer entry stored in the inbound lane data.
///
/// This struct represents a continuous range of messages that have been delivered by the same
/// relayer and whose confirmations are still pending.
#[derive(Encode, Decode, Clone, RuntimeDebug, PartialEq, Eq, TypeInfo, MaxEncodedLen)]
pub struct UnrewardedRelayer<RelayerId> {
	/// Identifier of the relayer.
	pub relayer: RelayerId,
	/// Messages range, delivered by this relayer.
	pub messages: DeliveredMessages,
}

/// Received messages with their dispatch result.
#[derive(Clone, Encode, Decode, RuntimeDebug, PartialEq, Eq, TypeInfo)]
pub struct ReceivedMessages<DispatchLevelResult> {
	/// Id of the lane which is receiving messages.
	pub lane: LaneId,
	/// Result of messages which we tried to dispatch
	pub receive_results: Vec<(MessageNonce, ReceivalResult<DispatchLevelResult>)>,
}

impl<DispatchLevelResult> ReceivedMessages<DispatchLevelResult> {
	pub fn new(
		lane: LaneId,
		receive_results: Vec<(MessageNonce, ReceivalResult<DispatchLevelResult>)>,
	) -> Self {
		ReceivedMessages { lane, receive_results }
	}

	pub fn push(&mut self, message: MessageNonce, result: ReceivalResult<DispatchLevelResult>) {
		self.receive_results.push((message, result));
	}
}

/// Result of single message receival.
#[derive(RuntimeDebug, Encode, Decode, PartialEq, Eq, Clone, TypeInfo)]
pub enum ReceivalResult<DispatchLevelResult> {
	/// Message has been received and dispatched. Note that we don't care whether dispatch has
	/// been successful or not - in both case message falls into this category.
	///
	/// The message dispatch result is also returned.
	Dispatched(MessageDispatchResult<DispatchLevelResult>),
	/// Message has invalid nonce and lane has rejected to accept this message.
	InvalidNonce,
	/// There are too many unrewarded relayer entries at the lane.
	TooManyUnrewardedRelayers,
	/// There are too many unconfirmed messages at the lane.
	TooManyUnconfirmedMessages,
}

/// Delivered messages with their dispatch result.
#[derive(Clone, Default, Encode, Decode, RuntimeDebug, PartialEq, Eq, TypeInfo, MaxEncodedLen)]
pub struct DeliveredMessages {
	/// Nonce of the first message that has been delivered (inclusive).
	pub begin: MessageNonce,
	/// Nonce of the last message that has been delivered (inclusive).
	pub end: MessageNonce,
}

impl DeliveredMessages {
	/// Create new `DeliveredMessages` struct that confirms delivery of single nonce with given
	/// dispatch result.
	pub fn new(nonce: MessageNonce) -> Self {
		DeliveredMessages { begin: nonce, end: nonce }
	}

	/// Return total count of delivered messages.
	pub fn total_messages(&self) -> MessageNonce {
		(self.begin..=self.end).saturating_len()
	}

	/// Note new dispatched message.
	pub fn note_dispatched_message(&mut self) {
		self.end += 1;
	}

	/// Returns true if delivered messages contain message with given nonce.
	pub fn contains_message(&self, nonce: MessageNonce) -> bool {
		(self.begin..=self.end).contains(&nonce)
	}
}

/// Gist of `InboundLaneData::relayers` field used by runtime APIs.
#[derive(Clone, Default, Encode, Decode, RuntimeDebug, PartialEq, Eq, TypeInfo)]
pub struct UnrewardedRelayersState {
	/// Number of entries in the `InboundLaneData::relayers` set.
	pub unrewarded_relayer_entries: MessageNonce,
	/// Number of messages in the oldest entry of `InboundLaneData::relayers`. This is the
	/// minimal number of reward proofs required to push out this entry from the set.
	pub messages_in_oldest_entry: MessageNonce,
	/// Total number of messages in the relayers vector.
	pub total_messages: MessageNonce,
	/// Nonce of the latest message that has been delivered to the target chain.
	///
	/// This corresponds to the result of the `InboundLaneData::last_delivered_nonce` call
	/// at the bridged chain.
	pub last_delivered_nonce: MessageNonce,
}

impl UnrewardedRelayersState {
	// Verify that the relayers state corresponds with the `InboundLaneData`.
	pub fn is_valid<RelayerId>(&self, lane_data: &InboundLaneData<RelayerId>) -> bool {
		self == &lane_data.into()
	}
}

impl<RelayerId> From<&InboundLaneData<RelayerId>> for UnrewardedRelayersState {
	fn from(lane: &InboundLaneData<RelayerId>) -> UnrewardedRelayersState {
		UnrewardedRelayersState {
			unrewarded_relayer_entries: lane.relayers.len() as _,
			messages_in_oldest_entry: lane
				.relayers
				.front()
				.map(|entry| entry.messages.total_messages())
				.unwrap_or(0),
			total_messages: lane.total_unrewarded_messages(),
			last_delivered_nonce: lane.last_delivered_nonce(),
		}
	}
}

/// Outbound lane data.
#[derive(Encode, Decode, Clone, RuntimeDebug, PartialEq, Eq, TypeInfo, MaxEncodedLen)]
pub struct OutboundLaneData {
	/// Lane state.
	///
	/// If state is `Closed`, then all attempts to send messages messages at this end will fail.
	pub state: LaneState,
	/// Nonce of the oldest message that we haven't yet pruned. May point to not-yet-generated
	/// message if all sent messages are already pruned.
	pub oldest_unpruned_nonce: MessageNonce,
	/// Nonce of the latest message, received by bridged chain.
	pub latest_received_nonce: MessageNonce,
	/// Nonce of the latest message, generated by us.
	pub latest_generated_nonce: MessageNonce,
}

impl OutboundLaneData {
	/// Returns default outbound lane data with opened state.
	pub fn opened() -> Self {
		OutboundLaneData { state: LaneState::Opened, ..Default::default() }
	}
}

impl Default for OutboundLaneData {
	fn default() -> Self {
		OutboundLaneData {
			state: LaneState::Closed,
			// it is 1 because we're pruning everything in [oldest_unpruned_nonce;
			// latest_received_nonce]
			oldest_unpruned_nonce: 1,
			latest_received_nonce: 0,
			latest_generated_nonce: 0,
		}
	}
}

/// Calculate the number of messages that the relayers have delivered.
pub fn calc_relayers_rewards<AccountId>(
	messages_relayers: VecDeque<UnrewardedRelayer<AccountId>>,
	received_range: &RangeInclusive<MessageNonce>,
) -> RelayersRewards<AccountId>
where
	AccountId: sp_std::cmp::Ord,
{
	// remember to reward relayers that have delivered messages
	// this loop is bounded by `T::MAX_UNREWARDED_RELAYERS_IN_CONFIRMATION_TX` on the bridged chain
	let mut relayers_rewards = RelayersRewards::new();
	for entry in messages_relayers {
		let nonce_begin = sp_std::cmp::max(entry.messages.begin, *received_range.start());
		let nonce_end = sp_std::cmp::min(entry.messages.end, *received_range.end());
		if nonce_end >= nonce_begin {
			*relayers_rewards.entry(entry.relayer).or_default() += nonce_end - nonce_begin + 1;
		}
	}
	relayers_rewards
}

/// The `BridgeMessagesCall` used by a chain.
pub type BridgeMessagesCallOf<C> = BridgeMessagesCall<
	AccountIdOf<C>,
	target_chain::FromBridgedChainMessagesProof<HashOf<C>>,
	source_chain::FromBridgedChainMessagesDeliveryProof<HashOf<C>>,
>;

/// A minimized version of `pallet-bridge-messages::Call` that can be used without a runtime.
#[derive(Encode, Decode, Debug, PartialEq, Eq, Clone, TypeInfo)]
#[allow(non_camel_case_types)]
pub enum BridgeMessagesCall<AccountId, MessagesProof, MessagesDeliveryProof> {
	/// `pallet-bridge-messages::Call::receive_messages_proof`
	#[codec(index = 2)]
	receive_messages_proof {
		relayer_id_at_bridged_chain: AccountId,
		proof: MessagesProof,
		messages_count: u32,
		dispatch_weight: Weight,
	},
	/// `pallet-bridge-messages::Call::receive_messages_delivery_proof`
	#[codec(index = 3)]
	receive_messages_delivery_proof {
		proof: MessagesDeliveryProof,
		relayers_state: UnrewardedRelayersState,
	},
}

/// Error that happens during message verification.
#[derive(Encode, Decode, RuntimeDebug, PartialEq, Eq, PalletError, TypeInfo)]
pub enum VerificationError {
	/// The message proof is empty.
	EmptyMessageProof,
	/// Error returned by the bridged header chain.
	HeaderChain(HeaderChainError),
	/// Error returned while reading/decoding inbound lane data from the storage proof.
	InboundLaneStorage(StorageProofError),
	/// The declared message weight is incorrect.
	InvalidMessageWeight,
	/// Declared messages count doesn't match actual value.
	MessagesCountMismatch,
	/// Error returned while reading/decoding message data from the `VerifiedStorageProof`.
	MessageStorage(StorageProofError),
	/// The message is too large.
	MessageTooLarge,
	/// Error returned while reading/decoding outbound lane data from the `VerifiedStorageProof`.
	OutboundLaneStorage(StorageProofError),
	/// Storage proof related error.
	StorageProof(StorageProofError),
	/// Custom error
	Other(#[codec(skip)] &'static str),
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn lane_is_closed_by_default() {
		assert_eq!(InboundLaneData::<()>::default().state, LaneState::Closed);
		assert_eq!(OutboundLaneData::default().state, LaneState::Closed);
	}

	#[test]
	fn total_unrewarded_messages_does_not_overflow() {
		let lane_data = InboundLaneData {
			state: LaneState::Opened,
			relayers: vec![
				UnrewardedRelayer { relayer: 1, messages: DeliveredMessages::new(0) },
				UnrewardedRelayer {
					relayer: 2,
					messages: DeliveredMessages::new(MessageNonce::MAX),
				},
			]
			.into_iter()
			.collect(),
			last_confirmed_nonce: 0,
		};
		assert_eq!(lane_data.total_unrewarded_messages(), MessageNonce::MAX);
	}

	#[test]
	fn inbound_lane_data_returns_correct_hint() {
		let test_cases = vec![
			// single relayer, multiple messages
			(1, 128u8),
			// multiple relayers, single message per relayer
			(128u8, 128u8),
			// several messages per relayer
			(13u8, 128u8),
		];
		for (relayer_entries, messages_count) in test_cases {
			let expected_size = InboundLaneData::<u8>::encoded_size_hint(relayer_entries as _);
			let actual_size = InboundLaneData {
				state: LaneState::Opened,
				relayers: (1u8..=relayer_entries)
					.map(|i| UnrewardedRelayer {
						relayer: i,
						messages: DeliveredMessages::new(i as _),
					})
					.collect(),
				last_confirmed_nonce: messages_count as _,
			}
			.encode()
			.len();
			let difference = (expected_size.unwrap() as f64 - actual_size as f64).abs();
			assert!(
				difference / (std::cmp::min(actual_size, expected_size.unwrap()) as f64) < 0.1,
				"Too large difference between actual ({actual_size}) and expected ({expected_size:?}) inbound lane data size. Test case: {relayer_entries}+{messages_count}",
			);
		}
	}

	#[test]
	fn contains_result_works() {
		let delivered_messages = DeliveredMessages { begin: 100, end: 150 };

		assert!(!delivered_messages.contains_message(99));
		assert!(delivered_messages.contains_message(100));
		assert!(delivered_messages.contains_message(150));
		assert!(!delivered_messages.contains_message(151));
	}

	#[test]
	fn lane_id_debug_format_matches_inner_hash_format() {
		assert_eq!(
			format!("{:?}", LaneId(H256::from([1u8; 32]))),
			format!("{:?}", H256::from([1u8; 32])),
		);
	}

	#[test]
	fn lane_id_is_generated_using_ordered_endpoints() {
		assert_eq!(LaneId::new(1, 2), LaneId::new(2, 1));
	}

	#[test]
	fn lane_id_is_different_for_different_endpoints() {
		assert_ne!(LaneId::new(1, 2), LaneId::new(1, 3));
	}

	#[test]
	fn lane_id_is_different_even_if_arguments_has_partial_matching_encoding() {
		/// Some artificial type that generates the same encoding for different values
		/// concatenations. I.e. the encoding for `(Either::Two(1, 2), Either::Two(3, 4))`
		/// is the same as encoding of `(Either::Three(1, 2, 3), Either::One(4))`.
		/// In practice, this type is not useful, because you can't do a proper decoding.
		/// But still there may be some collisions even in proper types.
		#[derive(Eq, Ord, PartialEq, PartialOrd)]
		enum Either {
			Three(u64, u64, u64),
			Two(u64, u64),
			One(u64),
		}

		impl codec::Encode for Either {
			fn encode(&self) -> Vec<u8> {
				match *self {
					Self::One(a) => a.encode(),
					Self::Two(a, b) => (a, b).encode(),
					Self::Three(a, b, c) => (a, b, c).encode(),
				}
			}
		}

		assert_ne!(
			LaneId::new(Either::Two(1, 2), Either::Two(3, 4)),
			LaneId::new(Either::Three(1, 2, 3), Either::One(4)),
		);
	}
}
