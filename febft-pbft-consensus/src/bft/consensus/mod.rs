pub mod decision;
pub mod accessory;

use std::collections::VecDeque;
use std::iter;
use std::sync::Arc;
use chrono::Utc;
use either::Either;
use log::{debug, warn};
use socket2::Protocol;
use febft_common::error::*;
use febft_common::crypto::hash::Digest;
use febft_common::globals::ReadOnly;
use febft_common::node_id::NodeId;
use febft_common::ordering::{InvalidSeqNo, Orderable, SeqNo, tbo_advance_message_queue, tbo_advance_message_queue_return, tbo_queue_message};
use febft_communication::message::{Header, StoredMessage};
use febft_communication::Node;
use febft_execution::ExecutorHandle;
use febft_execution::serialize::SharedData;
use febft_messages::serialize::StateTransferMessage;
use febft_messages::timeouts::Timeouts;
use crate::bft::consensus::decision::{ConsensusDecision, MessageQueue};
use crate::bft::message::{ConsensusMessage, ConsensusMessageKind};
use crate::bft::msg_log::decided_log::Log;
use crate::bft::msg_log::deciding_log::{CompletedBatch, DecidingLog};
use crate::bft::PBFT;
use crate::bft::sync::{AbstractSynchronizer, Synchronizer};
use crate::bft::sync::view::ViewInfo;

#[derive(Debug, Clone)]
/// Status returned from processing a consensus message.
pub enum ConsensusStatus {
    /// A particular node tried voting twice.
    VotedTwice(NodeId),
    /// A `febft` quorum still hasn't made a decision
    /// on a client request to be executed.
    Deciding,
    /// A `febft` quorum decided on the execution of
    /// the batch of requests with the given digests.
    /// The first digest is the digest of the Prepare message
    /// And therefore the entire batch digest
    /// THe second Vec<Digest> is a vec with digests of the requests contained in the batch
    /// The third is the messages that should be persisted for this batch to be considered persisted
    Decided,
}

#[derive(Debug, Clone)]
/// Represents the status of calling `poll()` on a `Consensus`.
pub enum ConsensusPollStatus<O> {
    /// The `Replica` associated with this `Consensus` should
    /// poll its main channel for more messages.
    Recv,
    /// The `Replica` associated with this `Consensus` should
    /// propose a new client request to be ordered, if it is
    /// the leader, and then it should poll its main channel
    /// for more messages. Alternatively, if the request has
    /// already been decided, it should be queued for
    /// execution.
    TryProposeAndRecv,
    /// A new consensus message is available to be processed.
    NextMessage(Header, ConsensusMessage<O>),
    /// The first consensus instance of the consensus queue is ready to be finalized
    /// as it has already been decided
    Decided,
}

/// Represents a queue of messages to be ordered in a consensus instance.
///
/// Because of the asynchronicity of the Internet, messages may arrive out of
/// context, e.g. for the same consensus instance, a `PRE-PREPARE` reaches
/// a node after a `PREPARE`. A `TboQueue` arranges these messages to be
/// processed in the correct order.
pub struct TboQueue<O> {
    curr_seq: SeqNo,
    watermark: u32,
    get_queue: bool,
    pre_prepares: VecDeque<VecDeque<StoredMessage<ConsensusMessage<O>>>>,
    prepares: VecDeque<VecDeque<StoredMessage<ConsensusMessage<O>>>>,
    commits: VecDeque<VecDeque<StoredMessage<ConsensusMessage<O>>>>,
}

impl<O> Orderable for TboQueue<O> {
    /// Reports the id of the consensus this `TboQueue` is tracking.
    fn sequence_number(&self) -> SeqNo {
        self.curr_seq
    }
}

impl<O> TboQueue<O> {
    fn new(curr_seq: SeqNo, watermark: u32) -> Self {
        Self {
            curr_seq,
            watermark,
            get_queue: false,
            pre_prepares: VecDeque::new(),
            prepares: VecDeque::new(),
            commits: VecDeque::new(),
        }
    }

