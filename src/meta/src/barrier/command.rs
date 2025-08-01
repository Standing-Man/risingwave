// Copyright 2025 RisingWave Labs
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

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::fmt::Formatter;

use itertools::Itertools;
use risingwave_common::bitmap::Bitmap;
use risingwave_common::catalog::TableId;
use risingwave_common::hash::ActorMapping;
use risingwave_common::must_match;
use risingwave_common::types::Timestamptz;
use risingwave_common::util::epoch::Epoch;
use risingwave_connector::source::SplitImpl;
use risingwave_connector::source::cdc::{
    CdcTableSnapshotSplitAssignment, build_pb_actor_cdc_table_snapshot_splits,
};
use risingwave_hummock_sdk::change_log::build_table_change_log_delta;
use risingwave_meta_model::WorkerId;
use risingwave_pb::catalog::CreateType;
use risingwave_pb::common::ActorInfo;
use risingwave_pb::source::{ConnectorSplit, ConnectorSplits};
use risingwave_pb::stream_plan::barrier::BarrierKind as PbBarrierKind;
use risingwave_pb::stream_plan::barrier_mutation::Mutation;
use risingwave_pb::stream_plan::connector_props_change_mutation::ConnectorPropsInfo;
use risingwave_pb::stream_plan::throttle_mutation::RateLimit;
use risingwave_pb::stream_plan::update_mutation::*;
use risingwave_pb::stream_plan::{
    AddMutation, BarrierMutation, CombinedMutation, ConnectorPropsChangeMutation, Dispatcher,
    Dispatchers, DropSubscriptionsMutation, LoadFinishMutation, PauseMutation, ResumeMutation,
    SourceChangeSplitMutation, StartFragmentBackfillMutation, StopMutation,
    SubscriptionUpstreamInfo, ThrottleMutation, UpdateMutation,
};
use risingwave_pb::stream_service::BarrierCompleteResponse;
use tracing::warn;

use super::info::{CommandFragmentChanges, InflightDatabaseInfo, InflightStreamingJobInfo};
use crate::barrier::InflightSubscriptionInfo;
use crate::barrier::backfill_order_control::get_nodes_with_backfill_dependencies;
use crate::barrier::edge_builder::FragmentEdgeBuildResult;
use crate::barrier::info::BarrierInfo;
use crate::barrier::rpc::ControlStreamManager;
use crate::barrier::utils::collect_resp_info;
use crate::controller::fragment::{InflightActorInfo, InflightFragmentInfo};
use crate::hummock::{CommitEpochInfo, NewTableFragmentInfo};
use crate::manager::{StreamingJob, StreamingJobType};
use crate::model::{
    ActorId, ActorUpstreams, DispatcherId, FragmentActorDispatchers, FragmentDownstreamRelation,
    FragmentId, FragmentReplaceUpstream, StreamActorWithDispatchers, StreamJobActorsToCreate,
    StreamJobFragments, StreamJobFragmentsToCreate,
};
use crate::stream::{
    AutoRefreshSchemaSinkContext, ConnectorPropsChange, FragmentBackfillOrder,
    JobReschedulePostUpdates, SplitAssignment, ThrottleConfig, build_actor_connector_splits,
};

/// [`Reschedule`] is for the [`Command::RescheduleFragment`], which is used for rescheduling actors
/// in some fragment, like scaling or migrating.
#[derive(Debug, Clone)]
pub struct Reschedule {
    /// Added actors in this fragment.
    pub added_actors: HashMap<WorkerId, Vec<ActorId>>,

    /// Removed actors in this fragment.
    pub removed_actors: HashSet<ActorId>,

    /// Vnode bitmap updates for some actors in this fragment.
    pub vnode_bitmap_updates: HashMap<ActorId, Bitmap>,

    /// The upstream fragments of this fragment, and the dispatchers that should be updated.
    pub upstream_fragment_dispatcher_ids: Vec<(FragmentId, DispatcherId)>,
    /// New hash mapping of the upstream dispatcher to be updated.
    ///
    /// This field exists only when there's upstream fragment and the current fragment is
    /// hash-sharded.
    pub upstream_dispatcher_mapping: Option<ActorMapping>,

    /// The downstream fragments of this fragment.
    pub downstream_fragment_ids: Vec<FragmentId>,

    /// Reassigned splits for source actors.
    /// It becomes the `actor_splits` in [`UpdateMutation`].
    /// `Source` and `SourceBackfill` are handled together here.
    pub actor_splits: HashMap<ActorId, Vec<SplitImpl>>,

    pub newly_created_actors: HashMap<ActorId, (StreamActorWithDispatchers, WorkerId)>,

    pub cdc_table_snapshot_split_assignment: CdcTableSnapshotSplitAssignment,
}

/// Replacing an old job with a new one. All actors in the job will be rebuilt.
///
/// Current use cases:
/// - `ALTER SOURCE` (via [`Command::ReplaceStreamJob`]) will replace a source job's plan.
/// - `ALTER TABLE` (via [`Command::ReplaceStreamJob`]) and `CREATE SINK INTO table` ([`Command::CreateStreamingJob`])
///   will replace a table job's plan.
#[derive(Debug, Clone)]
pub struct ReplaceStreamJobPlan {
    pub old_fragments: StreamJobFragments,
    pub new_fragments: StreamJobFragmentsToCreate,
    /// Downstream jobs of the replaced job need to update their `Merge` node to
    /// connect to the new fragment.
    pub replace_upstream: FragmentReplaceUpstream,
    pub upstream_fragment_downstreams: FragmentDownstreamRelation,
    /// For a table with connector, the `SourceExecutor` actor will also be rebuilt with new actor ids.
    /// We need to reassign splits for it.
    ///
    /// Note that there's no `SourceBackfillExecutor` involved for table with connector, so we don't need to worry about
    /// `backfill_splits`.
    pub init_split_assignment: SplitAssignment,
    /// The `StreamingJob` info of the table to be replaced. Must be `StreamingJob::Table`
    pub streaming_job: StreamingJob,
    /// The temporary dummy job fragments id of new table fragment
    pub tmp_id: u32,
    /// The state table ids to be dropped.
    pub to_drop_state_table_ids: Vec<TableId>,
    pub auto_refresh_schema_sinks: Option<Vec<AutoRefreshSchemaSinkContext>>,
    pub cdc_table_snapshot_split_assignment: CdcTableSnapshotSplitAssignment,
}

