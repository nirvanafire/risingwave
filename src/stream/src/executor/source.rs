// Copyright 2022 Singularity Data
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::fmt::{Debug, Formatter};
use std::sync::Arc;

use either::Either;
use futures::stream::{select_with_strategy, PollNext};
use futures::{Stream, StreamExt};
use futures_async_stream::try_stream;
use risingwave_common::array::column::Column;
use risingwave_common::array::{ArrayBuilder, ArrayImpl, I64ArrayBuilder, StreamChunk};
use risingwave_common::catalog::{ColumnId, Schema, TableId};
use risingwave_common::error::ErrorCode::InternalError;
use risingwave_common::error::{Result, RwError};
use risingwave_common::try_match_expand;
use risingwave_connector::state::SourceStateHandler;
use risingwave_connector::{ConnectorState, ConnectorStateV2, SplitImpl};
use risingwave_source::*;
use risingwave_storage::{Keyspace, StateStore};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use super::error::StreamExecutorError;
use super::monitor::StreamingMetrics;
use super::*;

/// [`SourceExecutor`] is a streaming source, from risingwave's batch table, or external systems
/// such as Kafka.
pub struct SourceExecutor<S: StateStore> {
    source_id: TableId,
    source_desc: SourceDesc,

    column_ids: Vec<ColumnId>,
    schema: Schema,
    pk_indices: PkIndices,

    /// Identity string
    identity: String,

    /// Receiver of barrier channel.
    barrier_receiver: Option<UnboundedReceiver<Barrier>>,

    // monitor
    metrics: Arc<StreamingMetrics>,

    /// Split info for stream source
    stream_source_splits: Vec<SplitImpl>,

    source_identify: String,

    split_state_store: SourceStateHandler<S>,

    // store latest split to offset mapping.
    // None if there is no update on source state since the previsouly seen barrier.
    state_cache: Option<Vec<ConnectorState>>,

    /// store the handler that bridging two channels, only modify when reconstructing chunk stream
    bridge_handler: Option<JoinHandle<UnboundedReceiver<Barrier>>>,
    bridge_stop_tx: Option<oneshot::Sender<()>>,
}

impl<S: StateStore> SourceExecutor<S> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        source_id: TableId,
        source_desc: SourceDesc,
        keyspace: Keyspace<S>,
        column_ids: Vec<ColumnId>,
        schema: Schema,
        pk_indices: PkIndices,
        barrier_receiver: UnboundedReceiver<Barrier>,
        executor_id: u64,
        _operator_id: u64,
        _op_info: String,
        streaming_metrics: Arc<StreamingMetrics>,
        stream_source_splits: Vec<SplitImpl>,
    ) -> Result<Self> {
        Ok(Self {
            source_id,
            source_desc,
            column_ids,
            schema,
            pk_indices,
            barrier_receiver: Some(barrier_receiver),
            identity: format!("SourceExecutor {:X}", executor_id),
            metrics: streaming_metrics,
            stream_source_splits,
            source_identify: "Table_".to_string() + &source_id.table_id().to_string(),
            split_state_store: SourceStateHandler::new(keyspace),
            state_cache: None,
            bridge_handler: None,
            bridge_stop_tx: None,
        })
    }

    /// Generate a row ID column.
    fn gen_row_id_column(&mut self, len: usize) -> Column {
        let mut builder = I64ArrayBuilder::new(len).unwrap();

        for _ in 0..len {
            builder
                .append(Some(self.source_desc.next_row_id()))
                .unwrap();
        }

        Column::new(Arc::new(ArrayImpl::from(builder.finish().unwrap())))
    }

    fn refill_row_id_column(&mut self, chunk: StreamChunk) -> StreamChunk {
        let row_id_index = self.source_desc.row_id_index;
        let row_id_column_id = self.source_desc.columns[row_id_index as usize].column_id;

        if let Some(idx) = self
            .column_ids
            .iter()
            .position(|column_id| *column_id == row_id_column_id)
        {
            let (ops, mut columns, bitmap) = chunk.into_inner();
            columns[idx] = self.gen_row_id_column(columns[idx].array().len());
            return StreamChunk::new(ops, columns, bitmap);
        }
        chunk
    }
}

struct SourceReader {
    /// The reader for stream source
    stream_reader: Box<dyn StreamSourceReader>,
    /// The reader for barrier
    barrier_receiver: UnboundedReceiver<Barrier>,
}

