// Copyright 2024 RisingWave Labs
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

use std::collections::VecDeque;
use std::future::{pending, Future};
use std::mem::replace;
use std::sync::Arc;

use anyhow::anyhow;
use futures::future::Either;
use futures::{pin_mut, Stream, TryStreamExt};
use itertools::Itertools;
use risingwave_common::array::{Op, StreamChunk};
use risingwave_common::catalog::TableId;
use risingwave_common::metrics::LabelGuardedIntCounter;
use risingwave_common::row::OwnedRow;
use risingwave_common::util::chunk_coalesce::DataChunkBuilder;
use risingwave_hummock_sdk::HummockReadEpoch;
use risingwave_storage::error::StorageResult;
use risingwave_storage::table::batch_table::storage_table::StorageTable;
use risingwave_storage::table::ChangeLogRow;
use risingwave_storage::StateStore;
use tokio::select;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::executor::backfill::utils::{create_builder, mapping_chunk};
use crate::executor::monitor::StreamingMetrics;
use crate::executor::prelude::{try_stream, StreamExt};
use crate::executor::{
    expect_first_barrier, ActorContextRef, BackfillExecutor, Barrier, BoxedMessageStream, Execute,
    Executor, Message, Mutation, StreamExecutorError, StreamExecutorResult,
};
use crate::task::CreateMviewProgress;

pub struct SnapshotBackfillExecutor<S: StateStore> {
    /// Upstream table
    upstream_table: StorageTable<S>,

    /// Upstream with the same schema with the upstream table.
    upstream: Executor,

    /// The column indices need to be forwarded to the downstream from the upstream and table scan.
    output_indices: Vec<usize>,

    progress: CreateMviewProgress,

    chunk_size: usize,
    rate_limit: Option<usize>,

    barrier_rx: UnboundedReceiver<Barrier>,

    actor_ctx: ActorContextRef,
    metrics: Arc<StreamingMetrics>,
}

impl<S: StateStore> SnapshotBackfillExecutor<S> {
    #[expect(clippy::too_many_arguments)]
    pub fn new(
        upstream_table: StorageTable<S>,
        upstream: Executor,
        output_indices: Vec<usize>,
        actor_ctx: ActorContextRef,
        progress: CreateMviewProgress,
        chunk_size: usize,
        rate_limit: Option<usize>,
        barrier_rx: UnboundedReceiver<Barrier>,
        metrics: Arc<StreamingMetrics>,
    ) -> Self {
        if let Some(rate_limit) = rate_limit {
            debug!(
                rate_limit,
                "create snapshot backfill executor with rate limit"
            );
        }
        Self {
            upstream_table,
            upstream,
            output_indices,
            progress,
            chunk_size,
            rate_limit,
            barrier_rx,
            actor_ctx,
            metrics,
        }
    }