impl ReplaceStreamJobPlan {
    fn fragment_changes(&self) -> HashMap<FragmentId, CommandFragmentChanges> {
        let mut fragment_changes = HashMap::new();
        for (fragment_id, new_fragment) in self.new_fragments.new_fragment_info() {
            let fragment_change = CommandFragmentChanges::NewFragment {
                job_id: self.streaming_job.id().into(),
                info: new_fragment,
                is_existing: false,
            };
            fragment_changes
                .try_insert(fragment_id, fragment_change)
                .expect("non-duplicate");
        }
        for fragment in self.old_fragments.fragments.values() {
            fragment_changes
                .try_insert(fragment.fragment_id, CommandFragmentChanges::RemoveFragment)
                .expect("non-duplicate");
        }
        for (fragment_id, replace_map) in &self.replace_upstream {
            fragment_changes
                .try_insert(
                    *fragment_id,
                    CommandFragmentChanges::ReplaceNodeUpstream(replace_map.clone()),
                )
                .expect("non-duplicate");
        }
        if let Some(sinks) = &self.auto_refresh_schema_sinks {
            for sink in sinks {
                let fragment_change = CommandFragmentChanges::NewFragment {
                    job_id: TableId::new(sink.original_sink.id as _),
                    info: sink.new_fragment_info(),
                    is_existing: false,
                };
                fragment_changes
                    .try_insert(sink.new_fragment.fragment_id, fragment_change)
                    .expect("non-duplicate");
                fragment_changes
                    .try_insert(
                        sink.original_fragment.fragment_id,
                        CommandFragmentChanges::RemoveFragment,
                    )
                    .expect("non-duplicate");
            }
        }
        fragment_changes
    }

    /// `old_fragment_id` -> `new_fragment_id`
    pub fn fragment_replacements(&self) -> HashMap<FragmentId, FragmentId> {
        let mut fragment_replacements = HashMap::new();
        for (upstream_fragment_id, new_upstream_fragment_id) in
            self.replace_upstream.values().flatten()
        {
            {
                let r =
                    fragment_replacements.insert(*upstream_fragment_id, *new_upstream_fragment_id);
                if let Some(r) = r {
                    assert_eq!(
                        *new_upstream_fragment_id, r,
                        "one fragment is replaced by multiple fragments"
                    );
                }
            }
        }
        fragment_replacements
    }
}

#[derive(educe::Educe, Clone)]
#[educe(Debug)]
pub struct CreateStreamingJobCommandInfo {
    #[educe(Debug(ignore))]
    pub stream_job_fragments: StreamJobFragmentsToCreate,
    pub upstream_fragment_downstreams: FragmentDownstreamRelation,
    pub init_split_assignment: SplitAssignment,
    pub definition: String,
    pub job_type: StreamingJobType,
    pub create_type: CreateType,
    pub streaming_job: StreamingJob,
    pub fragment_backfill_ordering: FragmentBackfillOrder,
    pub cdc_table_snapshot_split_assignment: CdcTableSnapshotSplitAssignment,
}

impl StreamJobFragments {
    pub(super) fn new_fragment_info(
        &self,
    ) -> impl Iterator<Item = (FragmentId, InflightFragmentInfo)> + '_ {
        self.fragments.values().map(|fragment| {
            (
                fragment.fragment_id,
                InflightFragmentInfo {
                    fragment_id: fragment.fragment_id,
                    distribution_type: fragment.distribution_type.into(),
                    nodes: fragment.nodes.clone(),
                    actors: fragment
                        .actors
                        .iter()
                        .map(|actor| {
                            (
                                actor.actor_id,
                                InflightActorInfo {
                                    worker_id: self
                                        .actor_status
                                        .get(&actor.actor_id)
                                        .expect("should exist")
                                        .worker_id()
                                        as WorkerId,
                                    vnode_bitmap: actor.vnode_bitmap.clone(),
                                },
                            )
                        })
                        .collect(),
                    state_table_ids: fragment
                        .state_table_ids
                        .iter()
                        .map(|table_id| TableId::new(*table_id))
                        .collect(),
                },
            )
        })
    }
}

#[derive(Debug, Clone)]
pub struct SnapshotBackfillInfo {
    /// `table_id` -> `Some(snapshot_backfill_epoch)`
    /// The `snapshot_backfill_epoch` should be None at the beginning, and be filled
    /// by global barrier worker when handling the command.
    pub upstream_mv_table_id_to_backfill_epoch: HashMap<TableId, Option<u64>>,
}

#[derive(Debug, Clone)]
pub enum CreateStreamingJobType {
    Normal,
    SinkIntoTable(ReplaceStreamJobPlan),
    SnapshotBackfill(SnapshotBackfillInfo),
}

/// [`Command`] is the input of [`crate::barrier::worker::GlobalBarrierWorker`]. For different commands,
/// it will [build different barriers to send](Self::to_mutation),
/// and may [do different stuffs after the barrier is collected](CommandContext::post_collect).
// FIXME: this enum is significantly large on stack, box it
#[derive(Debug)]
pub enum Command {
    /// `Flush` command will generate a checkpoint barrier. After the barrier is collected and committed
    /// all messages before the checkpoint barrier should have been committed.
    Flush,

    /// `Pause` command generates a `Pause` barrier **only if**
    /// the cluster is not already paused. Otherwise, a barrier with no mutation will be generated.
    Pause,

    /// `Resume` command generates a `Resume` barrier **only
    /// if** the cluster is paused with the same reason. Otherwise, a barrier with no mutation
    /// will be generated.
    Resume,

    /// `DropStreamingJobs` command generates a `Stop` barrier to stop the given
    /// [`Vec<ActorId>`]. The catalog has ensured that these streaming jobs are safe to be
    /// dropped by reference counts before.
    ///
    /// Barriers from the actors to be dropped will STILL be collected.
    /// After the barrier is collected, it notifies the local stream manager of compute nodes to
    /// drop actors, and then delete the job fragments info from meta store.
    DropStreamingJobs {
        table_fragments_ids: HashSet<TableId>,
        actors: Vec<ActorId>,
        unregistered_state_table_ids: HashSet<TableId>,
        unregistered_fragment_ids: HashSet<FragmentId>,
    },