    fn base_seq(&self) -> SeqNo {
        self.curr_seq + SeqNo::from(self.watermark)
    }

    /// Signal this `TboQueue` that it may be able to extract new
    /// consensus messages from its internal storage.
    pub fn signal(&mut self) {
        self.get_queue = true;
    }

    fn advance_queue(&mut self) -> MessageQueue<O> {
        self.curr_seq = self.curr_seq.next();

        let pre_prepares = tbo_advance_message_queue_return(&mut self.pre_prepares)
            .unwrap_or_else(|| VecDeque::new());
        let prepares = tbo_advance_message_queue_return(&mut self.prepares)
            .unwrap_or_else(|| VecDeque::new());
        let commits = tbo_advance_message_queue_return(&mut self.commits)
            .unwrap_or_else(|| VecDeque::new());

        MessageQueue::from_messages(pre_prepares, prepares, commits)
    }

    /// Queues a consensus message for later processing, or drops it
    /// immediately if it pertains to an older consensus instance.
    pub fn queue(&mut self, h: Header, m: ConsensusMessage<O>) {
        match m.kind() {
            ConsensusMessageKind::PrePrepare(_) => self.queue_pre_prepare(h, m),
            ConsensusMessageKind::Prepare(_) => self.queue_prepare(h, m),
            ConsensusMessageKind::Commit(_) => self.queue_commit(h, m),
        }
    }

    /// Queues a `PRE-PREPARE` message for later processing, or drops it
    /// immediately if it pertains to an older consensus instance.
    fn queue_pre_prepare(&mut self, h: Header, m: ConsensusMessage<O>) {
        tbo_queue_message(
            self.base_seq(),
            &mut self.pre_prepares,
            StoredMessage::new(h, m),
        )
    }

    /// Queues a `PREPARE` message for later processing, or drops it
    /// immediately if it pertains to an older consensus instance.
    fn queue_prepare(&mut self, h: Header, m: ConsensusMessage<O>) {
        tbo_queue_message(self.base_seq(), &mut self.prepares, StoredMessage::new(h, m))
    }

    /// Queues a `COMMIT` message for later processing, or drops it
    /// immediately if it pertains to an older consensus instance.
    fn queue_commit(&mut self, h: Header, m: ConsensusMessage<O>) {
        tbo_queue_message(self.base_seq(), &mut self.commits, StoredMessage::new(h, m))
    }
}

/// The consensus handler. Responsible for multiplexing consensus instances and keeping track
/// of missing messages
pub struct Consensus<D: SharedData + 'static, ST: StateTransferMessage + 'static> {
    node_id: NodeId,
    /// The handle to the executor of the function
    executor_handle: ExecutorHandle<D>,
    /// How many consensus instances can we overlap at the same time.
    watermark: u32,
    /// The current seq no that we are currently in
    seq_no: SeqNo,
    /// The consensus instances that are currently being processed
    /// A given consensus instance n will only be finished when all consensus instances
    /// j, where j < n have already been processed, in order to maintain total ordering
    decisions: VecDeque<ConsensusDecision<D, ST>>,
    /// The queue for messages that sit outside the range seq_no + watermark
    /// These messages cannot currently be processed since they sit outside the allowed
    /// zone but they will be processed once the seq no moves forward enough to include them
    tbo_queue: TboQueue<D::Request>,
}

