use bdk::bitcoin::{Address, BlockHeader, Script, Transaction, Txid};
use bdk::blockchain::{Blockchain, GetHeight, WalletSync};
use bdk::database::BatchDatabase;
use bdk::wallet::{AddressIndex, Wallet};
use bdk::{Balance, SignOptions, SyncOptions};

pub use indexed_chain::{IndexedChain, TxStatus};
use lightning::chain::chaininterface::BroadcasterInterface;
use lightning::chain::chaininterface::{ConfirmationTarget, FeeEstimator};
use lightning::chain::WatchedOutput;
use lightning::chain::{Confirm, Filter};
use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

pub type TransactionWithHeight = (u32, Transaction);
pub type TransactionWithPosition = (usize, Transaction);
pub type TransactionWithHeightAndPosition = (u32, Transaction, usize);

mod indexed_chain;

#[derive(Debug)]
pub enum Error {
    Bdk(bdk::Error),
}

impl From<bdk::Error> for Error {
    fn from(e: bdk::Error) -> Self {
        Self::Bdk(e)
    }
}

struct TxFilter {
    watched_transactions: Vec<(Txid, Script)>,
    watched_outputs: Vec<WatchedOutput>,
}

impl TxFilter {
    fn new() -> Self {
        Self {
            watched_transactions: vec![],
            watched_outputs: vec![],
        }
    }

    fn register_tx(&mut self, txid: Txid, script: Script) {
        self.watched_transactions.push((txid, script));
    }

    fn register_output(&mut self, output: WatchedOutput) {
        self.watched_outputs.push(output);
    }
}

impl Default for TxFilter {
    fn default() -> Self {
        Self::new()
    }
}

/// Lightning Wallet
///
/// A wrapper around a bdk::Wallet to fulfill many of the requirements
/// needed to use lightning with LDK.  Note: The bdk::Blockchain you use
/// must implement the IndexedChain trait.
pub struct LightningWallet<B, D> {
    client: Mutex<Box<B>>,
    wallet: Mutex<Wallet<D>>,
    filter: Mutex<TxFilter>,
}