    /// `CreateStreamingJob` command generates a `Add` barrier by given info.
    ///
    /// Barriers from the actors to be created, which is marked as `Inactive` at first, will STILL
    /// be collected since the barrier should be passthrough.
    ///
    /// After the barrier is collected, these newly created actors will be marked as `Running`. And
    /// it adds the job fragments info to meta store. However, the creating progress will **last
    /// for a while** until the `finish` channel is signaled, then the state of `TableFragments`
    /// will be set to `Created`.
    CreateStreamingJob {
        info: CreateStreamingJobCommandInfo,
        job_type: CreateStreamingJobType,
        cross_db_snapshot_backfill_info: SnapshotBackfillInfo,
    },
    MergeSnapshotBackfillStreamingJobs(
        HashMap<TableId, (HashSet<TableId>, InflightStreamingJobInfo)>,
    ),

    /// `Reschedule` command generates a `Update` barrier by the [`Reschedule`] of each fragment.
    /// Mainly used for scaling and migration.
    ///
    /// Barriers from which actors should be collected, and the post behavior of this command are
    /// very similar to `Create` and `Drop` commands, for added and removed actors, respectively.
    RescheduleFragment {
        reschedules: HashMap<FragmentId, Reschedule>,
        // Should contain the actor ids in upstream and downstream fragment of `reschedules`
        fragment_actors: HashMap<FragmentId, HashSet<ActorId>>,
        // Used for updating additional metadata after the barrier ends
        post_updates: JobReschedulePostUpdates,
    },

    /// `ReplaceStreamJob` command generates a `Update` barrier with the given `replace_upstream`. This is
    /// essentially switching the downstream of the old job fragments to the new ones, and
    /// dropping the old job fragments. Used for schema change.
    ///
    /// This can be treated as a special case of `RescheduleFragment`, while the upstream fragment
    /// of the Merge executors are changed additionally.
    ReplaceStreamJob(ReplaceStreamJobPlan),

    /// `SourceChangeSplit` generates a `Splits` barrier for pushing initialized splits or
    /// changed splits.
    SourceChangeSplit(SplitAssignment),

    /// `Throttle` command generates a `Throttle` barrier with the given throttle config to change
    /// the `rate_limit` of `FlowControl` Executor after `StreamScan` or Source.
    Throttle(ThrottleConfig),

    /// `CreateSubscription` command generates a `CreateSubscriptionMutation` to notify
    /// materialize executor to start storing old value for subscription.
    CreateSubscription {
        subscription_id: u32,
        upstream_mv_table_id: TableId,
        retention_second: u64,
    },

    /// `DropSubscription` command generates a `DropSubscriptionsMutation` to notify
    /// materialize executor to stop storing old value when there is no
    /// subscription depending on it.
    DropSubscription {
        subscription_id: u32,
        upstream_mv_table_id: TableId,
    },

    ConnectorPropsChange(ConnectorPropsChange),

    /// `StartFragmentBackfill` command will trigger backfilling for specified scans by `fragment_id`.
    StartFragmentBackfill {
        fragment_ids: Vec<FragmentId>,
    },

    /// `Refresh` command generates a barrier to refresh a table by truncating state
    /// and reloading data from source.
    Refresh {
        table_id: TableId,
        associated_source_id: TableId,
    },
    LoadFinish {
        table_id: TableId,
        associated_source_id: TableId,
    },
}

// For debugging and observability purposes. Can add more details later if needed.
impl std::fmt::Display for Command {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Command::Flush => write!(f, "Flush"),
            Command::Pause => write!(f, "Pause"),
            Command::Resume => write!(f, "Resume"),
            Command::DropStreamingJobs {
                table_fragments_ids,
                ..
            } => {
                write!(
                    f,
                    "DropStreamingJobs: {}",
                    table_fragments_ids.iter().sorted().join(", ")
                )
            }
            Command::CreateStreamingJob { info, .. } => {
                write!(f, "CreateStreamingJob: {}", info.streaming_job)
            }
            Command::MergeSnapshotBackfillStreamingJobs(_) => {
                write!(f, "MergeSnapshotBackfillStreamingJobs")
            }
            Command::RescheduleFragment { .. } => write!(f, "RescheduleFragment"),
            Command::ReplaceStreamJob(plan) => {
                write!(f, "ReplaceStreamJob: {}", plan.streaming_job)
            }
            Command::SourceChangeSplit(_) => write!(f, "SourceChangeSplit"),
            Command::Throttle(_) => write!(f, "Throttle"),
            Command::CreateSubscription {
                subscription_id, ..
            } => write!(f, "CreateSubscription: {subscription_id}"),
            Command::DropSubscription {
                subscription_id, ..
            } => write!(f, "DropSubscription: {subscription_id}"),
            Command::ConnectorPropsChange(_) => write!(f, "ConnectorPropsChange"),
            Command::StartFragmentBackfill { .. } => write!(f, "StartFragmentBackfill"),
            Command::Refresh {
                table_id,
                associated_source_id,
            } => write!(
                f,
                "Refresh: {} (source: {})",
                table_id, associated_source_id
            ),
            Command::LoadFinish {
                table_id,
                associated_source_id,
            } => write!(
                f,
                "LoadFinish: {} (source: {})",
                table_id, associated_source_id
            ),
        }
    }
}

impl Command {
    pub fn pause() -> Self {
        Self::Pause
    }

    pub fn resume() -> Self {
        Self::Resume
    }

    pub fn cancel(table_fragments: &StreamJobFragments) -> Self {
        Self::DropStreamingJobs {
            table_fragments_ids: HashSet::from_iter([table_fragments.stream_job_id()]),
            actors: table_fragments.actor_ids(),
            unregistered_state_table_ids: table_fragments
                .all_table_ids()
                .map(TableId::new)
                .collect(),
            unregistered_fragment_ids: table_fragments.fragment_ids().collect(),
        }
    }

