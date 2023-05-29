use crate::{
    traits::{BlockSource, ReceiptProvider},
    BlockHashProvider, BlockNumProvider, BlockProvider, EvmEnvProvider, HeaderProvider,
    ProviderError, TransactionsProvider, WithdrawalsProvider,
};
use reth_db::{
    cursor::DbCursorRO,
    database::DatabaseGAT,
    tables,
    transaction::{DbTx, DbTxMut},
};
use reth_interfaces::Result;
use reth_primitives::{
    Block, BlockHash, BlockHashOrNumber, BlockNumber, ChainInfo, ChainSpec, Head, Header, Receipt,
    SealedBlock, SealedHeader, TransactionMeta, TransactionSigned, TxHash, TxNumber, Withdrawal,
    H256, U256,
};
use reth_revm_primitives::{
    config::revm_spec,
    env::{fill_block_env, fill_cfg_and_block_env, fill_cfg_env},
    primitives::{BlockEnv, CfgEnv, SpecId},
};
use std::{ops::RangeBounds, sync::Arc};

pub(crate) type ProviderRO<'this, DB> = Provider<'this, <DB as DatabaseGAT<'this>>::TX>;

pub(crate) type ProviderRW<'this, DB> = Provider<'this, <DB as DatabaseGAT<'this>>::TXMut>;

/// Provider struct which allows to interface with the database using different types of providers.
/// Wrapper around [`DbTx`] and [`DbTxMut`]. Example: [`HeaderProvider`] [`BlockHashProvider`]
pub struct Provider<'this, TX>
where
    Self: 'this,
{
    tx: TX,
    /// Chain spec
    chain_spec: Arc<ChainSpec>,
    _phantom_data: std::marker::PhantomData<&'this ()>,
}

impl<'this, TX: DbTxMut<'this>> Provider<'this, TX> {
    pub fn new_rw(tx: TX, chain_spec: Arc<ChainSpec>) -> Self {
        Self { tx, chain_spec, _phantom_data: std::marker::PhantomData }
    }
}

impl<'this, TX: DbTx<'this>> Provider<'this, TX> {
    pub fn new(tx: TX, chain_spec: Arc<ChainSpec>) -> Self {
        Self { tx, chain_spec, _phantom_data: std::marker::PhantomData }
    }
}

impl<'this, TX: DbTxMut<'this> + DbTx<'this>> Provider<'this, TX> {
    /// commit tx
    pub fn commit(self) -> Result<bool> {
        Ok(self.tx.commit()?)
    }
}

impl<'this, TX: DbTx<'this>> HeaderProvider for Provider<'this, TX> {
    fn header(&self, block_hash: &BlockHash) -> Result<Option<Header>> {
        if let Some(num) = self.tx.get::<tables::HeaderNumbers>(*block_hash)? {
            Ok(self.tx.get::<tables::Headers>(num)?)
        } else {
            Ok(None)
        }
    }

    fn header_by_number(&self, num: BlockNumber) -> Result<Option<Header>> {
        Ok(self.tx.get::<tables::Headers>(num)?)
    }

    fn header_td(&self, hash: &BlockHash) -> Result<Option<U256>> {
        if let Some(num) = self.tx.get::<tables::HeaderNumbers>(*hash)? {
            Ok(self.tx.get::<tables::HeaderTD>(num)?.map(|td| td.0))
        } else {
            Ok(None)
        }
    }

    fn header_td_by_number(&self, number: BlockNumber) -> Result<Option<U256>> {
        Ok(self.tx.get::<tables::HeaderTD>(number)?.map(|td| td.0))
    }

    fn headers_range(&self, range: impl RangeBounds<BlockNumber>) -> Result<Vec<Header>> {
        let mut cursor = self.tx.cursor_read::<tables::Headers>()?;
        cursor
            .walk_range(range)?
            .map(|result| result.map(|(_, header)| header).map_err(Into::into))
            .collect::<Result<Vec<_>>>()
    }

    fn sealed_headers_range(
        &self,
        range: impl RangeBounds<BlockNumber>,
    ) -> Result<Vec<SealedHeader>> {
        let mut headers = vec![];
        for entry in self.tx.cursor_read::<tables::Headers>()?.walk_range(range)? {
            let (num, header) = entry?;
            let hash = read_header_hash(&self.tx, num)?;
            headers.push(header.seal(hash));
        }
        Ok(headers)
    }

    fn sealed_header(&self, number: BlockNumber) -> Result<Option<SealedHeader>> {
        if let Some(header) = self.tx.get::<tables::Headers>(number)? {
            let hash = read_header_hash(&self.tx, number)?;
            Ok(Some(header.seal(hash)))
        } else {
            Ok(None)
        }
    }
}

