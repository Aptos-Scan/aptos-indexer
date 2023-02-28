use std::{collections::HashMap, fmt::Debug};

use async_trait::async_trait;
use diesel::{ExpressionMethods, pg::upsert::excluded, PgConnection, result::Error};
use field_count::FieldCount;

use aptos_api_types::Transaction;

use crate::{
    database::{
        clean_data_for_db, execute_with_better_error, get_chunks, PgDbPool, PgPoolConnection,
    },
    indexer::{
        errors::TransactionProcessingError, processing_result::ProcessingResult,
        transaction_processor::TransactionProcessor,
    },
    models::{
        block_metadata_transactions::BlockMetadataTransactionModel,
        events::EventModel,
        move_modules::MoveModule,
        move_resources::MoveResource,
        move_tables::{CurrentTableItem, TableItem, TableMetadata},
        signatures::Signature,
        transactions::{TransactionDetail, TransactionModel},
        user_transactions::UserTransactionModel,
        write_set_changes::{WriteSetChangeDetail, WriteSetChangeModel},
    },
    schema,
};
use crate::driver::publisher::Publisher;

pub const NAME: &str = "custom_processor";

pub struct CustomTransactionProcessor {
    connection_pool: PgDbPool,
    publisher: Publisher,
}

impl CustomTransactionProcessor {
    pub fn new(connection_pool: PgDbPool, publisher: Publisher) -> Self {
        Self {
            connection_pool,
            publisher,
        }
    }
}

impl Debug for CustomTransactionProcessor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = &self.connection_pool.state();
        write!(
            f,
            "DefaultTransactionProcessor {{ connections: {:?}  idle_connections: {:?} }}",
            state.connections, state.idle_connections
        )
    }
}

#[async_trait]
impl TransactionProcessor for CustomTransactionProcessor {
    fn name(&self) -> &'static str {
        NAME
    }

    async fn process_transactions(
        &self,
        transactions: Vec<Transaction>,
        start_version: u64,
        end_version: u64,
    ) -> Result<ProcessingResult, TransactionProcessingError> {
        let (txns, txn_details, events, write_set_changes, wsc_details) =
            TransactionModel::from_transactions(&transactions);

        let mut signatures = vec![];
        let mut user_transactions = vec![];
        let mut block_metadata_transactions = vec![];
        for detail in txn_details {
            match detail {
                TransactionDetail::User(user_txn, sigs) => {
                    signatures.append(&mut sigs.clone());
                    user_transactions.push(user_txn.clone());
                }
                TransactionDetail::BlockMetadata(bmt) => {
                    block_metadata_transactions.push(bmt.clone())
                }
            }
        }
        let mut move_modules = vec![];
        let mut move_resources = vec![];
        let mut table_items = vec![];
        let mut current_table_items = HashMap::new();
        let mut table_metadata = HashMap::new();
        for detail in wsc_details {
            match detail {
                WriteSetChangeDetail::Module(module) => move_modules.push(module.clone()),
                WriteSetChangeDetail::Resource(resource) => move_resources.push(resource.clone()),
                WriteSetChangeDetail::Table(item, current_item, metadata) => {
                    table_items.push(item.clone());
                    current_table_items.insert(
                        (
                            current_item.table_handle.clone(),
                            current_item.key_hash.clone(),
                        ),
                        current_item.clone(),
                    );
                    if let Some(meta) = metadata {
                        table_metadata.insert(meta.handle.clone(), meta.clone());
                    }
                }
            }
        }
        // Getting list of values and sorting by pk in order to avoid postgres deadlock since we're doing multi threaded db writes
        let mut current_table_items = current_table_items
            .into_values()
            .collect::<Vec<CurrentTableItem>>();
        let mut table_metadata = table_metadata.into_values().collect::<Vec<TableMetadata>>();
        // Sort by PK
        current_table_items
            .sort_by(|a, b| (&a.table_handle, &a.key_hash).cmp(&(&b.table_handle, &b.key_hash)));
        table_metadata.sort_by(|a, b| a.handle.cmp(&b.handle));

        let mut conn = self.get_conn();
        let tx_result = insert_to_db(
            &self.publisher,
            &mut conn,
            self.name(),
            start_version,
            end_version,
            txns,
            (user_transactions, signatures, block_metadata_transactions),
            events,
            write_set_changes,
            (
                move_modules,
                move_resources,
                table_items,
                current_table_items,
                table_metadata,
            ),
        );
        match tx_result {
            Ok(_) => Ok(ProcessingResult::new(
                self.name(),
                start_version,
                end_version,
            )),
            Err(err) => Err(TransactionProcessingError::TransactionCommitError((
                anyhow::Error::from(err),
                start_version,
                end_version,
                self.name(),
            ))),
        }
    }

    fn connection_pool(&self) -> &PgDbPool {
        &self.connection_pool
    }
}