    pub(crate) fn fragment_changes(&self) -> Option<HashMap<FragmentId, CommandFragmentChanges>> {
        match self {
            Command::Flush => None,
            Command::Pause => None,
            Command::Resume => None,
            Command::DropStreamingJobs {
                unregistered_fragment_ids,
                ..
            } => Some(
                unregistered_fragment_ids
                    .iter()
                    .map(|fragment_id| (*fragment_id, CommandFragmentChanges::RemoveFragment))
                    .collect(),
            ),
            Command::CreateStreamingJob { info, job_type, .. } => {
                assert!(
                    !matches!(job_type, CreateStreamingJobType::SnapshotBackfill(_)),
                    "should handle fragment changes separately for snapshot backfill"
                );
                let mut changes: HashMap<_, _> = info
                    .stream_job_fragments
                    .new_fragment_info()
                    .map(|(fragment_id, fragment_info)| {
                        (
                            fragment_id,
                            CommandFragmentChanges::NewFragment {
                                job_id: info.streaming_job.id().into(),
                                info: fragment_info,
                                is_existing: false,
                            },
                        )
                    })
                    .collect();

                if let CreateStreamingJobType::SinkIntoTable(plan) = job_type {
                    let extra_change = plan.fragment_changes();
                    changes.extend(extra_change);
                }

                Some(changes)
            }
            Command::RescheduleFragment { reschedules, .. } => Some(
                reschedules
                    .iter()
                    .map(|(fragment_id, reschedule)| {
                        (
                            *fragment_id,
                            CommandFragmentChanges::Reschedule {
                                new_actors: reschedule
                                    .added_actors
                                    .iter()
                                    .flat_map(|(node_id, actors)| {
                                        actors.iter().map(|actor_id| {
                                            (
                                                *actor_id,
                                                InflightActorInfo {
                                                    worker_id: *node_id,
                                                    vnode_bitmap: reschedule
                                                        .newly_created_actors
                                                        .get(actor_id)
                                                        .expect("should exist")
                                                        .0
                                                        .0
                                                        .vnode_bitmap
                                                        .clone(),
                                                },
                                            )
                                        })
                                    })
                                    .collect(),
                                actor_update_vnode_bitmap: reschedule
                                    .vnode_bitmap_updates
                                    .iter()
                                    .filter(|(actor_id, _)| {
                                        // only keep the existing actors
                                        !reschedule.newly_created_actors.contains_key(actor_id)
                                    })
                                    .map(|(actor_id, bitmap)| (*actor_id, bitmap.clone()))
                                    .collect(),
                                to_remove: reschedule.removed_actors.iter().cloned().collect(),
                            },
                        )
                    })
                    .collect(),
            ),
            Command::ReplaceStreamJob(plan) => Some(plan.fragment_changes()),
            Command::MergeSnapshotBackfillStreamingJobs(_) => None,
            Command::SourceChangeSplit(_) => None,
            Command::Throttle(_) => None,
            Command::CreateSubscription { .. } => None,
            Command::DropSubscription { .. } => None,
            Command::ConnectorPropsChange(_) => None,
            Command::StartFragmentBackfill { .. } => None,
            Command::Refresh { .. } => None, // Refresh doesn't change fragment structure
            Command::LoadFinish { .. } => None, // LoadFinish doesn't change fragment structure
        }
    }

    pub fn need_checkpoint(&self) -> bool {
        // todo! Reviewing the flow of different command to reduce the amount of checkpoint
        !matches!(self, Command::Resume)
    }
}

#[derive(Debug, Clone)]
pub enum BarrierKind {
    Initial,
    Barrier,
    /// Hold a list of previous non-checkpoint prev-epoch + current prev-epoch
    Checkpoint(Vec<u64>),
}

impl BarrierKind {
    pub fn to_protobuf(&self) -> PbBarrierKind {
        match self {
            BarrierKind::Initial => PbBarrierKind::Initial,
            BarrierKind::Barrier => PbBarrierKind::Barrier,
            BarrierKind::Checkpoint(_) => PbBarrierKind::Checkpoint,
        }
    }

    pub fn is_checkpoint(&self) -> bool {
        matches!(self, BarrierKind::Checkpoint(_))
    }

    pub fn is_initial(&self) -> bool {
        matches!(self, BarrierKind::Initial)
    }

    pub fn as_str_name(&self) -> &'static str {
        match self {
            BarrierKind::Initial => "Initial",
            BarrierKind::Barrier => "Barrier",
            BarrierKind::Checkpoint(_) => "Checkpoint",
        }
    }
}

/// [`CommandContext`] is used for generating barrier and doing post stuffs according to the given
/// [`Command`].
pub(super) struct CommandContext {
    subscription_info: InflightSubscriptionInfo,

    pub(super) barrier_info: BarrierInfo,

    pub(super) table_ids_to_commit: HashSet<TableId>,

    pub(super) command: Option<Command>,

    /// The tracing span of this command.
    ///
    /// Differs from [`crate::barrier::TracedEpoch`], this span focuses on the lifetime of the corresponding
    /// barrier, including the process of waiting for the barrier to be sent, flowing through the
    /// stream graph on compute nodes, and finishing its `post_collect` stuffs.
    _span: tracing::Span,
}

impl std::fmt::Debug for CommandContext {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommandContext")
            .field("barrier_info", &self.barrier_info)
            .field("command", &self.command)
            .finish()
    }
}

impl std::fmt::Display for CommandContext {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "prev_epoch={}, curr_epoch={}, kind={}",
            self.barrier_info.prev_epoch.value().0,
            self.barrier_info.curr_epoch.value().0,
            self.barrier_info.kind.as_str_name()
        )?;
        if let Some(command) = &self.command {
            write!(f, ", command={}", command)?;
        }
        Ok(())
    }
}

impl CommandContext {
    pub(super) fn new(
        barrier_info: BarrierInfo,
        subscription_info: InflightSubscriptionInfo,
        table_ids_to_commit: HashSet<TableId>,
        command: Option<Command>,
        span: tracing::Span,
    ) -> Self {
        Self {
            subscription_info,
            barrier_info,
            table_ids_to_commit,
            command,
            _span: span,
        }
    }

    fn get_truncate_epoch(&self, retention_second: u64) -> Epoch {
        let Some(truncate_timestamptz) = Timestamptz::from_secs(
            self.barrier_info
                .prev_epoch
                .value()
                .as_timestamptz()
                .timestamp()
                - retention_second as i64,
        ) else {
            warn!(retention_second, prev_epoch = ?self.barrier_info.prev_epoch.value(), "invalid retention second value");
            return self.barrier_info.prev_epoch.value();
        };
        Epoch::from_unix_millis(truncate_timestamptz.timestamp_millis() as u64)
    }