    #[try_stream(ok = Message, error = StreamExecutorError)]
    async fn execute_inner(mut self) {
        debug!("snapshot backfill executor start");
        let mut upstream = self.upstream.execute();
        let upstream_table_id = self.upstream_table.table_id();
        let first_barrier = expect_first_barrier(&mut upstream).await?;
        debug!(epoch = ?first_barrier.epoch, "get first upstream barrier");
        let first_recv_barrier = receive_next_barrier(&mut self.barrier_rx).await?;
        debug!(epoch = ?first_recv_barrier.epoch, "get first inject barrier");
        let should_backfill = first_barrier.epoch != first_recv_barrier.epoch;

        {
            if should_backfill {
                let subscriber_ids = first_barrier
                    .added_subscriber_on_mv_table(upstream_table_id)
                    .collect_vec();
                let snapshot_backfill_table_fragment_id = match subscriber_ids.as_slice() {
                    [] => {
                        return Err(anyhow!(
                            "first recv barrier on backfill should add subscriber on upstream"
                        )
                        .into());
                    }
                    [snapshot_backfill_table_fragment_id] => *snapshot_backfill_table_fragment_id,
                    multiple => {
                        return Err(anyhow!(
                            "first recv barrier on backfill have multiple subscribers {:?} on upstream table {}",
                            multiple, upstream_table_id.table_id
                        )
                        .into());
                    }
                };

                let table_id_str = format!("{}", self.upstream_table.table_id().table_id);
                let actor_id_str = format!("{}", self.actor_ctx.id);

                let consume_upstream_row_count = self
                    .metrics
                    .snapshot_backfill_consume_row_count
                    .with_guarded_label_values(&[&table_id_str, &actor_id_str, "consume_upstream"]);

                let mut upstream_buffer = UpstreamBuffer::new(
                    &mut upstream,
                    upstream_table_id,
                    snapshot_backfill_table_fragment_id,
                    consume_upstream_row_count,
                );

                let first_barrier_epoch = first_barrier.epoch;

                // Phase 1: consume upstream snapshot
                {
                    {
                        let consuming_snapshot_row_count = self
                            .metrics
                            .snapshot_backfill_consume_row_count
                            .with_guarded_label_values(&[
                                &table_id_str,
                                &actor_id_str,
                                "consuming_snapshot",
                            ]);
                        let snapshot_stream = make_consume_snapshot_stream(
                            &self.upstream_table,
                            first_barrier_epoch.prev,
                            self.chunk_size,
                            self.rate_limit,
                            &mut self.barrier_rx,
                            &self.output_indices,
                            self.progress,
                            first_recv_barrier,
                        );

                        pin_mut!(snapshot_stream);

                        while let Some(message) = upstream_buffer
                            .run_future(snapshot_stream.try_next())
                            .await?
                        {
                            if let Message::Chunk(chunk) = &message {
                                consuming_snapshot_row_count.inc_by(chunk.cardinality() as _);
                            }
                            yield message;
                        }
                    }

                    let recv_barrier = self.barrier_rx.recv().await.expect("should exist");
                    assert_eq!(first_barrier.epoch, recv_barrier.epoch);
                    yield Message::Barrier(first_barrier);
                }

                let mut barrier_epoch = first_barrier_epoch;

                let initial_pending_barrier = upstream_buffer.barrier.len();
                info!(
                    ?barrier_epoch,
                    table_id = self.upstream_table.table_id().table_id,
                    initial_pending_barrier,
                    "start consuming log store"
                );

                let consuming_log_store_row_count = self
                    .metrics
                    .snapshot_backfill_consume_row_count
                    .with_guarded_label_values(&[
                        &table_id_str,
                        &actor_id_str,
                        "consuming_log_store",
                    ]);

                // Phase 2: consume upstream log store
                while let Some(barrier) = upstream_buffer.take_buffered_barrier().await? {
                    let recv_barrier = receive_next_barrier(&mut self.barrier_rx).await?;
                    assert_eq!(barrier.epoch, recv_barrier.epoch);
                    assert_eq!(barrier_epoch.curr, barrier.epoch.prev);
                    barrier_epoch = barrier.epoch;

                    debug!(?barrier_epoch, kind = ?barrier.kind, "before consume change log");
                    // use `upstream_buffer.run_future` to poll upstream concurrently so that we won't have back-pressure
                    // on the upstream. Otherwise, in `batch_iter_log_with_pk_bounds`, we may wait upstream epoch to be committed,
                    // and the back-pressure may cause the upstream unable to consume the barrier and then cause deadlock.
                    let stream =
                        upstream_buffer
                            .run_future(self.upstream_table.batch_iter_log_with_pk_bounds(
                                barrier_epoch.prev,
                                barrier_epoch.prev,
                            ))
                            .await?;
                    let data_types = self.upstream_table.schema().data_types();
                    let builder = create_builder(None, self.chunk_size, data_types);
                    let stream = read_change_log(stream, builder);
                    pin_mut!(stream);
                    while let Some(chunk) = upstream_buffer.run_future(stream.try_next()).await? {
                        debug!(
                            ?barrier_epoch,
                            size = chunk.cardinality(),
                            "consume change log yield chunk",
                        );
                        consuming_log_store_row_count.inc_by(chunk.cardinality() as _);
                        yield Message::Chunk(chunk);
                    }

                    debug!(?barrier_epoch, "after consume change log");

                    yield Message::Barrier(barrier);
                }

                info!(
                    ?barrier_epoch,
                    table_id = self.upstream_table.table_id().table_id,
                    "finish consuming log store"
                );
            } else {
                info!(
                    table_id = self.upstream_table.table_id().table_id,
                    "skip backfill"
                );
                let first_recv_barrier = receive_next_barrier(&mut self.barrier_rx).await?;
                assert_eq!(first_barrier.epoch, first_recv_barrier.epoch);
                yield Message::Barrier(first_barrier);
            }
        }
        // Phase 3: consume upstream
        while let Some(msg) = upstream.try_next().await? {
            if let Message::Barrier(barrier) = &msg {
                let recv_barrier = receive_next_barrier(&mut self.barrier_rx).await?;
                assert_eq!(barrier.epoch, recv_barrier.epoch);
            }
            yield msg;
        }
    }
}