fn insert_to_db(
    publisher: &Publisher,
    conn: &mut PgPoolConnection,
    name: &'static str,
    start_version: u64,
    end_version: u64,
    txns: Vec<TransactionModel>,
    txn_details: (
        Vec<UserTransactionModel>,
        Vec<Signature>,
        Vec<BlockMetadataTransactionModel>,
    ),
    events: Vec<EventModel>,
    wscs: Vec<WriteSetChangeModel>,
    wsc_details: (
        Vec<MoveModule>,
        Vec<MoveResource>,
        Vec<TableItem>,
        Vec<CurrentTableItem>,
        Vec<TableMetadata>,
    ),
) -> Result<(), diesel::result::Error> {
    aptos_logger::trace!(
        name = name,
        start_version = start_version,
        end_version = end_version,
        "Inserting to db",
    );
    let (user_transactions, signatures, block_metadata_transactions) = txn_details;
    let (move_modules, move_resources, table_items, current_table_items, table_metadata) =
        wsc_details;
    match conn
        .build_transaction()
        .read_write()
        .run::<_, Error, _>(|pg_conn| {
            insert_to_db_impl(
                publisher,
                pg_conn,
                &txns,
                (
                    &user_transactions,
                    &signatures,
                    &block_metadata_transactions,
                ),
                &events,
                &wscs,
                (
                    &move_modules,
                    &move_resources,
                    &table_items,
                    &current_table_items,
                    &table_metadata,
                ),
            )
        }) {
        Ok(_) => Ok(()),
        Err(_) => {
            let txns = clean_data_for_db(txns, true);
            let user_transactions = clean_data_for_db(user_transactions, true);
            let signatures = clean_data_for_db(signatures, true);
            let block_metadata_transactions = clean_data_for_db(block_metadata_transactions, true);
            let events = clean_data_for_db(events, true);
            let wscs = clean_data_for_db(wscs, true);
            let move_modules = clean_data_for_db(move_modules, true);
            let move_resources = clean_data_for_db(move_resources, true);
            let table_items = clean_data_for_db(table_items, true);
            let current_table_items = clean_data_for_db(current_table_items, true);
            let table_metadata = clean_data_for_db(table_metadata, true);

            conn.build_transaction()
                .read_write()
                .run::<_, Error, _>(|pg_conn| {
                    insert_to_db_impl(
                        publisher,
                        pg_conn,
                        &txns,
                        (
                            &user_transactions,
                            &signatures,
                            &block_metadata_transactions,
                        ),
                        &events,
                        &wscs,
                        (
                            &move_modules,
                            &move_resources,
                            &table_items,
                            &current_table_items,
                            &table_metadata,
                        ),
                    )
                })
        }
    }
}

fn insert_to_db_impl(
    publisher: &Publisher,
    conn: &mut PgConnection,
    txns: &[TransactionModel],
    txn_details: (
        &[UserTransactionModel],
        &[Signature],
        &[BlockMetadataTransactionModel],
    ),
    events: &[EventModel],
    wscs: &[WriteSetChangeModel],
    wsc_details: (
        &[MoveModule],
        &[MoveResource],
        &[TableItem],
        &[CurrentTableItem],
        &[TableMetadata],
    ),
) -> Result<(), diesel::result::Error> {
    let (user_transactions, signatures, block_metadata_transactions) = txn_details;
    let (move_modules, move_resources, table_items, current_table_items, table_metadata) =
        wsc_details;
    insert_transactions(publisher, txns)?;
    // insert_user_transactions(conn, user_transactions)?;
    // insert_signatures(conn, signatures)?;
    // insert_block_metadata_transactions(conn, block_metadata_transactions)?;
    // insert_events(conn, events)?;
    // insert_write_set_changes(conn, wscs)?;
    // insert_move_modules(conn, move_modules)?;
    // insert_move_resources(conn, move_resources)?;
    // insert_table_items(conn, table_items)?;
    // insert_current_table_items(conn, current_table_items)?;
    // insert_table_metadata(conn, table_metadata)?;
    Ok(())
}

fn insert_transactions(
    publisher: &Publisher,
    items_to_insert: &[TransactionModel],
) -> Result<(), diesel::result::Error> {
    // TODO: publish transactions to kafka
    publisher.send_txs(items_to_insert);

    // use schema::transactions::dsl::*;
    // let chunks = get_chunks(items_to_insert.len(), TransactionModel::field_count());
    // for (start_ind, end_ind) in chunks {
    //     execute_with_better_error(
    //         conn,
    //         diesel::insert_into(schema::transactions::table)
    //             .values(&items_to_insert[start_ind..end_ind])
    //             .on_conflict(version)
    //             .do_nothing(),
    //         None,
    //     )?;
    // }
    Ok(())
}