    pub(super) fn collect_commit_epoch_info(
        &self,
        info: &mut CommitEpochInfo,
        resps: Vec<BarrierCompleteResponse>,
        backfill_pinned_log_epoch: HashMap<TableId, (u64, HashSet<TableId>)>,
    ) {
        let (sst_to_context, synced_ssts, new_table_watermarks, old_value_ssts) =
            collect_resp_info(resps);

        let new_table_fragment_infos =
            if let Some(Command::CreateStreamingJob { info, job_type, .. }) = &self.command
                && !matches!(job_type, CreateStreamingJobType::SnapshotBackfill(_))
            {
                let table_fragments = &info.stream_job_fragments;
                let mut table_ids: HashSet<_> = table_fragments
                    .internal_table_ids()
                    .into_iter()
                    .map(TableId::new)
                    .collect();
                if let Some(mv_table_id) = table_fragments.mv_table_id() {
                    table_ids.insert(TableId::new(mv_table_id));
                }

                vec![NewTableFragmentInfo { table_ids }]
            } else {
                vec![]
            };

        let mut mv_log_store_truncate_epoch = HashMap::new();
        // TODO: may collect cross db snapshot backfill
        let mut update_truncate_epoch =
            |table_id: TableId, truncate_epoch| match mv_log_store_truncate_epoch
                .entry(table_id.table_id)
            {
                Entry::Occupied(mut entry) => {
                    let prev_truncate_epoch = entry.get_mut();
                    if truncate_epoch < *prev_truncate_epoch {
                        *prev_truncate_epoch = truncate_epoch;
                    }
                }
                Entry::Vacant(entry) => {
                    entry.insert(truncate_epoch);
                }
            };
        for (mv_table_id, subscriptions) in &self.subscription_info.mv_depended_subscriptions {
            if let Some(truncate_epoch) = subscriptions
                .values()
                .max()
                .map(|max_retention| self.get_truncate_epoch(*max_retention).0)
            {
                update_truncate_epoch(*mv_table_id, truncate_epoch);
            }
        }
        for (_, (backfill_epoch, upstream_mv_table_ids)) in backfill_pinned_log_epoch {
            for mv_table_id in upstream_mv_table_ids {
                update_truncate_epoch(mv_table_id, backfill_epoch);
            }
        }

        let table_new_change_log = build_table_change_log_delta(
            old_value_ssts.into_iter(),
            synced_ssts.iter().map(|sst| &sst.sst_info),
            must_match!(&self.barrier_info.kind, BarrierKind::Checkpoint(epochs) => epochs),
            mv_log_store_truncate_epoch.into_iter(),
        );

        let epoch = self.barrier_info.prev_epoch();
        for table_id in &self.table_ids_to_commit {
            info.tables_to_commit
                .try_insert(*table_id, epoch)
                .expect("non duplicate");
        }

        info.sstables.extend(synced_ssts);
        info.new_table_watermarks.extend(new_table_watermarks);
        info.sst_to_context.extend(sst_to_context);
        info.new_table_fragment_infos
            .extend(new_table_fragment_infos);
        info.change_log_delta.extend(table_new_change_log);
    }
}

