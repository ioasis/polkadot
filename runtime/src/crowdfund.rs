// Copyright 2017-2019 Parity Technologies (UK) Ltd.
// This file is part of Substrate.

// Substrate is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Substrate is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Substrate.  If not, see <http://www.gnu.org/licenses/>.

//! # Parachain Crowdfunding module
//!
//! The point of this is to allow parachain projects to offer the ability to help fund a deposit for
//! the parachain. When the parachain is retired, the funds may be returned.
//!
//! Contributing funds is permissionless. Each fund has a child-trie which stores all
//! contributors account IDs together with the amount they contributed; the root of this can then be
//! used by the parachain to allow contributors to prove that they made some particular contribution
//! to the project (e.g. to be rewarded through some token or badge). The trie is retained for later
//! (efficient) redistribution back to the contributors.
//!
//! Contributions must be of at least `MinContribution` (to account for the resouces taken in
//! tracking contributions), and may never tally greater than the fund's `cap`, set and fixed at the
//! time of creation. The `create` call may be used to create a new fund. In order to do this, then
//! a deposit must be paid of the amount `SubmissionDeposit`. Substantial resources are taken on
//! the main trie in tracking a fund and this accounts for that.
//!
//! Funds may be set up at any time; their closing time is fixed at creation (as a block number) and
//! if the fund is not successful by the closing time, then it will become *retired*. Contributors
//! may get a refund of their contributions from retired funds. After a period (`RetirementPeriod`)
//! the fund may be dissolved entirely. At this point any unrefunded contributions are considered
//! `orphaned` and are disposed of through the `OrphanedFunds` handler (which may e.g. place them
//! into the treasury).
//!
//! Funds may accept contributions at any point before their success or retirement. When a parachain
//! slot auction enters its ending period, then parachains will each place a bid; the bid will be
//! raised once per block if the parachain had additional funds contributed since the last bid.
//!
//! Funds may set their deploy data (the code hash and head data of their parachain) at any point.
//! It may only be done once and once set cannot be changed. Good procedure would be to set them
//! ahead of receiving any contributions in order that contributors may verify that their parachain
//! contains all expected functionality. However, this is not enforced and deploy data may happen
//! at any point, even after a slot has been successfully won or, indeed, never.
//!
//! Funds that are successful winners of a slot may have their slot claimed through the `onboard`
//! call. This may only be done once and must be after the deploy data has been fixed. Successful
//! funds remain tracked (in the `Funds` storage item and the associated child trie) as long as
//! the parachain remains active. Once it does not, it is up to the parachain to ensure that the
//! funds are returned to this module's fund sub-account in order that they be redistributed back to
//! contributors. *Retirement* may be initiated by any account (using the `begin_retirement` call)
//! once the parachain is removed from the its slot.
//!
//! @WARNING: For funds to be returned, it is imperative that this module's account is provided as
//! the offboarding account for the slot. In the case that a parachain supplemented these funds in
//! order to win a later auction, then it is the parachain's duty to ensure that the right amount of
//! funds ultimately end up in module's fund sub-account. If the funds do not arrive, then

use srml_support::{
	StorageValue, dispatch::Result, decl_module, decl_storage, decl_event, storage::child,
	traits::{LockableCurrency, ReservableCurrency, Currency}
};
use system::ensure_signed;
use sr_primitives::weights::TransactionWeight;
use primitives::parachain::Id as ParaId;
use crate::slots;

const MODULE_ID: ModuleId = ModuleId(*b"py/cfund");

type BalanceOf<T> = <<T as Trait>::Currency as Currency<<T as system::Trait>::AccountId>>::Balance;
type NegativeImbalanceOf<T> = <<T as Trait>::Currency as Currency<<T as system::Trait>::AccountId>>::NegativeImbalance;

pub trait Trait: slots::Trait {
	type Currency: LockableCurrency + ReservableCurrency;

	/// The overarching event type.
	type Event: From<Event<Self>> + Into<<Self as system::Trait>::Event>;

