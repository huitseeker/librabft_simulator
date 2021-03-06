// Copyright (c) Calibra Research
// SPDX-License-Identifier: Apache-2.0

use super::*;
use base_types::*;
use pacemaker::*;
use record::*;
use record_store::*;
use smr_context::SMRContext;

use std::{
    cmp::{max, min},
    collections::HashMap,
};

#[cfg(test)]
#[path = "unit_tests/node_tests.rs"]
mod node_tests;

// -- BEGIN FILE node_state --
#[derive(Debug)]
pub struct NodeState {
    /// Module dedicated to storing records for the current epoch.
    record_store: RecordStoreState,
    /// Module dedicated to leader election.
    pacemaker: PacemakerState,
    /// Current epoch.
    epoch_id: EpochId,
    /// Identity of this node.
    local_author: Author,
    /// Highest round voted so far.
    latest_voted_round: Round,
    /// Current locked round.
    locked_round: Round,
    /// Time of the latest query-all operation.
    latest_query_all_time: NodeTime,
    /// Track data to which the main handler has already reacted.
    tracker: CommitTracker,
    /// Record stores from previous epochs.
    past_record_stores: HashMap<EpochId, RecordStoreState>,
}
// -- END FILE --

// -- BEGIN FILE commit_tracker --
#[derive(Debug)]
pub struct CommitTracker {
    /// Latest epoch identifier that was processed.
    epoch_id: EpochId,
    /// Round of the latest commit that was processed.
    highest_committed_round: Round,
    /// Time of the latest commit that was processed.
    latest_commit_time: NodeTime,
    /// Minimal interval between query-all actions when no commit happens.
    target_commit_interval: Duration,
}
// -- END FILE --

impl CommitTracker {
    fn new(epoch_id: EpochId, node_time: NodeTime, target_commit_interval: Duration) -> Self {
        CommitTracker {
            epoch_id,
            highest_committed_round: Round(0),
            latest_commit_time: node_time,
            target_commit_interval,
        }
    }
}

impl NodeState {
    pub fn new(
        local_author: Author,
        initial_state: State,
        node_time: NodeTime,
        target_commit_interval: Duration,
        delta: Duration,
        gamma: f64,
        lambda: f64,
        smr_context: &SMRContext,
    ) -> NodeState {
        let epoch_id = EpochId(0);
        let tracker = CommitTracker::new(epoch_id, node_time, target_commit_interval);
        let record_store = RecordStoreState::new(
            epoch_id.initial_hash(),
            initial_state.clone(),
            epoch_id,
            smr_context.configuration(&initial_state),
        );
        NodeState {
            record_store,
            pacemaker: PacemakerState::new(epoch_id, node_time, delta, gamma, lambda),
            epoch_id,
            local_author,
            latest_voted_round: Round(0),
            locked_round: Round(0),
            latest_query_all_time: node_time,
            tracker,
            past_record_stores: HashMap::new(),
        }
    }

    pub fn epoch_id(&self) -> EpochId {
        self.epoch_id
    }

    pub fn local_author(&self) -> Author {
        self.local_author
    }

    pub fn record_store(&self) -> &RecordStore {
        &self.record_store
    }

    pub fn record_store_at(&self, epoch_id: EpochId) -> Option<&RecordStore> {
        if epoch_id == self.epoch_id {
            return Some(&self.record_store);
        }
        match self.past_record_stores.get(&epoch_id) {
            None => None,
            Some(store) => Some(&*store),
        }
    }

    pub fn pacemaker(&self) -> &Pacemaker {
        &self.pacemaker
    }

    pub fn update_tracker(&mut self, clock: NodeTime) {
        // Ignore actions
        self.tracker.update_tracker(
            self.latest_query_all_time,
            clock,
            self.epoch_id,
            &self.record_store,
        );
    }

    pub fn insert_network_record(
        &mut self,
        epoch_id: EpochId,
        record: Record,
        smr_context: &mut SMRContext,
    ) {
        if epoch_id == self.epoch_id {
            self.record_store.insert_network_record(record, smr_context);
        } else {
            debug!(
                "{:?} Skipped records outside the current epoch ({:?} instead of {:?})",
                self.local_author, epoch_id, self.epoch_id
            );
        }
    }
}

impl ActiveRound for NodeState {
    fn active_round(&self) -> Round {
        self.pacemaker.active_round()
    }
}

// -- BEGIN FILE process_pacemaker_actions --
impl NodeState {
    fn process_pacemaker_actions(
        &mut self,
        pacemaker_actions: PacemakerUpdateActions,
        clock: NodeTime,
        smr_context: &mut SMRContext,
    ) -> NodeUpdateActions {
        let mut actions = NodeUpdateActions::new();
        actions.next_scheduled_update = pacemaker_actions.next_scheduled_update;
        actions.should_broadcast = pacemaker_actions.should_broadcast;
        actions.should_query_all = pacemaker_actions.should_query_all;
        actions.should_send = pacemaker_actions.should_send;
        if let Some(round) = pacemaker_actions.should_create_timeout {
            self.record_store
                .create_timeout(self.local_author, round, smr_context);
            // Prevent voting at a round for which we have created a timeout already.
            self.latest_voted_round.max_update(round);
        }
        if let Some(previous_qc_hash) = pacemaker_actions.should_propose_block {
            self.record_store.propose_block(
                self.local_author,
                previous_qc_hash,
                clock,
                smr_context,
            );
        }
        actions
    }
}
// -- END FILE --