impl Command {
    /// Generate a mutation for the given command.
    ///
    /// `edges` contains the information of `dispatcher`s of `DispatchExecutor` and `actor_upstreams`s of `MergeNode`
    pub(super) fn to_mutation(
        &self,
        is_currently_paused: bool,
        edges: &mut Option<FragmentEdgeBuildResult>,
        control_stream_manager: &ControlStreamManager,
    ) -> Option<Mutation> {
        match self {
            Command::Flush => None,

            Command::Pause => {
                // Only pause when the cluster is not already paused.
                // XXX: what if pause(r1) - pause(r2) - resume(r1) - resume(r2)??
                if !is_currently_paused {
                    Some(Mutation::Pause(PauseMutation {}))
                } else {
                    None
                }
            }

            Command::Resume => {
                // Only resume when the cluster is paused with the same reason.
                if is_currently_paused {
                    Some(Mutation::Resume(ResumeMutation {}))
                } else {
                    None
                }
            }

            Command::SourceChangeSplit(change) => {
                let mut diff = HashMap::new();

                for actor_splits in change.values() {
                    diff.extend(actor_splits.clone());
                }

                Some(Mutation::Splits(SourceChangeSplitMutation {
                    actor_splits: build_actor_connector_splits(&diff),
                }))
            }

            Command::Throttle(config) => {
                let mut actor_to_apply = HashMap::new();
                for per_fragment in config.values() {
                    actor_to_apply.extend(
                        per_fragment
                            .iter()
                            .map(|(actor_id, limit)| (*actor_id, RateLimit { rate_limit: *limit })),
                    );
                }

                Some(Mutation::Throttle(ThrottleMutation {
                    actor_throttle: actor_to_apply,
                }))
            }

            Command::DropStreamingJobs { actors, .. } => Some(Mutation::Stop(StopMutation {
                actors: actors.clone(),
            })),

            Command::CreateStreamingJob {
                info:
                    CreateStreamingJobCommandInfo {
                        stream_job_fragments: table_fragments,
                        init_split_assignment: split_assignment,
                        upstream_fragment_downstreams,
                        fragment_backfill_ordering,
                        cdc_table_snapshot_split_assignment,
                        ..
                    },
                job_type,
                ..
            } => {
                let edges = edges.as_mut().expect("should exist");
                let added_actors = table_fragments.actor_ids();
                let actor_splits = split_assignment
                    .values()
                    .flat_map(build_actor_connector_splits)
                    .collect();
                let subscriptions_to_add =
                    if let CreateStreamingJobType::SnapshotBackfill(snapshot_backfill_info) =
                        job_type
                    {
                        snapshot_backfill_info
                            .upstream_mv_table_id_to_backfill_epoch
                            .keys()
                            .map(|table_id| SubscriptionUpstreamInfo {
                                subscriber_id: table_fragments.stream_job_id().table_id,
                                upstream_mv_table_id: table_id.table_id,
                            })
                            .collect()
                    } else {
                        Default::default()
                    };
                let backfill_nodes_to_pause: Vec<_> =
                    get_nodes_with_backfill_dependencies(fragment_backfill_ordering)
                        .into_iter()
                        .collect();
                let add = Some(Mutation::Add(AddMutation {
                    actor_dispatchers: edges
                        .dispatchers
                        .extract_if(|fragment_id, _| {
                            upstream_fragment_downstreams.contains_key(fragment_id)
                        })
                        .flat_map(|(_, fragment_dispatchers)| fragment_dispatchers.into_iter())
                        .map(|(actor_id, dispatchers)| (actor_id, Dispatchers { dispatchers }))
                        .collect(),
                    added_actors,
                    actor_splits,
                    // If the cluster is already paused, the new actors should be paused too.
                    pause: is_currently_paused,
                    subscriptions_to_add,
                    backfill_nodes_to_pause,
                    actor_cdc_table_snapshot_splits: build_pb_actor_cdc_table_snapshot_splits(
                        cdc_table_snapshot_split_assignment.clone(),
                    ),
                }));

                if let CreateStreamingJobType::SinkIntoTable(ReplaceStreamJobPlan {
                    old_fragments,
                    init_split_assignment,
                    replace_upstream,
                    upstream_fragment_downstreams,
                    cdc_table_snapshot_split_assignment,
                    ..
                }) = job_type
                {
                    let merge_updates = edges
                        .merge_updates
                        .extract_if(|fragment_id, _| replace_upstream.contains_key(fragment_id))
                        .collect();
                    let dispatchers = edges
                        .dispatchers
                        .extract_if(|fragment_id, _| {
                            upstream_fragment_downstreams.contains_key(fragment_id)
                        })
                        .collect();
                    let update = Self::generate_update_mutation_for_replace_table(
                        old_fragments.actor_ids(),
                        merge_updates,
                        dispatchers,
                        init_split_assignment,
                        cdc_table_snapshot_split_assignment,
                    );

                    Some(Mutation::Combined(CombinedMutation {
                        mutations: vec![
                            BarrierMutation { mutation: add },
                            BarrierMutation { mutation: update },
                        ],
                    }))
                } else {
                    add
                }
            }
            Command::MergeSnapshotBackfillStreamingJobs(jobs_to_merge) => {
                Some(Mutation::DropSubscriptions(DropSubscriptionsMutation {
                    info: jobs_to_merge
                        .iter()
                        .flat_map(|(table_id, (backfill_upstream_tables, _))| {
                            backfill_upstream_tables
                                .iter()
                                .map(move |upstream_table_id| SubscriptionUpstreamInfo {
                                    subscriber_id: table_id.table_id,
                                    upstream_mv_table_id: upstream_table_id.table_id,
                                })
                        })
                        .collect(),
                }))
            }

            Command::ReplaceStreamJob(ReplaceStreamJobPlan {
                old_fragments,
                replace_upstream,
                upstream_fragment_downstreams,
                init_split_assignment,
                auto_refresh_schema_sinks,
                cdc_table_snapshot_split_assignment,
                ..
            }) => {
                let edges = edges.as_mut().expect("should exist");
                let merge_updates = edges
                    .merge_updates
                    .extract_if(|fragment_id, _| replace_upstream.contains_key(fragment_id))
                    .collect();
                let dispatchers = edges
                    .dispatchers
                    .extract_if(|fragment_id, _| {
                        upstream_fragment_downstreams.contains_key(fragment_id)
                    })
                    .collect();
                Self::generate_update_mutation_for_replace_table(
                    old_fragments.actor_ids().into_iter().chain(
                        auto_refresh_schema_sinks
                            .as_ref()
                            .into_iter()
                            .flat_map(|sinks| {
                                sinks.iter().flat_map(|sink| {
                                    sink.original_fragment
                                        .actors
                                        .iter()
                                        .map(|actor| actor.actor_id)
                                })
                            }),
                    ),
                    merge_updates,
                    dispatchers,
                    init_split_assignment,
                    cdc_table_snapshot_split_assignment,
                )
            }

            Command::RescheduleFragment {
                reschedules,
                fragment_actors,
                ..
            } => {
                let mut dispatcher_update = HashMap::new();
                for reschedule in reschedules.values() {
                    for &(upstream_fragment_id, dispatcher_id) in
                        &reschedule.upstream_fragment_dispatcher_ids
                    {
                        // Find the actors of the upstream fragment.
                        let upstream_actor_ids = fragment_actors
                            .get(&upstream_fragment_id)
                            .expect("should contain");

                        let upstream_reschedule = reschedules.get(&upstream_fragment_id);

                        // Record updates for all actors.
                        for &actor_id in upstream_actor_ids {
                            let added_downstream_actor_id = if upstream_reschedule
                                .map(|reschedule| !reschedule.removed_actors.contains(&actor_id))
                                .unwrap_or(true)
                            {
                                reschedule
                                    .added_actors
                                    .values()
                                    .flatten()
                                    .cloned()
                                    .collect()
                            } else {
                                Default::default()
                            };
                            // Index with the dispatcher id to check duplicates.
                            dispatcher_update
                                .try_insert(
                                    (actor_id, dispatcher_id),
                                    DispatcherUpdate {
                                        actor_id,
                                        dispatcher_id,
                                        hash_mapping: reschedule
                                            .upstream_dispatcher_mapping
                                            .as_ref()
                                            .map(|m| m.to_protobuf()),
                                        added_downstream_actor_id,
                                        removed_downstream_actor_id: reschedule
                                            .removed_actors
                                            .iter()
                                            .cloned()
                                            .collect(),
                                    },
                                )
                                .unwrap();
                        }
                    }
                }
                let dispatcher_update = dispatcher_update.into_values().collect();

                let mut merge_update = HashMap::new();
                for (&fragment_id, reschedule) in reschedules {
                    for &downstream_fragment_id in &reschedule.downstream_fragment_ids {
                        // Find the actors of the downstream fragment.
                        let downstream_actor_ids = fragment_actors
                            .get(&downstream_fragment_id)
                            .expect("should contain");

                        // Downstream removed actors should be skipped
                        // Newly created actors of the current fragment will not dispatch Update
                        // barriers to them
                        let downstream_removed_actors: HashSet<_> = reschedules
                            .get(&downstream_fragment_id)
                            .map(|downstream_reschedule| {
                                downstream_reschedule
                                    .removed_actors
                                    .iter()
                                    .copied()
                                    .collect()
                            })
                            .unwrap_or_default();

                        // Record updates for all actors.
                        for &actor_id in downstream_actor_ids {
                            if downstream_removed_actors.contains(&actor_id) {
                                continue;
                            }

                            // Index with the fragment id to check duplicates.
                            merge_update
                                .try_insert(
                                    (actor_id, fragment_id),
                                    MergeUpdate {
                                        actor_id,
                                        upstream_fragment_id: fragment_id,
                                        new_upstream_fragment_id: None,
                                        added_upstream_actors: reschedule
                                            .added_actors
                                            .iter()
                                            .flat_map(|(worker_id, actors)| {
                                                let host =
                                                    control_stream_manager.host_addr(*worker_id);
                                                actors.iter().map(move |actor_id| ActorInfo {
                                                    actor_id: *actor_id,
                                                    host: Some(host.clone()),
                                                })
                                            })
                                            .collect(),
                                        removed_upstream_actor_id: reschedule
                                            .removed_actors
                                            .iter()
                                            .cloned()
                                            .collect(),
                                    },
                                )
                                .unwrap();
                        }
                    }
                }
                let merge_update = merge_update.into_values().collect();

                let mut actor_vnode_bitmap_update = HashMap::new();
                for reschedule in reschedules.values() {
                    // Record updates for all actors in this fragment.
                    for (&actor_id, bitmap) in &reschedule.vnode_bitmap_updates {
                        let bitmap = bitmap.to_protobuf();
                        actor_vnode_bitmap_update
                            .try_insert(actor_id, bitmap)
                            .unwrap();
                    }
                }
                let dropped_actors = reschedules
                    .values()
                    .flat_map(|r| r.removed_actors.iter().copied())
                    .collect();
                let mut actor_splits = HashMap::new();
                let mut actor_cdc_table_snapshot_splits = HashMap::new();

                for reschedule in reschedules.values() {
                    for (actor_id, splits) in &reschedule.actor_splits {
                        actor_splits.insert(
                            *actor_id as ActorId,
                            ConnectorSplits {
                                splits: splits.iter().map(ConnectorSplit::from).collect(),
                            },
                        );
                    }
                    actor_cdc_table_snapshot_splits.extend(
                        build_pb_actor_cdc_table_snapshot_splits(
                            reschedule.cdc_table_snapshot_split_assignment.clone(),
                        ),
                    );
                }

                // we don't create dispatchers in reschedule scenario
                let actor_new_dispatchers = HashMap::new();

                let mutation = Mutation::Update(UpdateMutation {
                    dispatcher_update,
                    merge_update,
                    actor_vnode_bitmap_update,
                    dropped_actors,
                    actor_splits,
                    actor_new_dispatchers,
                    actor_cdc_table_snapshot_splits,
                });
                tracing::debug!("update mutation: {mutation:?}");
                Some(mutation)
            }

            Command::CreateSubscription {
                upstream_mv_table_id,
                subscription_id,
                ..
            } => Some(Mutation::Add(AddMutation {
                actor_dispatchers: Default::default(),
                added_actors: vec![],
                actor_splits: Default::default(),
                pause: false,
                subscriptions_to_add: vec![SubscriptionUpstreamInfo {
                    upstream_mv_table_id: upstream_mv_table_id.table_id,
                    subscriber_id: *subscription_id,
                }],
                backfill_nodes_to_pause: vec![],
                actor_cdc_table_snapshot_splits: Default::default(),
            })),
            Command::DropSubscription {
                upstream_mv_table_id,
                subscription_id,
            } => Some(Mutation::DropSubscriptions(DropSubscriptionsMutation {
                info: vec![SubscriptionUpstreamInfo {
                    subscriber_id: *subscription_id,
                    upstream_mv_table_id: upstream_mv_table_id.table_id,
                }],
            })),
            Command::ConnectorPropsChange(config) => {
                let mut connector_props_infos = HashMap::default();
                for (k, v) in config {
                    connector_props_infos.insert(
                        *k,
                        ConnectorPropsInfo {
                            connector_props_info: v.clone(),
                        },
                    );
                }
                Some(Mutation::ConnectorPropsChange(
                    ConnectorPropsChangeMutation {
                        connector_props_infos,
                    },
                ))
            }
            Command::StartFragmentBackfill { fragment_ids } => Some(
                Mutation::StartFragmentBackfill(StartFragmentBackfillMutation {
                    fragment_ids: fragment_ids.clone(),
                }),
            ),
            Command::Refresh {
                table_id,
                associated_source_id,
            } => Some(Mutation::RefreshStart(
                risingwave_pb::stream_plan::RefreshStartMutation {
                    table_id: table_id.table_id,
                    associated_source_id: associated_source_id.table_id,
                },
            )),
            Command::LoadFinish {
                table_id: _,
                associated_source_id,
            } => Some(Mutation::LoadFinish(LoadFinishMutation {
                associated_source_id: associated_source_id.table_id,
            })),
        }
    }