	/// The amount to be held on deposit by the owner of a crowdfund.
	type SubmissionDeposit: Get<BalanceOf<Self>>;

	/// The minimum amount that may be contributed into a crowdfund. Should almost certainly be at
	/// least ExistentialDeposit.
	type MinContribution: Get<BalanceOf<Self>>;

	/// The period of time (in blocks) between after an unsuccessful crowdfund ending where
	/// contributors are able to withdraw their funds. After this period, their funds are lost.
	type RetirementPeriod: Get<Self::BlockNumber>;

	/// What to do with funds that were not withdrawn.
	type OrphanedFunds: OnUnbalanced<NegativeImbalanceOf<T>>;
}

pub type FundIndex = u32;

pub struct FundInfo<AccountId, Balance, Hash, BlockNumber> {
	/// The parachain that this fund has funded, if there is one. As long as this is `Some`, then
	/// the funds may not be withdrawn and the fund cannot be disolved.
	parachain: Option<ParaId>,
	/// The owning account who placed the deposit.
	owner: AccountId,
	/// The amount of deposit placed.
	deposit: Balance,
	/// The total amount raised.
	raised: Balance,
	/// Block number after which the funding must have succeeded. If not successful at this number
	/// the everyone may withdraw their funds.
	end: BlockNumber,
	/// A hard-cap on the amount that may be contributed.
	cap: Balance,
	/// The most recent block that this had a contribution. Determines if we make a bid or not.
	/// If this is `None`, then the last contribution was made outside of the ending period.
	last_contribution: Option<BlockNumber>,
	/// First slot in range to bid on; it's actually a LeasePeriod, but that's the same type as
	/// BlockNumber.
	first_slot: BlockNumber,
	/// Last slot in range to bid on; it's actually a LeasePeriod, but that's the same type as
	/// BlockNumber.
	last_slot: BlockNumber,
	/// The deployment data associated with this fund, if any. Once set it may not be reset. First
	/// is the code hash, second is the initial head data.
	deploy_data: Option<(Hash, Vec<u8>)>,
}

decl_storage! {
	trait Store for Module<T: Trait> as Example {
		/// Info on all of the funds.
		Funds pub(funds):
			map FundIndex => Option<FundInfo<T::AccountId, BalanceOf<T>, T::Hash, T::BlockNumber>>;

		/// The total number of ffunds that have so far been allocated.
		FundCount pub(fund_count): FundIndex;

		/// The funds that have had additional contributions during the last block. This is used
		/// in order to determine which funds should submit updated bids.
		NewRaise: Vec<FundId>;

		/// True if the fund was ending at the last block.
		WasEnding: bool;
	}
}

decl_event!(
	pub enum Event {
		Nothing,
	}
);