impl<S: StateStore> Execute for SnapshotBackfillExecutor<S> {
    fn execute(self: Box<Self>) -> BoxedMessageStream {
        self.execute_inner().boxed()
    }
}

#[try_stream(ok = StreamChunk, error = StreamExecutorError)]
async fn read_change_log(
    stream: impl Stream<Item = StorageResult<ChangeLogRow>>,
    mut builder: DataChunkBuilder,
) {
    let chunk_size = builder.batch_size();
    pin_mut!(stream);
    let mut ops = Vec::with_capacity(chunk_size);
    while let Some(change_log_row) = stream.try_next().await? {
        let change_log_row: ChangeLogRow = change_log_row;
        match change_log_row {
            ChangeLogRow::Insert(row) => {
                ops.push(Op::Insert);
                if let Some(chunk) = builder.append_one_row(row) {
                    let ops = replace(&mut ops, Vec::with_capacity(chunk_size));
                    yield StreamChunk::from_parts(ops, chunk);
                }
            }
            ChangeLogRow::Update {
                old_value,
                new_value,
            } => {
                if !builder.can_append(2) {
                    if let Some(chunk) = builder.consume_all() {
                        let ops = replace(&mut ops, Vec::with_capacity(chunk_size));
                        yield StreamChunk::from_parts(ops, chunk);
                    }
                }
                ops.extend([Op::UpdateDelete, Op::UpdateInsert]);
                assert!(builder.append_one_row(old_value).is_none());
                if let Some(chunk) = builder.append_one_row(new_value) {
                    let ops = replace(&mut ops, Vec::with_capacity(chunk_size));
                    yield StreamChunk::from_parts(ops, chunk);
                }
            }
            ChangeLogRow::Delete(row) => {
                ops.push(Op::Delete);
                if let Some(chunk) = builder.append_one_row(row) {
                    let ops = replace(&mut ops, Vec::with_capacity(chunk_size));
                    yield StreamChunk::from_parts(ops, chunk);
                }
            }
        }
    }

    if let Some(chunk) = builder.consume_all() {
        yield StreamChunk::from_parts(ops, chunk);
    }
}

struct UpstreamBuffer<'a> {
    upstream: &'a mut BoxedMessageStream,
    // newer barrier at the front
    barrier: VecDeque<Barrier>,
    consume_upstream_row_count: LabelGuardedIntCounter<3>,
    is_finished: bool,
    upstream_table_id: TableId,
    current_subscriber_id: u32,
}

impl<'a> UpstreamBuffer<'a> {
    fn new(
        upstream: &'a mut BoxedMessageStream,
        upstream_table_id: TableId,
        current_subscriber_id: u32,
        consume_upstream_row_count: LabelGuardedIntCounter<3>,
    ) -> Self {
        Self {
            upstream,
            barrier: Default::default(),
            consume_upstream_row_count,
            is_finished: false,
            upstream_table_id,
            current_subscriber_id,
        }
    }