    pub(super) fn actors_to_create(
        &self,
        graph_info: &InflightDatabaseInfo,
        edges: &mut Option<FragmentEdgeBuildResult>,
        control_stream_manager: &ControlStreamManager,
    ) -> Option<StreamJobActorsToCreate> {
        match self {
            Command::CreateStreamingJob { info, job_type, .. } => {
                let sink_into_table_replace_plan = match job_type {
                    CreateStreamingJobType::Normal => None,
                    CreateStreamingJobType::SinkIntoTable(replace_table) => Some(replace_table),
                    CreateStreamingJobType::SnapshotBackfill(_) => {
                        // for snapshot backfill, the actors to create is measured separately
                        return None;
                    }
                };
                let get_actors_to_create = || {
                    sink_into_table_replace_plan
                        .map(|plan| plan.new_fragments.actors_to_create())
                        .into_iter()
                        .flatten()
                        .chain(info.stream_job_fragments.actors_to_create())
                };
                let edges = edges.as_mut().expect("should exist");
                Some(edges.collect_actors_to_create(get_actors_to_create()))
            }
            Command::RescheduleFragment {
                reschedules,
                fragment_actors,
                ..
            } => {
                let mut actor_upstreams = Self::collect_actor_upstreams(
                    reschedules.iter().map(|(fragment_id, reschedule)| {
                        (
                            *fragment_id,
                            reschedule.newly_created_actors.values().map(
                                |((actor, dispatchers), _)| {
                                    (actor.actor_id, dispatchers.as_slice())
                                },
                            ),
                        )
                    }),
                    Some((reschedules, fragment_actors)),
                    graph_info,
                    control_stream_manager,
                );
                let mut map: HashMap<WorkerId, HashMap<_, (_, Vec<_>)>> = HashMap::new();
                for (fragment_id, (actor, dispatchers), worker_id) in
                    reschedules.iter().flat_map(|(fragment_id, reschedule)| {
                        reschedule
                            .newly_created_actors
                            .values()
                            .map(|(actors, status)| (*fragment_id, actors, status))
                    })
                {
                    let upstreams = actor_upstreams.remove(&actor.actor_id).unwrap_or_default();
                    map.entry(*worker_id)
                        .or_default()
                        .entry(fragment_id)
                        .or_insert_with(|| {
                            let node = graph_info.fragment(fragment_id).nodes.clone();
                            (node, vec![])
                        })
                        .1
                        .push((actor.clone(), upstreams, dispatchers.clone()));
                }
                Some(map)
            }
            Command::ReplaceStreamJob(replace_table) => {
                let edges = edges.as_mut().expect("should exist");
                let mut actors =
                    edges.collect_actors_to_create(replace_table.new_fragments.actors_to_create());
                if let Some(sinks) = &replace_table.auto_refresh_schema_sinks {
                    let sink_actors = edges.collect_actors_to_create(sinks.iter().map(|sink| {
                        (
                            sink.new_fragment.fragment_id,
                            &sink.new_fragment.nodes,
                            sink.new_fragment.actors.iter().map(|actor| {
                                (
                                    actor,
                                    sink.actor_status[&actor.actor_id]
                                        .location
                                        .as_ref()
                                        .unwrap()
                                        .worker_node_id as _,
                                )
                            }),
                        )
                    }));
                    for (worker_id, fragment_actors) in sink_actors {
                        actors.entry(worker_id).or_default().extend(fragment_actors);
                    }
                }
                Some(actors)
            }
            _ => None,
        }
    }