impl<B, D> LightningWallet<B, D>
where
    B: Blockchain + GetHeight + WalletSync + IndexedChain,
    D: BatchDatabase,
{
    /// create a new lightning wallet from your bdk wallet
    pub fn new(client: Box<B>, wallet: Wallet<D>) -> Self {
        LightningWallet {
            client: Mutex::new(client),
            wallet: Mutex::new(wallet),
            filter: Mutex::new(TxFilter::new()),
        }
    }

    /// syncs both your onchain and lightning wallet to current tip
    /// utilizes ldk's Confirm trait to provide chain data
    pub fn sync(&self, confirmables: Vec<&dyn Confirm>) -> Result<(), Error> {
        self.sync_onchain_wallet()?;

        let mut relevant_txids = confirmables
            .iter()
            .flat_map(|confirmable| confirmable.get_relevant_txids())
            .collect::<Vec<Txid>>();

        relevant_txids.sort_unstable();
        relevant_txids.dedup();

        let unconfirmed_txids = self.get_unconfirmed(relevant_txids)?;
        for unconfirmed_txid in unconfirmed_txids {
            for confirmable in confirmables.iter() {
                confirmable.transaction_unconfirmed(&unconfirmed_txid);
            }
        }

        let confirmed_txs = self.get_confirmed_txs_by_block()?;
        for (height, header, tx_list) in confirmed_txs {
            let tx_list_ref = tx_list
                .iter()
                .map(|(height, tx)| (height.to_owned(), tx))
                .collect::<Vec<(usize, &Transaction)>>();

            for confirmable in confirmables.iter() {
                confirmable.transactions_confirmed(&header, tx_list_ref.as_slice(), height);
            }
        }

        let (tip_height, tip_header) = self.get_tip()?;

        for confirmable in confirmables.iter() {
            confirmable.best_block_updated(&tip_header, tip_height);
        }

        Ok(())
    }

    /// returns the AddressIndex::LastUnused address for your wallet
    /// this is useful when you need to sweep funds from a channel
    /// back into your onchain wallet.
    pub fn get_unused_address(&self) -> Result<Address, Error> {
        let wallet = self.wallet.lock().unwrap();
        let address_info = wallet.get_address(AddressIndex::LastUnused)?;
        Ok(address_info.address)
    }

    /// when opening a channel you can use this to fund the channel
    /// with the utxos in your bdk wallet
    pub fn construct_funding_transaction(
        &self,
        output_script: &Script,
        value: u64,
        target_blocks: usize,
    ) -> Result<Transaction, Error> {
        let client = self.client.lock().unwrap();
        let wallet = self.wallet.lock().unwrap();
        let mut tx_builder = wallet.build_tx();
        let fee_rate = client.estimate_fee(target_blocks)?;

        tx_builder
            .add_recipient(output_script.clone(), value)
            .fee_rate(fee_rate)
            .enable_rbf();

        let (mut psbt, _tx_details) = tx_builder.finish()?;

        let _finalized = wallet.sign(&mut psbt, SignOptions::default())?;

        Ok(psbt.extract_tx())
    }

    /// get the balance of the inner onchain bdk wallet
    pub fn get_balance(&self) -> Result<Balance, Error> {
        let wallet = self.wallet.lock().unwrap();
        wallet.get_balance().map_err(Error::Bdk)
    }

    /// get a reference to the inner bdk wallet
    /// be careful using this because it will hold the lock
    /// on the inner wallet until the guard is dropped
    /// this is useful if you need methods on the wallet that
    /// are not yet exposed on LightningWallet
    pub fn get_wallet(&self) -> MutexGuard<Wallet<D>> {
        self.wallet.lock().unwrap()
    }

    fn sync_onchain_wallet(&self) -> Result<(), Error> {
        let wallet = self.wallet.lock().unwrap();
        let client = self.client.lock().unwrap();
        wallet.sync(client.as_ref(), SyncOptions::default())?;
        Ok(())
    }

    fn get_unconfirmed(&self, txids: Vec<Txid>) -> Result<Vec<Txid>, Error> {
        Ok(txids
            .into_iter()
            .map(|txid| self.augment_txid_with_confirmation_status(txid))
            .collect::<Result<Vec<(Txid, bool)>, Error>>()?
            .into_iter()
            .filter(|(_txid, confirmed)| !confirmed)
            .map(|(txid, _)| txid)
            .collect())
    }

    fn get_confirmed_txs_by_block(
        &self,
    ) -> Result<Vec<(u32, BlockHeader, Vec<TransactionWithPosition>)>, Error> {
        let mut txs_by_block: HashMap<u32, Vec<TransactionWithPosition>> = HashMap::new();

        let filter = self.filter.lock().unwrap();

        let mut confirmed_txs = filter
            .watched_transactions
            .iter()
            .map(|(txid, script)| self.get_confirmed_tx(txid, script))
            .collect::<Result<Vec<Option<TransactionWithHeight>>, Error>>()?
            .into_iter()
            .flatten()
            .collect::<Vec<TransactionWithHeight>>();

        let mut confirmed_spent = filter
            .watched_outputs
            .iter()
            .map(|output| self.get_confirmed_txs(output))
            .collect::<Result<Vec<Vec<TransactionWithHeight>>, Error>>()?
            .into_iter()
            .flatten()
            .collect::<Vec<TransactionWithHeight>>();

        confirmed_txs.append(&mut confirmed_spent);

        let confirmed_txs_with_position = confirmed_txs
            .into_iter()
            .map(|(height, tx)| self.augment_with_position(height, tx))
            .collect::<Result<Vec<Option<TransactionWithHeightAndPosition>>, Error>>()?
            .into_iter()
            .flatten()
            .collect::<Vec<TransactionWithHeightAndPosition>>();

        for (height, tx, pos) in confirmed_txs_with_position {
            txs_by_block.entry(height).or_default().push((pos, tx))
        }

        txs_by_block
            .into_iter()
            .map(|(height, tx_list)| self.augment_with_header(height, tx_list))
            .collect()
    }

    /// get a tuple containing the current tip height and header
    pub fn get_tip(&self) -> Result<(u32, BlockHeader), Error> {
        let client = self.client.lock().unwrap();
        let tip_height = client.get_height()?;
        let tip_header = client.get_header(tip_height)?;
        Ok((tip_height, tip_header))
    }

    fn augment_txid_with_confirmation_status(&self, txid: Txid) -> Result<(Txid, bool), Error> {
        let client = self.client.lock().unwrap();
        client
            .get_tx_status(&txid)
            .map(|status| match status {
                Some(status) => (txid, status.confirmed),
                None => (txid, false),
            })
            .map_err(Error::Bdk)
    }

    fn get_confirmed_tx(
        &self,
        txid: &Txid,
        script: &Script,
    ) -> Result<Option<TransactionWithHeight>, Error> {
        let client = self.client.lock().unwrap();
        client
            .get_script_tx_history(script)
            .map(|history| {
                history
                    .into_iter()
                    .find(|(status, tx)| status.confirmed && tx.txid().eq(txid))
                    .map(|(status, tx)| (status.block_height.unwrap(), tx))
            })
            .map_err(Error::Bdk)
    }

    fn get_confirmed_txs_from_script_history(
        &self,
        history: Vec<(TxStatus, Transaction)>,
    ) -> Vec<TransactionWithHeight> {
        history
            .into_iter()
            .filter(|(status, _tx)| status.confirmed)
            .map(|(status, tx)| (status.block_height.unwrap(), tx))
            .collect::<Vec<TransactionWithHeight>>()
    }

    fn get_confirmed_txs(
        &self,
        output: &WatchedOutput,
    ) -> Result<Vec<TransactionWithHeight>, Error> {
        let client = self.client.lock().unwrap();

        client
            .get_script_tx_history(&output.script_pubkey)
            .map(|history| self.get_confirmed_txs_from_script_history(history))
            .map_err(Error::Bdk)
    }

    fn augment_with_position(
        &self,
        height: u32,
        tx: Transaction,
    ) -> Result<Option<TransactionWithHeightAndPosition>, Error> {
        let client = self.client.lock().unwrap();

        client
            .get_position_in_block(&tx.txid(), height as usize)
            .map(|position| position.map(|pos| (height, tx, pos)))
            .map_err(Error::Bdk)
    }

    fn augment_with_header(
        &self,
        height: u32,
        tx_list: Vec<TransactionWithPosition>,
    ) -> Result<(u32, BlockHeader, Vec<TransactionWithPosition>), Error> {
        let client = self.client.lock().unwrap();
        client
            .get_header(height)
            .map(|header| (height, header, tx_list))
            .map_err(Error::Bdk)
    }
}

