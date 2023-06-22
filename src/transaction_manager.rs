use std::collections::HashSet;
use std::net::SocketAddr;
use std::{time::Duration, time::SystemTime};

use bitcoin::consensus::deserialize;
use bitcoin::{
    blockdata::transaction::Transaction, hash_types::Txid, network::message::NetworkMessage,
    network::message_blockdata::Inventory,
};
use hashlink::LinkedHashMap;
use logger::{debug, trace, warn, ReplicaLogger};
use metrics::MetricsRegistry;

use crate::metrics::TransactionMetrics;
use crate::ProcessBitcoinNetworkMessageError;
use crate::{Channel, Command};

/// How long should the transaction manager hold on to a transaction.
const TX_CACHE_TIMEOUT_PERIOD_SECS: u64 = 10 * 60; // 10 minutes

/// Maxmimum number of transaction to advertise.
// https://developer.bitcoin.org/reference/p2p_networking.html#inv
const MAXIMUM_TRANSACTION_PER_INV: usize = 50_000;

/// Maximum number of transactions the adapter holds.
/// A transaction gets removed from the cache in two cases:
///     - Transaction times out
///     - Cache size limit is hit and this transaction is the oldest.
/// Note: This number should not be too large since it holds user generated
/// transaction data, which can be a few Mb per transaction.
const TX_CACHE_SIZE: usize = 250;

/// This struct represents the current information to track the
/// broadcasting of a transaction.
#[derive(Debug)]
struct TransactionInfo {
    /// The actual transaction to be sent to the BTC network.
    transaction: Transaction,
    /// Set of peer to which we advertised this transaction.
    advertised: HashSet<SocketAddr>,
    /// How long the transaction should be held on to.
    timeout_at: SystemTime,
}

impl TransactionInfo {
    /// This function is used to instantiate a [TransactionInfo](TransactionInfo) struct.
    fn new(transaction: &Transaction) -> Self {
        Self {
            transaction: transaction.clone(),
            advertised: HashSet::new(),
            timeout_at: SystemTime::now() + Duration::from_secs(TX_CACHE_TIMEOUT_PERIOD_SECS),
        }
    }
}

/// This struct stores the list of transactions submitted by the system component.
pub struct TransactionManager {
    /// This field contains a logger for the transaction manager to
    logger: ReplicaLogger,
    /// This field contains the transactions being tracked by the manager.
    transactions: LinkedHashMap<Txid, TransactionInfo>,
    metrics: TransactionMetrics,
}

impl TransactionManager {
    /// This function creates a new transaction manager.
    pub fn new(logger: ReplicaLogger, metrics_registry: &MetricsRegistry) -> Self {
        TransactionManager {
            logger,
            transactions: LinkedHashMap::new(),
            metrics: TransactionMetrics::new(metrics_registry),
        }
    }

    /// This heartbeat method is called periodically by the adapter.
    /// This method is used to send messages to Bitcoin peers.
    pub fn tick(&mut self, channel: &mut impl Channel) {
        self.advertise_txids(channel);
        self.reap();
        self.metrics
            .tx_store_size
            .set(self.transactions.len() as i64);
    }

    /// This method is used to send a single transaction.
    /// If the transaction is not known, the transaction is added the the transactions map.
    pub fn send_transaction(&mut self, raw_tx: &[u8]) {
        if let Ok(transaction) = deserialize::<Transaction>(raw_tx) {
            let txid = transaction.txid();
            trace!(self.logger, "Received {} from the system component", txid);
            // If hashmap has `TX_CACHE_SIZE` values we remove the oldest transaction in the cache.
            if self.transactions.len() == TX_CACHE_SIZE {
                self.transactions.pop_front();
            }
            self.transactions
                .entry(txid)
                .or_insert_with(|| TransactionInfo::new(&transaction));
        }
    }

    /// This method is used when the adapter is no longer receiving RPC calls from the replica.
    /// Clears all transactions the adapter is currently caching.
    pub fn make_idle(&mut self) {
        self.transactions.clear();
    }