impl<'this, TX: DbTx<'this>> BlockHashProvider for Provider<'this, TX> {
    fn block_hash(&self, number: u64) -> Result<Option<H256>> {
        Ok(self.tx.get::<tables::CanonicalHeaders>(number)?)
    }

    fn canonical_hashes_range(&self, start: BlockNumber, end: BlockNumber) -> Result<Vec<H256>> {
        let range = start..end;
        let mut cursor = self.tx.cursor_read::<tables::CanonicalHeaders>()?;
        cursor
            .walk_range(range)?
            .map(|result| result.map(|(_, hash)| hash).map_err(Into::into))
            .collect::<Result<Vec<_>>>()
    }
}

impl<'this, TX: DbTx<'this>> BlockNumProvider for Provider<'this, TX> {
    fn chain_info(&self) -> Result<ChainInfo> {
        let best_number = self.best_block_number()?;
        let best_hash = self.block_hash(best_number)?.unwrap_or_default();
        Ok(ChainInfo { best_hash, best_number })
    }

    fn best_block_number(&self) -> Result<BlockNumber> {
        Ok(best_block_number(&self.tx)?.unwrap_or_default())
    }

    fn block_number(&self, hash: H256) -> Result<Option<BlockNumber>> {
        Ok(read_block_number(&self.tx, hash)?)
    }
}

impl<'this, TX: DbTx<'this>> BlockProvider for Provider<'this, TX> {
    fn find_block_by_hash(&self, hash: H256, source: BlockSource) -> Result<Option<Block>> {
        if source.is_database() {
            self.block(hash.into())
        } else {
            Ok(None)
        }
    }

    fn block(&self, id: BlockHashOrNumber) -> Result<Option<Block>> {
        if let Some(number) = convert_hash_or_number(&self.tx, id)? {
            if let Some(header) = read_header(&self.tx, number)? {
                // we check for shanghai first
                let (ommers, withdrawals) =
                    // TODO another: read_block_ommers_and_withdrawals
                    {
                        let mut ommers = None;
                        let mut withdrawals = None;
                        if self.chain_spec.is_shanghai_activated_at_timestamp(header.timestamp) {
                            withdrawals = read_withdrawals_by_number(&self.tx, number)?;
                        } else {
                            ommers = self.tx.get::<tables::BlockOmmers>(number)?.map(|o| o.ommers);
                        }
                        (ommers, withdrawals)
                    };

                let transactions = read_transactions_by_number(&self.tx, number)?
                    .ok_or(ProviderError::BlockBodyIndicesNotFound(number))?;

                return Ok(Some(Block {
                    header,
                    body: transactions,
                    ommers: ommers.unwrap_or_default(),
                    withdrawals,
                }))
            }
        }

        Ok(None)
    }

    fn pending_block(&self) -> Result<Option<SealedBlock>> {
        Ok(None)
    }

    fn ommers(&self, id: BlockHashOrNumber) -> Result<Option<Vec<Header>>> {
        if let Some(number) = convert_hash_or_number(&self.tx, id)? {
            // TODO: this can be optimized to return empty Vec post-merge
            let ommers = self.tx.get::<tables::BlockOmmers>(number)?.map(|o| o.ommers);
            return Ok(ommers)
        }

        Ok(None)
    }
}