decl_module! {
	pub struct Module<T: Trait> for enum Call where origin: T::Origin {
		fn deposit_event<T>() = default;

		/// Create a new crowdfunding campaign for a parachain slot deposit.
		#[weight = TransactionWeight::Basic(100_000, 10)]
		fn create(origin,
			#[compact] end: T::BlockNumber,
			#[compact] cap: BalanceOf<T>,
			#[compact] first_slot: T::BlockNumber,
			#[compact] last_slot: T::BlockNumber,
		) {
			let owner = ensure_signed(origin)?;
			let deposit = T::SubmissionDeposit::get();

			let imb = T::Currency::withdraw(
				&owner,
				deposit,
				WithdrawReason::Transfer,
				ExistenceRequirement::AllowDeath
			)?;

			let index = <FundCount<T>>::mutate(|c| { let r = *c; *c += 1; r });

			// No fees are paid here if we need to create this account; that's why we don't just
			// use the stock `transfer`.
			T::Currency::resolve_creating(Self::fund_account_id(index), imb);

			<Funds<T>>::insert(index, FundInfo {
				raised: Zero::zero(),
				// Ensure it's Some, so that the first contribution causes it to be inserted into
				// `NewRaise`.
				last_contribution: Some(Zero::zero()),
				deploy_data: None,
				..
			});
		}

		/// Contribute to a crowd sale. This will transfer some balance over to fund a parachain
		/// slot. It will be withdrawable in two instances: the parachain becomes retired; or the
		/// slot is
		fn contribute(origin, #[compact] index: FundIndex, #[compact] value: T::Balance) {
			let who = ensure_signed(origin)?;

			ensure!(value >= T::MinContribution::get(), "contribution too small");

			let mut fund = Self::funds(index).ok_or("invalid fund index")?;
			fund.raised += value;
			ensure!(fund.raised <= fund.cap, "contributions exceed cap");
			let now = <system::Module<T>>::block_number();
			ensure!(fund.end > now, "contribution period ended");

			T::Currency::transfer(&who, &Self::fund_account_id(index), value)?;

			let id = Self::id_from_index(index);
			let balance = child::get_or_default::<Balance>(&id, &who);
			let balance = balance.saturating_add(&value);
			child::put(&id, &owner, balance);

			if <slots::Module<T>>::is_ending() {
				// Now in end period; record it
				fund.last_contribution = Some(now);
				if let Some(c) = fund.last_contribution {
					if c != now {
						// last contribution was at earlier time; re-insert into `NewRaise`
						NewRaise::mutate(|v| v.push(index));
					}
				}
			} else {
				if fund.last_contribution.is_some() {
					// Now outside end period and last was also inside end period: reset last to
					// None and insert (because it will have been removed at end of last block).
					fund.last_contribution = None;
					NewRaise::mutate(|v| v.push(index));
				}
			}

			<Funds<T>>::insert(index, &fund);
		}

		/// Withdraw full balance of a contributer to an unsuccessful fund.
		fn withdraw(origin, #[compact] index: FundIndex) {
			let who = ensure_signed(origin)?;

			let mut fund = Self::funds(index).ok_or("invalid fund index")?;
			let now = <system::Module<T>>::block_number();
			ensure!(now >= fund.end, "contribution period not over");

			let id = Self::id_from_index(index);
			let balance = child::get::<Balance>(&id, &who)
				.ok_or("not a contributor")?;

			// Avoid using transfer to ensure we don't pay any fees.
			T::Currency::resolve_into_existing(&who, T::Currency::withdraw(
				&Self::fund_account_id(index),
				balance,
				WithdrawReason::Transfer,
				ExistenceRequirement::AllowDeath
			)?);

			let id = Self::id_from_index(index);
			child::kill(&id, &who);
			fund.raised = fund.raised.saturating_sub(&balance);

			<Funds<T>>::insert(index, &fund);
		}

		/// Note that a successful fund has lost its parachain slot, and place it into retirement.
		fn begin_retirement(_, #[compact] index: FundIndex) {
			// origin unimportant.
			let mut fund = Self::funds(index).ok_or("invalid fund index")?;
			let parachain_id = fund.parachain.take().ok_or("fund has no parachain")?;
			let account = Self::fund_account_id(fund.index);
			ensure!(T::Currency::free_balance(&account) >= fund.raised, "funds not yet returned");

			// This fund just ended. Withdrawal period begins.
			let now = <system::Module<T>>::block_number();
			fund.end = now;

			<Funds<T>>::insert(index, &fund);
		}

		/// Remove a fund after either: it was unsuccessful and it timed out; or it was successful
		/// but it has been retired from its parachain slot. This places any unwithdrawn deposits
		/// into the treasury.
		fn dissolve(_, #[compact] index: FundIndex) {
			// origin unimportant.

			let fund = Self::funds(index).ok_or("invalid fund index")?;
			ensure!(fund.parachain.is_none(), "cannot disolve fund with active parachain");
			let now = <system::Module<T>>::block_number();
			ensure!(now >= fund.end + T::RetirementPeriod::get(), "retirement period not over");

			let account = Self::fund_account_id(index);

			// Avoid using transfer to ensure we don't pay any fees.
			T::Currency::resolve_into_existing(&fund.owner, T::Currency::withdraw(
				&account,
				fund.deposit,
				WithdrawReason::Transfer,
				ExistenceRequirement::AllowDeath
			)?);

			T::OrphanedFunds::on_unbalanced(T::Currency::withdraw(
				&account,
				fund.raised,
				WithdrawReason::Transfer,
				ExistenceRequirement::AllowDeath
			)?);

			let id = Self::id_from_index(index);
			child::kill_storage(&id);
			<Funds<T>>::kill(index);
		}

		/// Set the deploy information for a successful bid to deploy a new parachain.
		///
		/// - `origin` must be the successful bidder account.
		/// - `sub` is the sub-bidder ID of the bidder.
		/// - `para_id` is the parachain ID allotted to the winning bidder.
		/// - `code_hash` is the hash of the parachain's Wasm validation function.
		/// - `initial_head_data` is the parachain's initial head data.
		fn fix_deploy_data(origin,
			#[compact] sub: SubId,
			#[compact] para_id: ParaIdOf<T>,
			code_hash: T::Hash,
			initial_head_data: Vec<u8>
		) {
			let who = ensure_signed(origin)?;
			let (starts, details) = <Onboarding<T>>::get(&para_id)
				.ok_or("parachain id not in onboarding")?;
			if let IncomingParachain::Unset(ref nb) = details {
				ensure!(nb.who == who && nb.sub == sub, "parachain not registered by origin");
			} else {
				return Err("already registered")
			}
			let item = (starts, IncomingParachain::Fixed{code_hash, initial_head_data});
			<Onboarding<T>>::insert(&para_id, item);
		}

		/// Set the deploy data of the funded parachain if not already set. Once set, this cannot
		/// be changed again.
		///
		/// - `origin` must be the fund owner.
		/// - `index` is the fund index that `origin` owns and whose deploy data will be set.
		/// - `code_hash` is the hash of the parachain's Wasm validation function.
		/// - `initial_head_data` is the parachain's initial head data.
		fn fix_deploy_data(origin,
			#[compact] index: FundIndex,
			code_hash: T::Hash,
			initial_head_data: Vec<u8>
		) {
			let who = ensure_signed(origin)?;

			let mut fund = Self::funds(index).ok_or("invalid fund index")?;
			ensure!(fund.owner == who, "origin must be fund owner");
			ensure!(fund.deploy_data.is_none(), "deploy data already set");

			fund.deploy_data = Some((code_hash, initial_head_data));

			<Funds<T>>::insert(index, &fund);
		}

		/// Complete onboarding process for a winning parachain fund. This can be called once by
		/// any origin once a fund wins a slot and the fund has set its deploy data (using
		/// `fix_deploy_data`).
		///
		/// - `index` is the fund index that `origin` owns and whose deploy data will be set.
		/// - `para_id` is the parachain index that this fund won.
		fn onboard(_,
			#[compact] index: FundIndex,
			#[compact] para_id: ParaIdOf<T>
		) {
			let mut fund = Self::funds(index).ok_or("invalid fund index")?;
			let (code_hash, initial_head_data) = fund.deploy_data.ok_or("deploy data not fixed")?;
			ensure!(fund.parachain.is_none(), "fund already onboarded");
			fund.parachain = Some(para_id);

			let fund_origin = system::RawOrigin::Signed(Self::fund_account_id(index)).into();
			slots::fix_deploy_data(fund_origin, index, para_id, code_hash, initial_head_data)?;

			<Funds<T>>::insert(index, &fund);
		}

		fn on_finalize(n: T::BlockNumber) {
			if <slots::Module<T>>::is_ending() {
				for fund in NewRaise::take().into_iter().filter_map(Self::funds) {
					if fund.last_contribution == n {
						let bidder = slots::Bidder::New(slots::NewBidder {
							who: Self::fund_account_id(fund.index),
							/// FundIndex and slots::SubId happen to be the same type (u32). If this
							/// ever changes, then some sort of coversion will be needed here.
							sub: 0,
						});
						<slots::Module<T>>::handle_bid(
							bidder,
							auction_index: <slots::Module<T>>::auction_counter(),
							first_slot: fund.first_slot,
							last_slot: fund.last_slot,
							amount: fund.raised,
						)
					}
				}
			}
		}
	}
}