    async fn concurrently_consume_upstream(&mut self) -> StreamExecutorError {
        while !self.is_finished {
            let result = self.consume_until_next_barrier().await;
            let barrier = match result {
                Ok(barrier) => barrier,
                Err(e) => {
                    return e;
                }
            };
            self.barrier.push_front(barrier);
        }
        pending().await
    }

    async fn consume_until_next_barrier(&mut self) -> StreamExecutorResult<Barrier> {
        assert!(!self.is_finished);
        loop {
            let msg: Message = self
                .upstream
                .try_next()
                .await?
                .ok_or_else(|| anyhow!("end of upstream"))?;
            match msg {
                Message::Chunk(chunk) => {
                    self.consume_upstream_row_count
                        .inc_by(chunk.cardinality() as _);
                }
                Message::Barrier(barrier) => {
                    self.is_finished = self.is_finish_barrier(&barrier);
                    break Ok(barrier);
                }
                Message::Watermark(_) => {}
            }
        }
    }

    async fn take_buffered_barrier(&mut self) -> StreamExecutorResult<Option<Barrier>> {
        Ok(if let Some(barrier) = self.barrier.pop_back() {
            Some(barrier)
        } else if self.is_finished {
            None
        } else {
            Some(self.consume_until_next_barrier().await?)
        })
    }

    fn is_finish_barrier(&self, barrier: &Barrier) -> bool {
        if let Some(Mutation::DropSubscriptions {
            subscriptions_to_drop,
        }) = barrier.mutation.as_deref()
        {
            let is_finished = subscriptions_to_drop
                .iter()
                .any(|(subscriber_id, _)| *subscriber_id == self.current_subscriber_id);
            if is_finished {
                assert!(subscriptions_to_drop.iter().any(
                    |(subscriber_id, subscribed_upstream_table_id)| {
                        *subscriber_id == self.current_subscriber_id
                            && self.upstream_table_id == *subscribed_upstream_table_id
                    }
                ))
            }
            is_finished
        } else {
            false
        }
    }

    /// Run a future while concurrently polling the upstream so that the upstream
    /// won't be back-pressured.
    async fn run_future<T, E: Into<StreamExecutorError>>(
        &mut self,
        future: impl Future<Output = Result<T, E>>,
    ) -> StreamExecutorResult<T> {
        select! {
            biased;
            e = self.concurrently_consume_upstream() => {
                Err(e)
            }
            // this arm won't be starved, because the first arm is always pending unless returning with error
            result = future => {
                result.map_err(Into::into)
            }
        }
    }
}

async fn receive_next_barrier(
    barrier_rx: &mut UnboundedReceiver<Barrier>,
) -> StreamExecutorResult<Barrier> {
    Ok(barrier_rx
        .recv()
        .await
        .ok_or_else(|| anyhow!("end of barrier receiver"))?)
}

#[try_stream(ok = StreamChunk, error = StreamExecutorError)]
async fn make_snapshot_stream<'a>(
    row_stream: impl Stream<Item = StreamExecutorResult<OwnedRow>> + 'a,
    mut builder: DataChunkBuilder,
    output_indices: &'a [usize],
) {
    pin_mut!(row_stream);
    while let Some(row) = row_stream.try_next().await? {
        if let Some(data_chunk) = builder.append_one_row(row) {
            let ops = vec![Op::Insert; data_chunk.capacity()];
            yield mapping_chunk(StreamChunk::from_parts(ops, data_chunk), output_indices);
        }
    }
    if let Some(data_chunk) = builder.consume_all() {
        let ops = vec![Op::Insert; data_chunk.capacity()];
        yield mapping_chunk(StreamChunk::from_parts(ops, data_chunk), output_indices);
    }
}