impl<'this, TX: DbTx<'this>> TransactionsProvider for Provider<'this, TX> {
    fn transaction_id(&self, tx_hash: TxHash) -> Result<Option<TxNumber>> {
        Ok(self.tx.get::<tables::TxHashNumber>(tx_hash)?)
    }

    fn transaction_by_id(&self, id: TxNumber) -> Result<Option<TransactionSigned>> {
        Ok(self.tx.get::<tables::Transactions>(id)?.map(Into::into))
    }

    fn transaction_by_hash(&self, hash: TxHash) -> Result<Option<TransactionSigned>> {
        if let Some(id) = self.tx.get::<tables::TxHashNumber>(hash)? {
            Ok(self.tx.get::<tables::Transactions>(id)?)
        } else {
            Ok(None)
        }
        .map(|tx| tx.map(Into::into))
    }

    fn transaction_by_hash_with_meta(
        &self,
        tx_hash: TxHash,
    ) -> Result<Option<(TransactionSigned, TransactionMeta)>> {
        if let Some(transaction_id) = self.tx.get::<tables::TxHashNumber>(tx_hash)? {
            if let Some(transaction) = self.tx.get::<tables::Transactions>(transaction_id)? {
                let mut transaction_cursor = self.tx.cursor_read::<tables::TransactionBlock>()?;
                if let Some(block_number) =
                    transaction_cursor.seek(transaction_id).map(|b| b.map(|(_, bn)| bn))?
                {
                    if let Some((header, block_hash)) = read_sealed_header(&self.tx, block_number)?
                    {
                        if let Some(block_body) =
                            self.tx.get::<tables::BlockBodyIndices>(block_number)?
                        {
                            // the index of the tx in the block is the offset:
                            // len([start..tx_id])
                            // SAFETY: `transaction_id` is always `>=` the block's first
                            // index
                            let index = transaction_id - block_body.first_tx_num();

                            let meta = TransactionMeta {
                                tx_hash,
                                index,
                                block_hash,
                                block_number,
                                base_fee: header.base_fee_per_gas,
                            };

                            return Ok(Some((transaction.into(), meta)))
                        }
                    }
                }
            }
        }

        Ok(None)
    }

    fn transaction_block(&self, id: TxNumber) -> Result<Option<BlockNumber>> {
        let mut cursor = self.tx.cursor_read::<tables::TransactionBlock>()?;
        Ok(cursor.seek(id)?.map(|(_, bn)| bn))
    }

    fn transactions_by_block(
        &self,
        id: BlockHashOrNumber,
    ) -> Result<Option<Vec<TransactionSigned>>> {
        if let Some(number) = convert_hash_or_number(&self.tx, id)? {
            return Ok(read_transactions_by_number(&self.tx, number)?)
        }
        Ok(None)
    }

    fn transactions_by_block_range(
        &self,
        range: impl RangeBounds<BlockNumber>,
    ) -> Result<Vec<Vec<TransactionSigned>>> {
        let mut results = Vec::default();
        let mut body_cursor = self.tx.cursor_read::<tables::BlockBodyIndices>()?;
        let mut tx_cursor = self.tx.cursor_read::<tables::Transactions>()?;
        for entry in body_cursor.walk_range(range)? {
            let (_, body) = entry?;
            let tx_num_range = body.tx_num_range();
            if tx_num_range.is_empty() {
                results.push(Vec::default());
            } else {
                results.push(
                    tx_cursor
                        .walk_range(tx_num_range)?
                        .map(|result| result.map(|(_, tx)| tx.into()))
                        .collect::<std::result::Result<Vec<_>, _>>()?,
                );
            }
        }
        Ok(results)
    }
}

impl<'this, TX: DbTx<'this>> ReceiptProvider for Provider<'this, TX> {
    fn receipt(&self, id: TxNumber) -> Result<Option<Receipt>> {
        Ok(self.tx.get::<tables::Receipts>(id)?)
    }

    fn receipt_by_hash(&self, hash: TxHash) -> Result<Option<Receipt>> {
        if let Some(id) = self.tx.get::<tables::TxHashNumber>(hash)? {
            Ok(self.tx.get::<tables::Receipts>(id)?)
        } else {
            Ok(None)
        }
    }

    fn receipts_by_block(&self, block: BlockHashOrNumber) -> Result<Option<Vec<Receipt>>> {
        if let Some(number) = convert_hash_or_number(&self.tx, block)? {
            if let Some(body) = self.tx.get::<tables::BlockBodyIndices>(number)? {
                let tx_range = body.tx_num_range();
                return if tx_range.is_empty() {
                    Ok(Some(Vec::new()))
                } else {
                    let mut tx_cursor = self.tx.cursor_read::<tables::Receipts>()?;
                    let transactions = tx_cursor
                        .walk_range(tx_range)?
                        .map(|result| result.map(|(_, tx)| tx))
                        .collect::<std::result::Result<Vec<_>, _>>()?;
                    Ok(Some(transactions))
                }
            }
        }
        Ok(None)
    }
}