impl<B, D> FeeEstimator for LightningWallet<B, D>
where
    B: Blockchain,
    D: BatchDatabase,
{
    fn get_est_sat_per_1000_weight(&self, confirmation_target: ConfirmationTarget) -> u32 {
        let client = self.client.lock().unwrap();

        let target_blocks = match confirmation_target {
            ConfirmationTarget::Background => 6,
            ConfirmationTarget::Normal => 3,
            ConfirmationTarget::HighPriority => 1,
        };

        let estimate = client.estimate_fee(target_blocks).unwrap_or_default();
        let sats_per_vbyte = estimate.as_sat_per_vb() as u32;
        sats_per_vbyte * 253
    }
}

impl<B, D> BroadcasterInterface for LightningWallet<B, D>
where
    B: Blockchain,
    D: BatchDatabase,
{
    fn broadcast_transaction(&self, tx: &Transaction) {
        let client = self.client.lock().unwrap();
        let _result = client.broadcast(tx);
    }
}

impl<B, D> Filter for LightningWallet<B, D>
where
    B: Blockchain,
    D: BatchDatabase,
{
    fn register_tx(&self, txid: &Txid, script_pubkey: &Script) {
        let mut filter = self.filter.lock().unwrap();
        filter.register_tx(*txid, script_pubkey.clone());
    }

    fn register_output(&self, output: WatchedOutput) {
        let mut filter = self.filter.lock().unwrap();
        filter.register_output(output);
        // TODO: do we need to check for tx here or wait for next sync?
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {
        let result = 2 + 2;
        assert_eq!(result, 4);
    }
}