    /// Clear out transactions that have been held on to for more than the transaction timeout period.
    fn reap(&mut self) {
        let now = SystemTime::now();
        self.transactions
            .retain(|tx, info| {
                if info.timeout_at < now {
                    warn!(self.logger, "Advertising bitcoin transaction {} timed out, meaning it was not picked up by any bitcoin peer.", tx);
                    false
                }
                else {
                    true
                }
            });
    }

    /// This method is used to broadcast known transaction IDs to connected peers.
    /// If the timeout period has passed for a transaction ID, it is broadcasted again.
    /// If the transaction has not been broadcasted, the transaction ID is broadcasted.
    fn advertise_txids(&mut self, channel: &mut impl Channel) {
        for address in channel.available_connections() {
            let mut inventory = vec![];
            for (txid, info) in self.transactions.iter_mut() {
                if !info.advertised.contains(&address) {
                    inventory.push(Inventory::Transaction(*txid));
                    info.advertised.insert(address);
                }
                // If the inventory contains the maximum allowed number of transactions, we will send it
                // and start building a new one.
                if inventory.len() == MAXIMUM_TRANSACTION_PER_INV {
                    debug!(self.logger, "Broadcasting Txids ({:?}) to peers", inventory);
                    for address in channel.available_connections() {
                        channel
                            .send(Command {
                                address: Some(address),
                                message: NetworkMessage::Inv(inventory.clone()),
                            })
                            .ok();
                    }
                    inventory = vec![];
                }
            }

            if inventory.is_empty() {
                continue;
            }

            debug!(
                self.logger,
                "Broadcasting Txids ({:?}) to peer {:?}", inventory, address
            );

            channel
                .send(Command {
                    address: Some(address),
                    message: NetworkMessage::Inv(inventory.clone()),
                })
                .ok();
        }
    }