impl SourceReader {
    #[try_stream(ok = StreamChunkWithState, error = RwError)]
    async fn stream_reader(mut stream_reader: Box<dyn StreamSourceReader>) {
        loop {
            match stream_reader.next().await {
                Ok(chunk) => yield chunk,
                Err(e) => {
                    // TODO: report this error to meta service to mark the actors failed.
                    error!("hang up stream reader due to polling error: {}", e);

                    // Drop the reader, then the error might be caught by the writer side.
                    drop(stream_reader);
                    // Then hang up this stream by breaking the loop.
                    break;
                }
            }
        }

        futures::future::pending().await
    }

    #[try_stream(ok = Message, error = RwError)]
    async fn barrier_receiver(mut rx: UnboundedReceiver<Barrier>) {
        while let Some(barrier) = rx.recv().await {
            yield Message::Barrier(barrier);
        }
        return Err(RwError::from(InternalError(
            "barrier reader closed unexpectedly".to_string(),
        )));
    }

    fn into_stream(
        self,
    ) -> impl Stream<Item = Either<Result<Message>, Result<StreamChunkWithState>>> {
        let barrier_receiver = Self::barrier_receiver(self.barrier_receiver);
        let stream_reader = Self::stream_reader(self.stream_reader);
        select_with_strategy(
            barrier_receiver.map(Either::Left),
            stream_reader.map(Either::Right),
            |_: &mut ()| PollNext::Left, // perfer barrier
        )
    }
}

async fn bridge_channel(
    mut rx: UnboundedReceiver<Barrier>,
    tx: UnboundedSender<Barrier>,
    mut stop_rx: oneshot::Receiver<()>,
) -> UnboundedReceiver<Barrier> {
    loop {
        if stop_rx.try_recv().is_ok() {
            break;
        };
        tx.send(rx.recv().await.unwrap()).unwrap();
    }
    rx
}

impl<S: StateStore> SourceExecutor<S> {
    #[try_stream(ok = Message, error = StreamExecutorError)]
    async fn into_stream(mut self) {
        let mut barrier_receiver = self.barrier_receiver.take().unwrap();
        let barrier = barrier_receiver.recv().await.unwrap();
        let epoch = barrier.epoch.prev;

        yield Message::Barrier(barrier);

        let mut recover_state = ConnectorStateV2::Splits(self.stream_source_splits.clone());
        if !self.stream_source_splits.is_empty() {
            if let Ok(state) = self
                .split_state_store
                // TODO(tabVersion): try_recover_from_state_store change to return SplitImpl later
                .try_recover_from_state_store(&self.stream_source_splits[0], epoch)
                .await
            {
                recover_state = ConnectorStateV2::State(state);
            }
        }
        loop {
            // avoid using original channel
            if self.bridge_handler.is_some() {
                self.bridge_stop_tx.take().unwrap().send(()).unwrap();
                self.barrier_receiver = Some(self.bridge_handler.take().unwrap().await.unwrap());
            }
            let (tx_bk, rx_bk) = unbounded_channel::<Barrier>();
            let (stop_tx, stop_rx) = oneshot::channel::<()>();
            self.bridge_handler = Some(tokio::spawn(bridge_channel(
                self.barrier_receiver.take().unwrap(),
                tx_bk,
                stop_rx,
            )));
            self.bridge_stop_tx = Some(stop_tx);

            // todo: use epoch from msg to restore state from state store
            let stream_reader = match self.source_desc.source.as_ref() {
                SourceImpl::TableV2(t) => t
                    .stream_reader(self.column_ids.clone())
                    .await
                    .map(SourceStreamReaderImpl::TableV2),
                SourceImpl::Connector(c) => {
                    let splits = try_match_expand!(recover_state.clone(), ConnectorStateV2::Splits)
                        .expect("Parallel Connector Source only support Vec<Split> as state");
                    c.stream_reader(splits, self.column_ids.clone())
                        .await
                        .map(SourceStreamReaderImpl::Connector)
                }
            }
            .map_err(StreamExecutorError::source_error)?;

            let reader = SourceReader {
                stream_reader: Box::new(stream_reader),
                barrier_receiver: rx_bk,
            };

            #[for_await]
            for msg in reader.into_stream() {
                match msg {
                    // This branch will be preferred.
                    Either::Left(barrier) => {
                        match barrier.map_err(StreamExecutorError::source_error)? {
                            Message::Barrier(barrier) => {
                                let epoch = barrier.epoch.prev;
                                if self.state_cache.is_some() {
                                    self.split_state_store
                                        .take_snapshot(self.state_cache.clone().unwrap(), epoch)
                                        .await
                                        .map_err(|e| {
                                            StreamExecutorError::source_error(RwError::from(
                                                InternalError(e.to_string()),
                                            ))
                                        })?;
                                    self.state_cache = None;
                                }
                                if false {
                                    // TODO handle mutation here
                                    recover_state = ConnectorStateV2::Splits(vec![]);
                                    break;
                                }

                                yield Message::Barrier(barrier)
                            }
                            _ => unreachable!(),
                        }
                    }
                    Either::Right(chunk_with_state) => {
                        let chunk_with_state =
                            chunk_with_state.map_err(StreamExecutorError::source_error)?;
                        if chunk_with_state.split_offset_mapping.is_some() {
                            self.state_cache = Some(ConnectorState::from_hashmap(
                                chunk_with_state.split_offset_mapping.unwrap(),
                            ));
                        }
                        let mut chunk = chunk_with_state.chunk;

                        if !matches!(self.source_desc.source.as_ref(), SourceImpl::TableV2(_)) {
                            chunk = self.refill_row_id_column(chunk);
                        }

                        self.metrics
                            .source_output_row_count
                            .with_label_values(&[self.source_identify.as_str()])
                            .inc_by(chunk.cardinality() as u64);
                        yield Message::Chunk(chunk);
                    }
                }
            }
        }
    }
}