impl<'this, TX: DbTx<'this>> WithdrawalsProvider for Provider<'this, TX> {
    fn withdrawals_by_block(
        &self,
        id: BlockHashOrNumber,
        timestamp: u64,
    ) -> Result<Option<Vec<Withdrawal>>> {
        if self.chain_spec.is_shanghai_activated_at_timestamp(timestamp) {
            if let Some(number) = convert_hash_or_number(&self.tx, id)? {
                // If we are past shanghai, then all blocks should have a withdrawal list, even if
                // empty
                let withdrawals = read_withdrawals_by_number(&self.tx, number)?.unwrap_or_default();
                return Ok(Some(withdrawals))
            }
        }
        Ok(None)
    }

    fn latest_withdrawal(&self) -> Result<Option<Withdrawal>> {
        let latest_block_withdrawal = self.tx.cursor_read::<tables::BlockWithdrawals>()?.last();
        latest_block_withdrawal
            .map(|block_withdrawal_pair| {
                block_withdrawal_pair
                    .and_then(|(_, block_withdrawal)| block_withdrawal.withdrawals.last().cloned())
            })
            .map_err(Into::into)
    }
}

impl<'this, TX: DbTx<'this>> EvmEnvProvider for Provider<'this, TX> {
    fn fill_env_at(
        &self,
        cfg: &mut CfgEnv,
        block_env: &mut BlockEnv,
        at: BlockHashOrNumber,
    ) -> Result<()> {
        let hash = self.convert_number(at)?.ok_or(ProviderError::HeaderNotFound(at))?;
        let header = self.header(&hash)?.ok_or(ProviderError::HeaderNotFound(at))?;
        self.fill_env_with_header(cfg, block_env, &header)
    }

    fn fill_env_with_header(
        &self,
        cfg: &mut CfgEnv,
        block_env: &mut BlockEnv,
        header: &Header,
    ) -> Result<()> {
        let total_difficulty = self
            .header_td_by_number(header.number)?
            .ok_or_else(|| ProviderError::HeaderNotFound(header.number.into()))?;
        fill_cfg_and_block_env(cfg, block_env, &self.chain_spec, header, total_difficulty);
        Ok(())
    }

    fn fill_block_env_at(&self, block_env: &mut BlockEnv, at: BlockHashOrNumber) -> Result<()> {
        let hash = self.convert_number(at)?.ok_or(ProviderError::HeaderNotFound(at))?;
        let header = self.header(&hash)?.ok_or(ProviderError::HeaderNotFound(at))?;

        self.fill_block_env_with_header(block_env, &header)
    }

    fn fill_block_env_with_header(&self, block_env: &mut BlockEnv, header: &Header) -> Result<()> {
        let total_difficulty = self
            .header_td_by_number(header.number)?
            .ok_or_else(|| ProviderError::HeaderNotFound(header.number.into()))?;
        let spec_id = revm_spec(
            &self.chain_spec,
            Head {
                number: header.number,
                timestamp: header.timestamp,
                difficulty: header.difficulty,
                total_difficulty,
                // Not required
                hash: Default::default(),
            },
        );
        let after_merge = spec_id >= SpecId::MERGE;
        fill_block_env(block_env, &self.chain_spec, header, after_merge);
        Ok(())
    }

    fn fill_cfg_env_at(&self, cfg: &mut CfgEnv, at: BlockHashOrNumber) -> Result<()> {
        let hash = self.convert_number(at)?.ok_or(ProviderError::HeaderNotFound(at))?;
        let header = self.header(&hash)?.ok_or(ProviderError::HeaderNotFound(at))?;
        self.fill_cfg_env_with_header(cfg, &header)
    }

    fn fill_cfg_env_with_header(&self, cfg: &mut CfgEnv, header: &Header) -> Result<()> {
        let total_difficulty = self
            .header_td_by_number(header.number)?
            .ok_or_else(|| ProviderError::HeaderNotFound(header.number.into()))?;
        fill_cfg_env(cfg, &self.chain_spec, header, total_difficulty);
        Ok(())
    }
}

/// Returns the block number for the given block hash or number.
#[inline]
fn convert_hash_or_number<'a, TX>(
    tx: &TX,
    block: BlockHashOrNumber,
) -> std::result::Result<Option<BlockNumber>, reth_interfaces::db::DatabaseError>
where
    TX: DbTx<'a> + Send + Sync,
{
    match block {
        BlockHashOrNumber::Hash(hash) => read_block_number(tx, hash),
        BlockHashOrNumber::Number(number) => Ok(Some(number)),
    }
}