// -- BEGIN FILE consensus_node_impl --
impl<Context: SMRContext> ConsensusNode<Context> for NodeState {
    fn update_node(&mut self, clock: NodeTime, smr_context: &mut Context) -> NodeUpdateActions {
        // Update pacemaker state and process pacemaker actions (e.g., creating a timeout, proposing
        // a block).
        let pacemaker_actions = self.pacemaker.update_pacemaker(
            self.local_author,
            self.epoch_id,
            &self.record_store,
            self.latest_query_all_time,
            clock,
        );
        let mut actions = self.process_pacemaker_actions(pacemaker_actions, clock, smr_context);
        // Vote on a valid proposal block designated by the pacemaker, if any.
        if let Some((block_hash, block_round, proposer)) =
            self.record_store.proposed_block(&self.pacemaker)
        {
            // Enforce voting constraints.
            if block_round > self.latest_voted_round
                && self.record_store.previous_round(block_hash) >= self.locked_round
            {
                // Update the latest voted round.
                self.latest_voted_round = block_round;
                // Update the locked round.
                self.locked_round = max(
                    self.locked_round,
                    self.record_store.second_previous_round(block_hash),
                );
                // Try to execute the command contained the a block and create a vote.
                if self
                    .record_store
                    .create_vote(self.local_author, block_hash, smr_context)
                {
                    // Ask to notify and send our vote to the author of the block.
                    actions.should_send = vec![proposer];
                }
            }
        }
        // Check if our last proposal has reached a quorum of votes and create a QC.
        if self
            .record_store
            .check_for_new_quorum_certificate(self.local_author, smr_context)
        {
            // Broadcast the QC to finish our work as a leader.
            actions.should_broadcast = true;
            // Schedule a new run now to process the new QC.
            actions.next_scheduled_update = clock;
        }
        // Check for new commits and verify if we should start a new epoch.
        self.process_commits(smr_context);
        // Update the commit tracker and ask that we query all nodes if needed.
        let tracker_actions = self.tracker.update_tracker(
            self.latest_query_all_time,
            clock,
            self.epoch_id,
            &self.record_store,
        );
        actions.should_query_all = actions.should_query_all || tracker_actions.should_query_all;
        actions.next_scheduled_update = min(
            actions.next_scheduled_update,
            tracker_actions.next_scheduled_update,
        );
        // Update the time of the latest query-all action.
        if actions.should_query_all {
            self.latest_query_all_time = clock;
        }
        // Return desired actions to main handler.
        actions
    }
}
// -- END FILE --

// -- BEGIN FILE process_commits --
impl NodeState {
    pub fn process_commits(&mut self, smr_context: &mut SMRContext) {
        // For all commits that have not been processed yet, according to the commit tracker..
        for (round, state) in self
            .record_store
            .committed_states_after(self.tracker.highest_committed_round)
        {
            // .. deliver the committed state to the SMR layer, together with a commit certificate,
            // if any.
            if round == self.record_store.highest_committed_round() {
                smr_context.commit(&state, self.record_store.highest_commit_certificate())
            } else {
                smr_context.commit(&state, None);
            };
            // .. check if the current epoch just ended. If it did..
            let new_epoch_id = smr_context.read_epoch_id(&state);
            if new_epoch_id > self.epoch_id {
                // .. create a new record store and switch to the new epoch.
                let new_record_store = RecordStoreState::new(
                    new_epoch_id.initial_hash(),
                    state.clone(),
                    new_epoch_id,
                    smr_context.configuration(&state),
                );
                let old_record_store = std::mem::replace(&mut self.record_store, new_record_store);
                self.past_record_stores
                    .insert(self.epoch_id, old_record_store);
                self.epoch_id = new_epoch_id;
                // .. initialize voting constraints.
                self.latest_voted_round = Round(0);
                self.locked_round = Round(0);
                // .. stop delivering commits after an epoch change.
                break;
            }
        }
    }
}
// -- END FILE --

// -- BEGIN FILE commit_tracker_impl --
#[derive(Debug)]
pub struct CommitTrackerUpdateActions {
    /// Time at which to call `update_node` again, at the latest.
    next_scheduled_update: NodeTime,
    /// Whether we need to query all other nodes.
    should_query_all: bool,
}

impl CommitTracker {
    fn update_tracker(
        &mut self,
        latest_query_all_time: NodeTime,
        clock: NodeTime,
        current_epoch_id: EpochId,
        current_record_store: &RecordStore,
    ) -> CommitTrackerUpdateActions {
        let mut actions = CommitTrackerUpdateActions::new();
        // Update tracked values: epoch, round, and time of the latest commit.
        if current_epoch_id > self.epoch_id {
            self.epoch_id = current_epoch_id;
            self.highest_committed_round = current_record_store.highest_committed_round();
            self.latest_commit_time = clock;
        } else {
            let highest_committed_round = current_record_store.highest_committed_round();
            if highest_committed_round > self.highest_committed_round {
                self.highest_committed_round = highest_committed_round;
                self.latest_commit_time = clock;
            }
        }
        // Decide if too much time passed since the latest commit or the latest query-all action.
        let mut deadline =
            max(self.latest_commit_time, latest_query_all_time) + self.target_commit_interval;
        if clock >= deadline {
            // If yes, trigger a query-all action.
            actions.should_query_all = true;
            deadline = clock + self.target_commit_interval;
        }
        // Schedule the next update.
        actions.next_scheduled_update = deadline;
        // Return desired actions to main handler.
        actions
    }
}
// -- END FILE --

impl CommitTrackerUpdateActions {
    fn new() -> Self {
        CommitTrackerUpdateActions {
            should_query_all: false,
            next_scheduled_update: NodeTime::never(),
        }
    }
}