    /// This method is used to process an event from the connected BTC nodes.
    /// This function processes a `getdata` message from a BTC node.
    /// If there are messages for transactions, the transaction is sent to the
    /// requesting node. Transactions sent are then removed from the cache.
    pub fn process_bitcoin_network_message(
        &mut self,
        channel: &mut impl Channel,
        addr: SocketAddr,
        message: &NetworkMessage,
    ) -> Result<(), ProcessBitcoinNetworkMessageError> {
        if let NetworkMessage::GetData(inventory) = message {
            if inventory.len() > MAXIMUM_TRANSACTION_PER_INV {
                return Err(ProcessBitcoinNetworkMessageError::InvalidMessage);
            }

            for inv in inventory {
                if let Inventory::Transaction(txid) = inv {
                    if let Some(TransactionInfo { transaction, .. }) =
                        self.transactions.get_mut(txid)
                    {
                        channel
                            .send(Command {
                                address: Some(addr),
                                message: NetworkMessage::Tx(transaction.clone()),
                            })
                            .ok();
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::common::test_common::TestChannel;
    use bitcoin::{
        blockdata::constants::genesis_block, consensus::serialize, Network, Transaction,
    };
    use logger::replica_logger::no_op_logger;
    use std::str::FromStr;

    /// This function creates a new transaction manager with a test logger.
    fn make_transaction_manager() -> TransactionManager {
        TransactionManager::new(no_op_logger(), &MetricsRegistry::default())
    }

    /// This function pulls a transaction out of the `regtest` genesis block.
    fn get_transaction() -> Transaction {
        let block = genesis_block(Network::Regtest);
        block
            .txdata
            .first()
            .cloned()
            .expect("There should be a transaction here.")
    }

    /// This function tests the `TransactionManager::reap(...)` method.
    /// Test Steps:
    /// 1. Receive a transaction
    /// 2. Attempt to reap the transaction that was just received.
    /// 3. Update the TransactionManager's `last_received_transactions_at` field to a timestamp
    ///    in the future.
    /// 4. Attempt to reap transactions again.
    #[test]
    fn test_reap() {
        let mut manager = make_transaction_manager();
        let transaction = get_transaction();
        let raw_tx = serialize(&transaction);
        manager.send_transaction(&raw_tx);
        assert_eq!(manager.transactions.len(), 1);
        manager.reap();
        assert_eq!(manager.transactions.len(), 1);

        let info = manager
            .transactions
            .get_mut(&transaction.txid())
            .expect("transaction should be map");
        info.timeout_at = SystemTime::now() - Duration::from_secs(TX_CACHE_TIMEOUT_PERIOD_SECS);
        manager.reap();
        assert_eq!(manager.transactions.len(), 0);
    }

    /// This function tests the `TransactionManager::broadcast_txids(...)` method.
    /// Test Steps:
    /// 1. Receive a transaction
    /// 2. Perform an initial broadcast.
    #[test]
    fn test_broadcast_txids() {
        let mut channel = TestChannel::new(vec![
            SocketAddr::from_str("127.0.0.1:8333").expect("invalid address")
        ]);
        let mut manager = make_transaction_manager();
        let transaction = get_transaction();
        let raw_tx = serialize(&transaction);
        let txid = transaction.txid();
        manager.send_transaction(&raw_tx);
        assert_eq!(manager.transactions.len(), 1);
        let info = manager
            .transactions
            .get(&transaction.txid())
            .expect("transaction should be map");
        assert!(info.advertised.is_empty());
        // Initial broadcast
        manager.advertise_txids(&mut channel);
        let info = manager
            .transactions
            .get_mut(&txid)
            .expect("transaction should be map");
        assert!(info.advertised.len() == 1);
        assert_eq!(channel.command_count(), 1);
        let command = channel.pop_front().expect("There should be one.");
        assert!(command.address.is_some());
        assert!(matches!(command.message, NetworkMessage::Inv(_)));
        let inventory = if let NetworkMessage::Inv(inv) = command.message {
            inv
        } else {
            vec![]
        };
        assert!(
            matches!(inventory.first().expect("should be one entry"), Inventory::Transaction(ctxid) if *ctxid == txid)
        );
    }

    /// This function tests that the oldest transaction gets removed in case of a full transaction cache.
    /// Test Steps:
    /// 1. Add transaction that should be removed to manager.
    /// 2. Add n transaction such that the first one gets evicted.
    /// 3. Make sure the first transaction is actually removed from the cache.
    #[test]
    fn test_adapter_transaction_cache_full() {
        let mut manager = make_transaction_manager();

        // Send one transaction. This transaction should be removed first if we are at capacity.
        let mut first_tx = get_transaction();
        first_tx.lock_time = u32::MAX;
        let raw_tx = serialize(&first_tx);
        manager.send_transaction(&raw_tx);

        for i in 0..TX_CACHE_SIZE {
            // First regtest genesis transaction.
            let mut transaction = get_transaction();
            // Alter transaction such that we get a different `txid`
            transaction.lock_time = i.try_into().unwrap();
            let raw_tx = serialize(&transaction);
            manager.send_transaction(&raw_tx);
        }
        assert_eq!(manager.transactions.len(), TX_CACHE_SIZE);
        assert!(manager.transactions.get(&first_tx.txid()).is_none());
    }

    /// This function tests that we don't readvertise transactions that were already advertised.
    /// Test Steps:
    /// 1. Add transaction to manager.
    /// 2. Advertise that transaction and create requests from peer.
    /// 3. Check that this transaction does not get advertised again during manager tick.
    /// 3. Check transaction advertisment is correctly tracked.
    #[test]
    fn test_adapter_dont_readvertise() {
        let address = SocketAddr::from_str("127.0.0.1:8333").expect("invalid address");
        let mut channel = TestChannel::new(vec![address]);
        let mut manager = make_transaction_manager();

        let mut transaction = get_transaction();
        transaction.lock_time = 0;
        let raw_tx = serialize(&transaction);
        manager.send_transaction(&raw_tx);
        manager.tick(&mut channel);
        channel.pop_front().unwrap();

        // Request transaction
        manager
            .process_bitcoin_network_message(
                &mut channel,
                address,
                &NetworkMessage::GetData(vec![Inventory::Transaction(transaction.txid())]),
            )
            .unwrap();
        // Send transaction
        channel.pop_front().unwrap();

        manager.tick(&mut channel);
        // Transaction should not be readvertised.
        assert_eq!(channel.command_count(), 0);
        // Transaction should be marked as advertised
        assert_eq!(
            manager
                .transactions
                .get(&transaction.txid())
                .unwrap()
                .advertised
                .len(),
            1
        );
        assert_eq!(
            manager
                .transactions
                .get(&transaction.txid())
                .unwrap()
                .advertised
                .get(&address),
            Some(&address)
        );
    }

    /// This function tests that we advertise to muliple peers and don't readvertise after
    /// first adverisment.
    /// Test Steps:
    /// 1. Add transaction to manager.
    /// 2. Advertise that transaction and request it from peer 1.
    /// 3. Check that this transaction does not get readvertised.
    #[test]
    fn test_adapter_dont_readvertise_multiple_peers() {
        let address1 = SocketAddr::from_str("127.0.0.1:8333").expect("invalid address");
        let address2 = SocketAddr::from_str("127.0.0.1:8334").expect("invalid address");
        let mut channel = TestChannel::new(vec![address1, address2]);
        let mut manager = make_transaction_manager();

        let mut transaction = get_transaction();
        transaction.lock_time = 0;
        let raw_tx = serialize(&transaction);
        manager.send_transaction(&raw_tx);
        manager.tick(&mut channel);
        // Transaction advertisment to both peers.
        assert_eq!(channel.command_count(), 2);
        channel.pop_front().unwrap();
        channel.pop_front().unwrap();

        // Request transaction from peer 1
        manager
            .process_bitcoin_network_message(
                &mut channel,
                address1,
                &NetworkMessage::GetData(vec![Inventory::Transaction(transaction.txid())]),
            )
            .unwrap();
        // Send transaction to peer 1
        channel.pop_front().unwrap();
        assert_eq!(channel.command_count(), 0);

        manager.tick(&mut channel);
        // Transaction should not be readvertised.
        assert_eq!(channel.command_count(), 0);
    }

    /// This function tests that we advertise and already advertised tx to new peers.
    /// Test Steps:
    /// 1. Add transaction to manager.
    /// 2. Advertise that transaction and request it.
    /// 3. Check that this transaction does not get readvertised to peer 1.
    /// 4. Add new peer to available connections.
    /// 5. Check that new peer get advertisment.
    #[test]
    fn test_adapter_advertise_new_peer() {
        let address1 = SocketAddr::from_str("127.0.0.1:8333").expect("invalid address");
        let mut channel = TestChannel::new(vec![address1]);
        let mut manager = make_transaction_manager();

        // 1.
        let mut transaction = get_transaction();
        transaction.lock_time = 0;
        let raw_tx = serialize(&transaction);
        manager.send_transaction(&raw_tx);
        manager.tick(&mut channel);
        assert_eq!(channel.command_count(), 1);
        channel.pop_front().unwrap();

        // 2.
        manager
            .process_bitcoin_network_message(
                &mut channel,
                address1,
                &NetworkMessage::GetData(vec![Inventory::Transaction(transaction.txid())]),
            )
            .unwrap();
        channel.pop_front().unwrap();
        assert_eq!(channel.command_count(), 0);

        // 3.
        manager.tick(&mut channel);
        assert_eq!(channel.command_count(), 0);

        // 4.
        let address2 = SocketAddr::from_str("127.0.0.2:8333").expect("invalid address");
        channel.add_address(address2);
        manager.tick(&mut channel);

        // 5.
        assert_eq!(
            channel.pop_front().unwrap(),
            Command {
                address: Some(address2),
                message: NetworkMessage::Inv(vec![Inventory::Transaction(transaction.txid())])
            }
        );
    }

    /// This function tests the `TransactionManager::process_bitcoin_network_message(...)` method.
    /// Test Steps:
    /// 1. Receive a transaction.
    /// 2. Process a [StreamEvent](StreamEvent) containing a `getdata` network message.
    /// 3. Process the outgoing commands.
    /// 4. Check the TestChannel for received outgoing commands.
    #[test]
    fn test_process_bitcoin_network_message() {
        let address = SocketAddr::from_str("127.0.0.1:8333").expect("invalid address");
        let mut channel = TestChannel::new(vec![address]);
        let mut manager = make_transaction_manager();
        let transaction = get_transaction();
        let raw_tx = serialize(&transaction);
        let txid = transaction.txid();
        manager.send_transaction(&raw_tx);
        assert_eq!(manager.transactions.len(), 1);
        manager
            .process_bitcoin_network_message(
                &mut channel,
                address,
                &NetworkMessage::GetData(vec![Inventory::Transaction(txid)]),
            )
            .ok();
        assert_eq!(channel.command_count(), 1);
        let command = channel.pop_front().unwrap();
        assert!(matches!(command.message, NetworkMessage::Tx(t) if t.txid() == txid));
    }

    /// This function tests the `TransactionManager::process_bitcoin_network_message(...)` method.
    /// Test Steps:
    /// 1. Receive a more than `MAXIMUM_TRANSACTION_PER_INV` transaction.
    /// 2. Process a [StreamEvent](StreamEvent) containing a `getdata` network message and reject.
    #[test]
    fn test_invalid_process_bitcoin_network_message() {
        let num_transaction = MAXIMUM_TRANSACTION_PER_INV + 1;
        let address = SocketAddr::from_str("127.0.0.1:8333").expect("invalid address");
        let mut channel = TestChannel::new(vec![address]);
        let mut manager = make_transaction_manager();

        let mut inventory = vec![];
        for i in 0..num_transaction {
            // First regtest genesis transaction.
            let mut transaction = get_transaction();
            // Alter transaction such that we get a different `txid`
            transaction.lock_time = i.try_into().unwrap();
            let txid = transaction.txid();
            inventory.push(Inventory::Transaction(txid));
        }
        manager
            .process_bitcoin_network_message(
                &mut channel,
                address,
                &NetworkMessage::GetData(inventory),
            )
            .unwrap_err();
    }

    /// This function tests the `TransactionManager::tick(...)` method.
    /// Test Steps:
    /// 1. Receive a transaction.
    /// 2. Process a [StreamEvent](StreamEvent) containing a `getdata` network message.
    /// 3. Call the manager's `tick` method.
    /// 4. Check the TestChannel for received outgoing commands for an `inv` message and a `tx` message.
    #[test]
    fn test_tick() {
        let address = SocketAddr::from_str("127.0.0.1:8333").expect("invalid address");
        let mut channel = TestChannel::new(vec![address]);
        let mut manager = make_transaction_manager();
        let transaction = get_transaction();
        let raw_tx = serialize(&transaction);
        let txid = transaction.txid();
        manager.send_transaction(&raw_tx);
        manager.tick(&mut channel);
        manager
            .process_bitcoin_network_message(
                &mut channel,
                address,
                &NetworkMessage::GetData(vec![Inventory::Transaction(txid)]),
            )
            .ok();
        assert_eq!(channel.command_count(), 2);
        assert_eq!(manager.transactions.len(), 1);

        let command = channel.pop_front().unwrap();
        assert!(matches!(command.message, NetworkMessage::Inv(_)));
        let inventory = if let NetworkMessage::Inv(inv) = command.message {
            inv
        } else {
            vec![]
        };
        assert!(
            matches!(inventory.first().expect("should be one entry"), Inventory::Transaction(ctxid) if *ctxid == txid)
        );

        let command = channel.pop_front().unwrap();
        assert!(matches!(command.message, NetworkMessage::Tx(t) if t.txid() == txid));

        manager.send_transaction(&raw_tx);
        let info = manager
            .transactions
            .get_mut(&transaction.txid())
            .expect("transaction should be in the map");
        info.timeout_at = SystemTime::now() - Duration::from_secs(TX_CACHE_TIMEOUT_PERIOD_SECS);
        manager.tick(&mut channel);
        assert_eq!(manager.transactions.len(), 0);
    }

    /// Test to ensure that when `TransactionManager.idle(...)` is called that the `transactions`
    /// field is cleared.
    #[test]
    fn test_make_idle() {
        let mut manager = make_transaction_manager();
        let transaction = get_transaction();
        let raw_tx = serialize(&transaction);
        let txid = transaction.txid();

        manager.send_transaction(&raw_tx);

        assert_eq!(manager.transactions.len(), 1);
        assert!(manager.transactions.contains_key(&txid));

        manager.make_idle();
        assert_eq!(manager.transactions.len(), 0);
        assert!(!manager.transactions.contains_key(&txid));
    }
}