impl<T: Trait> Module<T> {
	/// The account ID of the fund pot.
	///
	/// This actually does computation. If you need to keep using it, then make sure you cache the
	/// value and only call this once.
	pub fn fund_account_id(index: FundIndex) -> T::AccountId {
		MODULE_ID.into_sub_account(index)
	}

	pub fn id_from_index(index: FundIndex) -> T::Hash {
		T::Hasher::hash_of((b"crowdfund", index))
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	use srml_support::{impl_outer_origin, assert_ok};
	use sr_io::with_externalities;
	use substrate_primitives::{H256, Blake2Hasher};
	// The testing primitives are very useful for avoiding having to work with signatures
	// or public keys. `u64` is used as the `AccountId` and no `Signature`s are requried.
	use sr_primitives::{
		BuildStorage, traits::{BlakeTwo256, OnInitialize, OnFinalize, IdentityLookup},
		testing::Header
	};

	impl_outer_origin! {
		pub enum Origin for Test {}
	}

	// For testing the module, we construct most of a mock runtime. This means
	// first constructing a configuration type (`Test`) which `impl`s each of the
	// configuration traits of modules we want to use.
	#[derive(Clone, Eq, PartialEq)]
	pub struct Test;
	impl system::Trait for Test {
		type Origin = Origin;
		type Index = u64;
		type BlockNumber = u64;
		type Hash = H256;
		type Hashing = BlakeTwo256;
		type AccountId = u64;
		type Lookup = IdentityLookup<Self::AccountId>;
		type Header = Header;
		type Event = ();
	}
	impl balances::Trait for Test {
		type Balance = u64;
		type OnFreeBalanceZero = ();
		type OnNewAccount = ();
		type Event = ();
		type TransactionPayment = ();
		type TransferPayment = ();
		type DustRemoval = ();
	}
	impl Trait for Test {
		type Event = ();
	}
	type Example = Module<Test>;

	// This function basically just builds a genesis storage key/value store according to
	// our desired mockup.
	fn new_test_ext() -> sr_io::TestExternalities<Blake2Hasher> {
		let mut t = system::GenesisConfig::<Test>::default().build_storage().unwrap().0;
		// We use default for brevity, but you can configure as desired if needed.
		t.extend(balances::GenesisConfig::<Test>::default().build_storage().unwrap().0);
		t.extend(GenesisConfig::<Test>{
			dummy: 42,
			// we configure the map with (key, value) pairs.
			bar: vec![(1, 2), (2, 3)],
			foo: 24,
		}.build_storage().unwrap().0);
		t.into()
	}

	#[test]
	fn it_works_for_optional_value() {
		with_externalities(&mut new_test_ext(), || {
			// Check that GenesisBuilder works properly.
			assert_eq!(Example::dummy(), Some(42));

			// Check that accumulate works when we have Some value in Dummy already.
			assert_ok!(Example::accumulate_dummy(Origin::signed(1), 27));
			assert_eq!(Example::dummy(), Some(69));

			// Check that finalizing the block removes Dummy from storage.
			<Example as OnFinalize<u64>>::on_finalize(1);
			assert_eq!(Example::dummy(), None);

			// Check that accumulate works when we Dummy has None in it.
			<Example as OnInitialize<u64>>::on_initialize(2);
			assert_ok!(Example::accumulate_dummy(Origin::signed(1), 42));
			assert_eq!(Example::dummy(), Some(42));
		});
	}

	#[test]
	fn it_works_for_default_value() {
		with_externalities(&mut new_test_ext(), || {
			assert_eq!(Example::foo(), 24);
			assert_ok!(Example::accumulate_foo(Origin::signed(1), 1));
			assert_eq!(Example::foo(), 25);
		});
	}
}