#[try_stream(ok = Message, error = StreamExecutorError)]
#[expect(clippy::too_many_arguments)]
async fn make_consume_snapshot_stream<'a, S: StateStore>(
    upstream_table: &'a StorageTable<S>,
    snapshot_epoch: u64,
    chunk_size: usize,
    rate_limit: Option<usize>,
    barrier_rx: &'a mut UnboundedReceiver<Barrier>,
    output_indices: &'a [usize],
    mut progress: CreateMviewProgress,
    first_recv_barrier: Barrier,
) {
    let mut barrier_epoch = first_recv_barrier.epoch;
    yield Message::Barrier(first_recv_barrier);

    info!(
        table_id = upstream_table.table_id().table_id,
        ?barrier_epoch,
        "start consuming snapshot"
    );

    // start consume upstream snapshot
    let snapshot_row_stream = BackfillExecutor::snapshot_read(
        upstream_table,
        HummockReadEpoch::Committed(snapshot_epoch),
        None,
    );
    let data_types = upstream_table.schema().data_types();
    let builder = create_builder(rate_limit, chunk_size, data_types.clone());
    let snapshot_stream = make_snapshot_stream(snapshot_row_stream, builder, output_indices);
    pin_mut!(snapshot_stream);

    async fn select_barrier_and_snapshot_stream(
        barrier_rx: &mut UnboundedReceiver<Barrier>,
        snapshot_stream: &mut (impl Stream<Item = StreamExecutorResult<StreamChunk>> + Unpin),
        throttle_snapshot_stream: bool,
    ) -> StreamExecutorResult<Either<Barrier, Option<StreamChunk>>> {
        select!(
            result = receive_next_barrier(barrier_rx) => {
                Ok(Either::Left(result?))
            },
            result = snapshot_stream.try_next(), if !throttle_snapshot_stream => {
                Ok(Either::Right(result?))
            }
        )
    }

    let mut count = 0;
    let mut epoch_row_count = 0;
    loop {
        let throttle_snapshot_stream = if let Some(rate_limit) = rate_limit {
            epoch_row_count > rate_limit
        } else {
            false
        };
        match select_barrier_and_snapshot_stream(
            barrier_rx,
            &mut snapshot_stream,
            throttle_snapshot_stream,
        )
        .await?
        {
            Either::Left(barrier) => {
                assert_eq!(barrier.epoch.prev, barrier_epoch.curr);
                barrier_epoch = barrier.epoch;
                if barrier_epoch.curr >= snapshot_epoch {
                    return Err(anyhow!("should not receive barrier with epoch {barrier_epoch:?} later than snapshot epoch {snapshot_epoch}").into());
                }
                debug!(?barrier_epoch, count, "update progress");
                progress.update(barrier_epoch, barrier_epoch.prev, count as _);
                epoch_row_count = 0;
                yield Message::Barrier(barrier);
            }
            Either::Right(Some(chunk)) => {
                count += chunk.cardinality();
                epoch_row_count += chunk.cardinality();
                yield Message::Chunk(chunk);
            }
            Either::Right(None) => {
                break;
            }
        }
    }

    // finish consuming upstream snapshot, report finish
    let barrier_to_report_finish = receive_next_barrier(barrier_rx).await?;
    assert_eq!(barrier_to_report_finish.epoch.prev, barrier_epoch.curr);
    barrier_epoch = barrier_to_report_finish.epoch;
    info!(?barrier_epoch, count, "report finish");
    progress.finish(barrier_epoch, count as _);
    yield Message::Barrier(barrier_to_report_finish);

    // keep receiving remaining barriers until receiving a barrier with epoch as snapshot_epoch
    loop {
        let barrier = receive_next_barrier(barrier_rx).await?;
        assert_eq!(barrier.epoch.prev, barrier_epoch.curr);
        barrier_epoch = barrier.epoch;
        yield Message::Barrier(barrier);
        if barrier_epoch.curr == snapshot_epoch {
            break;
        }
    }
    info!(?barrier_epoch, "finish consuming snapshot");
}