/// Reads the number for the given block hash.
#[inline]
fn read_block_number<'a, TX>(
    tx: &TX,
    hash: H256,
) -> std::result::Result<Option<BlockNumber>, reth_interfaces::db::DatabaseError>
where
    TX: DbTx<'a> + Send + Sync,
{
    tx.get::<tables::HeaderNumbers>(hash)
}

/// Reads the hash for the given block number
///
/// Returns an error if no matching entry is found.
#[inline]
fn read_header_hash<'a, TX>(
    tx: &TX,
    number: u64,
) -> std::result::Result<BlockHash, reth_interfaces::Error>
where
    TX: DbTx<'a> + Send + Sync,
{
    match tx.get::<tables::CanonicalHeaders>(number)? {
        Some(hash) => Ok(hash),
        None => Err(ProviderError::HeaderNotFound(number.into()).into()),
    }
}

/// Fetches the Withdrawals that belong to the given block number
#[inline]
fn read_transactions_by_number<'a, TX>(
    tx: &TX,
    block_number: u64,
) -> std::result::Result<Option<Vec<TransactionSigned>>, reth_interfaces::db::DatabaseError>
where
    TX: DbTx<'a> + Send + Sync,
{
    if let Some(body) = tx.get::<tables::BlockBodyIndices>(block_number)? {
        let tx_range = body.tx_num_range();
        return if tx_range.is_empty() {
            Ok(Some(Vec::new()))
        } else {
            let mut tx_cursor = tx.cursor_read::<tables::Transactions>()?;
            let transactions = tx_cursor
                .walk_range(tx_range)?
                .map(|result| result.map(|(_, tx)| tx.into()))
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(Some(transactions))
        }
    }

    Ok(None)
}

/// Fetches the Withdrawals that belong to the given block number
#[inline]
fn read_withdrawals_by_number<'a, TX>(
    tx: &TX,
    block_number: u64,
) -> std::result::Result<Option<Vec<Withdrawal>>, reth_interfaces::db::DatabaseError>
where
    TX: DbTx<'a> + Send + Sync,
{
    tx.get::<tables::BlockWithdrawals>(block_number).map(|w| w.map(|w| w.withdrawals))
}

/// Fetches the corresponding header
#[inline]
fn read_header<'a, TX>(
    tx: &TX,
    block_number: u64,
) -> std::result::Result<Option<Header>, reth_interfaces::db::DatabaseError>
where
    TX: DbTx<'a> + Send + Sync,
{
    tx.get::<tables::Headers>(block_number)
}

/// Fetches Header and its hash
#[inline]
fn read_sealed_header<'a, TX>(
    tx: &TX,
    block_number: u64,
) -> std::result::Result<Option<(Header, BlockHash)>, reth_interfaces::db::DatabaseError>
where
    TX: DbTx<'a> + Send + Sync,
{
    let block_hash = match tx.get::<tables::CanonicalHeaders>(block_number)? {
        Some(block_hash) => block_hash,
        None => return Ok(None),
    };
    match read_header(tx, block_number)? {
        Some(header) => Ok(Some((header, block_hash))),
        None => Ok(None),
    }
}

/// Fetches checks if the block number is the latest block number.
#[inline]
pub(crate) fn is_latest_block_number<'a, TX>(
    tx: &TX,
    block_number: BlockNumber,
) -> std::result::Result<bool, reth_interfaces::db::DatabaseError>
where
    TX: DbTx<'a> + Send + Sync,
{
    // check if the block number is the best block number
    // there's always at least one header in the database (genesis)
    let best = best_block_number(tx)?.unwrap_or_default();
    let last = last_canonical_header(tx)?.map(|(last, _)| last).unwrap_or_default();
    Ok(block_number == best && block_number == last)
}

/// Fetches the best block number from the database.
#[inline]
pub(crate) fn best_block_number<'a, TX>(
    tx: &TX,
) -> std::result::Result<Option<BlockNumber>, reth_interfaces::db::DatabaseError>
where
    TX: DbTx<'a> + Send + Sync,
{
    tx.get::<tables::SyncStage>("Finish".to_string())
        .map(|result| result.map(|checkpoint| checkpoint.block_number))
}

/// Fetches the last canonical header from the database.
#[inline]
pub(crate) fn last_canonical_header<'a, TX>(
    tx: &TX,
) -> std::result::Result<Option<(BlockNumber, BlockHash)>, reth_interfaces::db::DatabaseError>
where
    TX: DbTx<'a> + Send + Sync,
{
    tx.cursor_read::<tables::CanonicalHeaders>()?.last()
}