impl<S: StateStore> Executor for SourceExecutor<S> {
    fn execute(self: Box<Self>) -> BoxedMessageStream {
        self.into_stream().boxed()
    }

    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn pk_indices(&self) -> PkIndicesRef {
        &self.pk_indices
    }

    fn identity(&self) -> &str {
        self.identity.as_str()
    }
}

impl<S: StateStore> Debug for SourceExecutor<S> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SourceExecutor")
            .field("source_id", &self.source_id)
            .field("column_ids", &self.column_ids)
            .field("pk_indices", &self.pk_indices)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use futures::StreamExt;
    use risingwave_common::array::stream_chunk::StreamChunkTestExt;
    use risingwave_common::array::StreamChunk;
    use risingwave_common::catalog::{ColumnDesc, Field, Schema};
    use risingwave_common::types::DataType;
    use risingwave_source::*;
    use risingwave_storage::memory::MemoryStateStore;
    use tokio::sync::mpsc::unbounded_channel;

    use super::*;

    #[madsim::test]
    async fn test_table_source() -> Result<()> {
        let table_id = TableId::default();

        let rowid_type = DataType::Int64;
        let col1_type = DataType::Int32;
        let col2_type = DataType::Varchar;

        let table_columns = vec![
            ColumnDesc {
                column_id: ColumnId::from(0),
                data_type: rowid_type.clone(),
                name: String::new(),
                field_descs: vec![],
                type_name: "".to_string(),
            },
            ColumnDesc {
                column_id: ColumnId::from(1),
                data_type: col1_type.clone(),
                name: String::new(),
                field_descs: vec![],
                type_name: "".to_string(),
            },
            ColumnDesc {
                column_id: ColumnId::from(2),
                data_type: col2_type.clone(),
                name: String::new(),
                field_descs: vec![],
                type_name: "".to_string(),
            },
        ];
        let source_manager = MemSourceManager::default();
        source_manager.create_table_source(&table_id, table_columns)?;
        let source_desc = source_manager.get_source(&table_id)?;
        let source = source_desc.clone().source;

        let chunk1 = StreamChunk::from_pretty(
            " I i T
            + 0 1 foo
            + 0 2 bar
            + 0 3 baz",
        );
        let chunk2 = StreamChunk::from_pretty(
            " I i T
            + 0 4 hello
            + 0 5 .
            + 0 6 world",
        );

        let schema = Schema {
            fields: vec![
                Field::unnamed(rowid_type),
                Field::unnamed(col1_type),
                Field::unnamed(col2_type),
            ],
        };

        let column_ids = vec![0, 1, 2].into_iter().map(ColumnId::from).collect();
        let pk_indices = vec![0];

        let (barrier_sender, barrier_receiver) = unbounded_channel();
        let keyspace = Keyspace::executor_root(MemoryStateStore::new(), 0x2333);

        let executor = SourceExecutor::new(
            table_id,
            source_desc,
            keyspace,
            column_ids,
            schema,
            pk_indices,
            barrier_receiver,
            1,
            1,
            "SourceExecutor".to_string(),
            Arc::new(StreamingMetrics::new(prometheus::Registry::new())),
            vec![],
        )
        .unwrap();
        let mut executor = Box::new(executor).execute();

        let write_chunk = |chunk: StreamChunk| {
            let source = source.clone();
            madsim::task::spawn(async move {
                let table_source = source.as_table_v2().unwrap();
                table_source.blocking_write_chunk(chunk).await.unwrap();
            })
            .detach();
        };

        barrier_sender.send(Barrier::new_test_barrier(1)).unwrap();

        // Write 1st chunk
        write_chunk(chunk1);

        for _ in 0..2 {
            match executor.next().await.unwrap().unwrap() {
                Message::Chunk(chunk) => assert_eq!(
                    chunk,
                    StreamChunk::from_pretty(
                        " I i T
                        + 0 1 foo
                        + 0 2 bar
                        + 0 3 baz",
                    )
                ),
                Message::Barrier(barrier) => {
                    assert_eq!(barrier.epoch, Epoch::new_test_epoch(1))
                }
            }
        }

        // Write 2nd chunk
        write_chunk(chunk2);

        let msg = executor.next().await.unwrap().unwrap();
        assert_eq!(
            msg.into_chunk().unwrap(),
            StreamChunk::from_pretty(
                " I i T
                + 0 4 hello
                + 0 5 .
                + 0 6 world",
            )
        );

        Ok(())
    }

    #[madsim::test]
    async fn test_table_dropped() -> Result<()> {
        let table_id = TableId::default();

        let rowid_type = DataType::Int64;
        let col1_type = DataType::Int32;
        let col2_type = DataType::Varchar;

        let table_columns = vec![
            ColumnDesc {
                column_id: ColumnId::from(0),
                data_type: rowid_type.clone(),
                name: String::new(),
                field_descs: vec![],
                type_name: "".to_string(),
            },
            ColumnDesc {
                column_id: ColumnId::from(1),
                data_type: col1_type.clone(),
                name: String::new(),
                field_descs: vec![],
                type_name: "".to_string(),
            },
            ColumnDesc {
                column_id: ColumnId::from(2),
                data_type: col2_type.clone(),
                name: String::new(),
                field_descs: vec![],
                type_name: "".to_string(),
            },
        ];
        let source_manager = MemSourceManager::default();
        source_manager.create_table_source(&table_id, table_columns)?;
        let source_desc = source_manager.get_source(&table_id)?;
        let source = source_desc.clone().source;

        // Prepare test data chunks
        let chunk = StreamChunk::from_pretty(
            " I i T
            + 0 1 foo
            + 0 2 bar
            + 0 3 baz",
        );

        let schema = Schema {
            fields: vec![
                Field::unnamed(rowid_type),
                Field::unnamed(col1_type),
                Field::unnamed(col2_type),
            ],
        };

        let column_ids = vec![0.into(), 1.into(), 2.into()];
        let pk_indices = vec![0];

        let (barrier_sender, barrier_receiver) = unbounded_channel();
        let keyspace = Keyspace::executor_root(MemoryStateStore::new(), 0x2333);
        let executor = SourceExecutor::new(
            table_id,
            source_desc,
            keyspace,
            column_ids,
            schema,
            pk_indices,
            barrier_receiver,
            1,
            1,
            "SourceExecutor".to_string(),
            Arc::new(StreamingMetrics::unused()),
            vec![],
        )
        .unwrap();
        let mut executor = Box::new(executor).execute();

        let write_chunk = |chunk: StreamChunk| {
            let source = source.clone();
            madsim::task::spawn(async move {
                let table_source = source.as_table_v2().unwrap();
                table_source.blocking_write_chunk(chunk).await.unwrap();
            })
            .detach();
        };

        write_chunk(chunk.clone());

        barrier_sender
            .send(Barrier::new_test_barrier(1).with_stop())
            .unwrap();

        executor.next().await.unwrap().unwrap();
        executor.next().await.unwrap().unwrap();
        write_chunk(chunk);

        Ok(())
    }
}