impl<D, ST> Consensus<D, ST> where D: SharedData + 'static,
                                   ST: StateTransferMessage + 'static {

    pub fn new_replica(node_id: NodeId, view: &ViewInfo, executor_handle: ExecutorHandle<D>, seq_no: SeqNo, watermark: u32) -> Self {

        let mut decision_deque = VecDeque::with_capacity(watermark as usize);

        for i in 0..watermark {
            let seq_add = SeqNo::from(i);

            let seq_no = seq_no + seq_add;

            let decision = ConsensusDecision::init_decision(
                node_id,
                seq_no,
                view
            );

            decision_deque.push_back(decision);
        }

        Self {
            node_id,
            executor_handle,
            watermark,
            seq_no,
            decisions: decision_deque,
            tbo_queue: TboQueue::new(seq_no, watermark),
        }
    }

    pub fn queue(&mut self, header: Header, message: ConsensusMessage<D::Request>) {

        let i = match message.sequence_number().index(self.seq_no) {
            Either::Right(i) => i,
            Either::Left(_) => {
                return;
            }
        };

        if i >= self.decisions.len() {
            // We are not currently processing this consensus instance
            // so we need to queue the message
            self.tbo_queue.queue(header, message);
        } else {
            // Queue the message in the corresponding pending decision
            self.decisions.get_mut(i).unwrap().queue(header, message);
        }
    }

    pub fn poll(&mut self) -> ConsensusPollStatus<D::Request> {
        for ind in 0..self.decisions.len() {
            match self.decisions[ind].poll() {
                ConsensusPollStatus::NextMessage(header, message) => {
                    return ConsensusPollStatus::NextMessage(header, message);
                }
                _ => {}
            }
        }

        ConsensusPollStatus::Recv
    }

    pub fn process_message<NT>(&mut self,
                               header: Header,
                               message: ConsensusMessage<D::Request>,
                               synchronizer: &Synchronizer<D>,
                               timeouts: &Timeouts,
                               log: &mut Log<D>,
                               node: &NT) -> ConsensusStatus
        where NT: Node<PBFT<D, ST>> {
        let i = match message.sequence_number().index(self.seq_no) {
            Either::Right(i) => i,
            Either::Left(_) => {
                // FIXME: maybe notify peers if we detect a message
                // with an invalid (too large) seq no? return the
                // `NodeId` of the offending node.
                //
                // NOTE: alternatively, if this seq no pertains to consensus,
                // we can try running the state transfer protocol
                warn!("Message is behind our current sequence no {:?}", curr_seq, );
                return ConsensusStatus::Deciding;
            }
        };

        if i >= self.decisions.len() {
            // We are not currently processing this consensus instance
            // so we need to queue the message
            self.tbo_queue.queue(header, message);

            return ConsensusStatus::Deciding;
        }

        let decision = self.decisions.get_mut(i).unwrap();

        decision.process_message(header, message, synchronizer, timeouts, log, node)
    }

    /// Finalize the next consensus instance if possible
    pub fn finalize(&mut self, view: &ViewInfo) -> Result<Option<CompletedBatch<D::Request>>> {

        // If the decision can't be finalized, then we can't finalize the batch
        if let Some(decision) = self.decisions.front() {
            if !decision.is_finalizeable() {
                return Ok(None);
            }
        } else {
            return Ok(None);
        }

        // Move to the next instance of the consensus since the current one is going to be finalized
        let decision = self.next_instance(view);

        let batch = decision.finalize()?;

        Ok(Some(batch))
    }

    /// Advance to the next instance of the consensus
    /// This will also create the necessary new decision to keep the pending decisions
    /// equal to the water mark
    pub fn next_instance(&mut self, view: &ViewInfo) -> ConsensusDecision<D, ST> {
        self.seq_no = self.seq_no.next();

        let decision = self.decisions.pop_front().unwrap();

        let queue = self.tbo_queue.advance_queue();

        self.decisions.push_back(ConsensusDecision::init_with_msg_log(self.node_id,
                                                        self.seq_no,
                                                        view,
                                                        queue,));

        decision
    }

    pub fn install_sequence_number(&mut self, seq_no: SeqNo) {
        match self.seq_no.index(seq_no) {
            Either::Left(_) => {
            }
            Either::Right(0) => {
                // We are in the correct sequence number
            }
            Either::Right(limit) => {

            }
        }


    }
}