    fn generate_update_mutation_for_replace_table(
        dropped_actors: impl IntoIterator<Item = ActorId>,
        merge_updates: HashMap<FragmentId, Vec<MergeUpdate>>,
        dispatchers: FragmentActorDispatchers,
        init_split_assignment: &SplitAssignment,
        cdc_table_snapshot_split_assignment: &CdcTableSnapshotSplitAssignment,
    ) -> Option<Mutation> {
        let dropped_actors = dropped_actors.into_iter().collect();

        let actor_new_dispatchers = dispatchers
            .into_values()
            .flatten()
            .map(|(actor_id, dispatchers)| (actor_id, Dispatchers { dispatchers }))
            .collect();

        let actor_splits = init_split_assignment
            .values()
            .flat_map(build_actor_connector_splits)
            .collect();

        Some(Mutation::Update(UpdateMutation {
            actor_new_dispatchers,
            merge_update: merge_updates.into_values().flatten().collect(),
            dropped_actors,
            actor_splits,
            actor_cdc_table_snapshot_splits: build_pb_actor_cdc_table_snapshot_splits(
                cdc_table_snapshot_split_assignment.clone(),
            ),
            ..Default::default()
        }))
    }

    /// For `CancelStreamingJob`, returns the table id of the target table.
    pub fn tables_to_drop(&self) -> impl Iterator<Item = TableId> + '_ {
        match self {
            Command::DropStreamingJobs {
                table_fragments_ids,
                ..
            } => Some(table_fragments_ids.iter().cloned()),
            _ => None,
        }
        .into_iter()
        .flatten()
    }
}

impl Command {
    #[expect(clippy::type_complexity)]
    pub(super) fn collect_actor_upstreams(
        actor_dispatchers: impl Iterator<
            Item = (FragmentId, impl Iterator<Item = (ActorId, &[Dispatcher])>),
        >,
        reschedule_dispatcher_update: Option<(
            &HashMap<FragmentId, Reschedule>,
            &HashMap<FragmentId, HashSet<ActorId>>,
        )>,
        graph_info: &InflightDatabaseInfo,
        control_stream_manager: &ControlStreamManager,
    ) -> HashMap<ActorId, ActorUpstreams> {
        let mut actor_upstreams: HashMap<ActorId, ActorUpstreams> = HashMap::new();
        for (upstream_fragment_id, upstream_actors) in actor_dispatchers {
            let upstream_fragment = graph_info.fragment(upstream_fragment_id);
            for (upstream_actor_id, dispatchers) in upstream_actors {
                let upstream_actor_location =
                    upstream_fragment.actors[&upstream_actor_id].worker_id;
                let upstream_actor_host = control_stream_manager.host_addr(upstream_actor_location);
                for downstream_actor_id in dispatchers
                    .iter()
                    .flat_map(|dispatcher| dispatcher.downstream_actor_id.iter())
                {
                    actor_upstreams
                        .entry(*downstream_actor_id)
                        .or_default()
                        .entry(upstream_fragment_id)
                        .or_default()
                        .insert(
                            upstream_actor_id,
                            ActorInfo {
                                actor_id: upstream_actor_id,
                                host: Some(upstream_actor_host.clone()),
                            },
                        );
                }
            }
        }
        if let Some((reschedules, fragment_actors)) = reschedule_dispatcher_update {
            for reschedule in reschedules.values() {
                for (upstream_fragment_id, _) in &reschedule.upstream_fragment_dispatcher_ids {
                    let upstream_fragment = graph_info.fragment(*upstream_fragment_id);
                    let upstream_reschedule = reschedules.get(upstream_fragment_id);
                    for upstream_actor_id in fragment_actors
                        .get(upstream_fragment_id)
                        .expect("should exist")
                    {
                        let upstream_actor_location =
                            upstream_fragment.actors[upstream_actor_id].worker_id;
                        let upstream_actor_host =
                            control_stream_manager.host_addr(upstream_actor_location);
                        if let Some(upstream_reschedule) = upstream_reschedule
                            && upstream_reschedule
                                .removed_actors
                                .contains(upstream_actor_id)
                        {
                            continue;
                        }
                        for (_, downstream_actor_id) in
                            reschedule
                                .added_actors
                                .iter()
                                .flat_map(|(worker_id, actors)| {
                                    actors.iter().map(|actor| (*worker_id, *actor))
                                })
                        {
                            actor_upstreams
                                .entry(downstream_actor_id)
                                .or_default()
                                .entry(*upstream_fragment_id)
                                .or_default()
                                .insert(
                                    *upstream_actor_id,
                                    ActorInfo {
                                        actor_id: *upstream_actor_id,
                                        host: Some(upstream_actor_host.clone()),
                                    },
                                );
                        }
                    }
                }
            }
        }
        actor_upstreams
    }
}
