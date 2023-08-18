#![feature(inherent_associated_types)]

use std::cmp::Ordering;
use std::fmt::{Debug, Formatter};
use std::sync::Arc;
use std::time::{Duration, Instant};

use log::{debug, error, info, warn};
#[cfg(feature = "serialize_serde")]
use serde::{Deserialize, Serialize};
use atlas_common::channel::ChannelSyncTx;

use atlas_common::collections;
use atlas_common::collections::HashMap;
use atlas_common::crypto::hash::Digest;
use atlas_common::error::*;
use atlas_common::globals::ReadOnly;
use atlas_common::node_id::NodeId;
use atlas_common::ordering::{Orderable, SeqNo};
use atlas_communication::message::{Header, NetworkMessageKind, StoredMessage};
use atlas_communication::protocol_node::ProtocolNetworkNode;
use atlas_execution::app::{Application, Reply, Request};
use atlas_execution::ExecutorHandle;
use atlas_execution::serialize::ApplicationData;
use atlas_core::messages::{StateTransfer, SystemMessage};
use atlas_core::ordering_protocol::{ExecutionResult, OrderingProtocol, SerProof, View};
use atlas_core::persistent_log::{MonolithicStateLog, PersistableStateTransferProtocol, OperationMode};
use atlas_core::serialize::{LogTransferMessage, NetworkView, OrderingProtocolMessage, ServiceMsg, StatefulOrderProtocolMessage, StateTransferMessage};
use atlas_core::state_transfer::{Checkpoint, CstM, StateTransferProtocol, STResult, STTimeoutResult};
use atlas_core::state_transfer::monolithic_state::MonolithicStateTransfer;
use atlas_core::state_transfer::networking::StateTransferSendNode;
use atlas_core::timeouts::{RqTimeout, TimeoutKind, Timeouts};
use atlas_execution::state::monolithic_state::{InstallStateMessage, MonolithicState};
use atlas_metrics::metrics::metric_duration;

use crate::config::StateTransferConfig;
use crate::message::{CstMessage, CstMessageKind};
use crate::message::serialize::CSTMsg;
use crate::metrics::STATE_TRANSFER_STATE_INSTALL_CLONE_TIME_ID;

pub mod message;
pub mod config;
pub mod metrics;

/// The state of the checkpoint
pub enum CheckpointState<D> {
    // no checkpoint has been performed yet
    None,
    // we are calling this a partial checkpoint because we are
    // waiting for the application state from the execution layer
    Partial {
        // sequence number of the last executed request
        seq: SeqNo,
    },
    PartialWithEarlier {
        // sequence number of the last executed request
        seq: SeqNo,
        // save the earlier checkpoint, in case corruption takes place
        earlier: Arc<ReadOnly<Checkpoint<D>>>,
    },
    // application state received, the checkpoint state is finalized
    Complete(Arc<ReadOnly<Checkpoint<D>>>),
}

enum ProtoPhase<S> {
    Init,
    WaitingCheckpoint(Vec<StoredMessage<CstMessage<S>>>),
    ReceivingCid(usize),
    ReceivingState(usize),
}

impl<S> Debug for ProtoPhase<S> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            ProtoPhase::Init => {
                write!(f, "Init Phase")
            }
            ProtoPhase::WaitingCheckpoint(header) => {
                write!(f, "Waiting for checkpoint {}", header.len())
            }
            ProtoPhase::ReceivingCid(size) => {
                write!(f, "Receiving CID phase {} responses", size)
            }
            ProtoPhase::ReceivingState(size) => {
                write!(f, "Receiving state phase {} responses", size)
            }
        }
    }
}

/// Contains state used by a recovering node.
///
/// Cloning this is better than it was because of the read only checkpoint,
/// and because decision log also got a lot easier to clone, but we still have
/// to be very careful thanks to the requests vector, which can be VERY large
#[cfg_attr(feature = "serialize_serde", derive(Serialize, Deserialize))]
#[derive(Clone, Debug)]
pub struct RecoveryState<S> {
    pub checkpoint: Arc<ReadOnly<Checkpoint<S>>>,
}

impl<S> RecoveryState<S> {
    /// Creates a new `RecoveryState`.
    pub fn new(
        checkpoint: Arc<ReadOnly<Checkpoint<S>>>,
    ) -> Self {
        Self {
            checkpoint,
        }
    }

    /// Returns the local checkpoint of this recovery state.
    pub fn checkpoint(&self) -> &Arc<ReadOnly<Checkpoint<S>>> {
        &self.checkpoint
    }
}

#[derive(Debug)]
struct ReceivedState<S> {
    count: usize,
    state: RecoveryState<S>,
}

#[derive(Debug)]
struct ReceivedStateCid {
    cid: SeqNo,
    count: usize,
}

// NOTE: in this module, we may use cid interchangeably with
// consensus sequence number
/// The collaborative state transfer algorithm.
///
/// The implementation is based on the paper «On the Efﬁciency of
/// Durable State Machine Replication», by A. Bessani et al.
pub struct CollabStateTransfer<S, NT, PL>
    where S: MonolithicState + 'static {
    curr_seq: SeqNo,
    current_checkpoint_state: CheckpointState<S>,
    base_timeout: Duration,
    curr_timeout: Duration,
    timeouts: Timeouts,
    // NOTE: remembers whose replies we have
    // received already, to avoid replays
    //voted: HashSet<NodeId>,
    node: Arc<NT>,
    received_states: HashMap<Digest, ReceivedState<S>>,
    received_state_ids: HashMap<Digest, ReceivedStateCid>,
    phase: ProtoPhase<S>,

    install_channel: ChannelSyncTx<InstallStateMessage<S>>,

    /// Persistent logging for the state transfer protocol.
    persistent_log: PL,
}

/// Status returned from processing a state transfer message.
pub enum CstStatus<S> {
    /// We are not running the CST protocol.
    ///
    /// Drop any attempt of processing a message in this condition.
    Nil,
    /// The CST protocol is currently running.
    Running,
    /// We should request the latest cid from the view.
    RequestStateCid,
    /// We have received and validated the largest state sequence
    /// number available.
    SeqNo(SeqNo),
    /// We should request the latest state from the view.
    RequestState,
    /// We have received and validated the state from
    /// a group of replicas.
    State(RecoveryState<S>),
}

impl<S> Debug for CstStatus<S> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            CstStatus::Nil => {
                write!(f, "Nil")
            }
            CstStatus::Running => {
                write!(f, "Running")
            }
            CstStatus::RequestStateCid => {
                write!(f, "Request latest CID")
            }
            CstStatus::RequestState => {
                write!(f, "Request latest state")
            }
            CstStatus::SeqNo(seq) => {
                write!(f, "Received seq no {:?}", seq)
            }
            CstStatus::State(_) => {
                write!(f, "Received state")
            }
        }
    }
}

/// Represents progress in the CST state machine.
///
/// To clarify, the mention of state machine here has nothing to do with the
/// SMR protocol, but rather the implementation in code of the CST protocol.
pub enum CstProgress<S> {
    // TODO: Timeout( some type here)
    /// This value represents null progress in the CST code's state machine.
    Nil,
    /// We have a fresh new message to feed the CST state machine, from
    /// the communication layer.
    Message(Header, CstMessage<S>),
}

macro_rules! getmessage {
    ($progress:expr, $status:expr) => {
        match $progress {
            CstProgress::Nil => return $status,
            CstProgress::Message(h, m) => (h, m),
        }
    };
    // message queued while waiting for exec layer to deliver app state
    ($phase:expr) => {{
        let phase = std::mem::replace($phase, ProtoPhase::Init);
        match phase {
            ProtoPhase::WaitingCheckpoint(h, m) => (h, m),
            _ => return CstStatus::Nil,
        }
    }};
}

impl<S, NT, PL> StateTransferProtocol<S, NT, PL> for CollabStateTransfer<S, NT, PL>
    where S: MonolithicState + 'static,
          PL: MonolithicStateLog<S> + 'static,
          NT: StateTransferSendNode<CSTMsg<S>> + 'static
{
    type Serialization = CSTMsg<S>;

    fn request_latest_state<V>(&mut self, view: V) -> Result<()>
        where V: NetworkView {
        self.request_latest_consensus_seq_no::<V>(view);

        Ok(())
    }

    fn handle_off_ctx_message<V>(&mut self, view: V, message: StoredMessage<StateTransfer<CstM<Self::Serialization>>>)
                                 -> Result<()>
        where V: NetworkView {
        let (header, message) = message.into_inner();

        let message = message.into_inner();

        debug!("{:?} // Off context Message {:?} from {:?} with seq {:?}", self.node.id(), message, header.from(), message.sequence_number());

        match &message.kind() {
            CstMessageKind::RequestStateCid => {
                self.process_request_seq(header, message);

                return Ok(());
            }
            CstMessageKind::RequestState => {
                self.process_request_state(header, message);

                return Ok(());
            }
            _ => {}
        }

        let status = self.process_message(
            view,
            CstProgress::Message(header, message),
        );

        match status {
            CstStatus::Nil => (),
// should not happen...
            _ => {
                return Err(format!("Invalid state reached while state transfer processing message! {:?}", status)).wrapped(ErrorKind::CoreServer);
            }
        }

        Ok(())
    }

    fn process_message<V>(&mut self, view: V,
                          message: StoredMessage<StateTransfer<CstM<Self::Serialization>>>)
                          -> Result<STResult> where V: NetworkView {
        let (header, message) = message.into_inner();

        let message = message.into_inner();

        debug!("{:?} // Message {:?} from {:?} while in phase {:?}", self.node.id(), message, header.from(), self.phase);

        match &message.kind() {
            CstMessageKind::RequestStateCid => {
                self.process_request_seq(header, message);

                return Ok(STResult::StateTransferRunning);
            }
            CstMessageKind::RequestState => {
                self.process_request_state(header, message);

                return Ok(STResult::StateTransferRunning);
            }
            _ => {}
        }

        // Notify timeouts that we have received this message
        self.timeouts.received_cst_request(header.from(), message.sequence_number());

        let status = self.process_message(view.clone(),
                                          CstProgress::Message(header, message), );

        match status {
            CstStatus::Running => (),
            CstStatus::State(state) => {
                let start = Instant::now();

                self.install_channel.send(InstallStateMessage::new(state.checkpoint.state().clone())).unwrap();

                metric_duration(STATE_TRANSFER_STATE_INSTALL_CLONE_TIME_ID, start.elapsed());

                return Ok(STResult::StateTransferFinished(state.checkpoint.sequence_number()));
            }
            CstStatus::SeqNo(seq) => {
                if self.current_checkpoint_state.sequence_number() < seq {
                    debug!("{:?} // Requesting state {:?}", self.node.id(), seq);

                    self.request_latest_state(view);
                } else {
                    debug!("{:?} // Not installing sequence number nor requesting state ???? {:?} {:?}", self.node.id(), self.current_checkpoint_state.sequence_number(), seq);
                    return Ok(STResult::StateTransferNotNeeded(seq));
                }
            }
            CstStatus::RequestStateCid => {
                self.request_latest_consensus_seq_no(view);
            }
            CstStatus::RequestState => {
                self.request_latest_state(view);
            }
            CstStatus::Nil => {
                // No actions are required for the CST
                // This can happen for example when we already received the a quorum of sequence number replies
                // And therefore we are already in the Init phase and we are still receiving replies
                // And have not yet processed the
            }
        }

        Ok(STResult::StateTransferRunning)
    }

    fn handle_app_state_requested<V>(&mut self, view: V, seq: SeqNo) -> Result<ExecutionResult>
        where V: NetworkView {
        let earlier = std::mem::replace(&mut self.current_checkpoint_state, CheckpointState::None);

        self.current_checkpoint_state = match earlier {
            CheckpointState::None => CheckpointState::Partial { seq },
            CheckpointState::Complete(earlier) => {
                CheckpointState::PartialWithEarlier { seq, earlier }
            }
// FIXME: this may not be an invalid state after all; we may just be generating
// checkpoints too fast for the execution layer to keep up, delivering the
// hash digests of the appstate
            _ => {
                error!("Invalid checkpoint state detected");

                self.current_checkpoint_state = earlier;

                return Ok(ExecutionResult::Nil);
            }
        };

        Ok(ExecutionResult::BeginCheckpoint)
    }

    fn handle_timeout<V>(&mut self, view: V, timeout: Vec<RqTimeout>) -> Result<STTimeoutResult>
        where V: NetworkView {
        for cst_seq in timeout {
            if let TimeoutKind::Cst(cst_seq) = cst_seq.timeout_kind() {
                if self.cst_request_timed_out(cst_seq.clone(), view.clone()) {
                    return Ok(STTimeoutResult::RunCst);
                }
            }
        }

        Ok(STTimeoutResult::CstNotNeeded)
    }
}

impl<S, NT, PL> MonolithicStateTransfer<S, NT, PL> for CollabStateTransfer<S, NT, PL>
    where S: MonolithicState + 'static,
          PL: MonolithicStateLog<S> + 'static,
          NT: StateTransferSendNode<CSTMsg<S>> + 'static {
    type Config = StateTransferConfig;

    fn initialize(config: Self::Config, timeouts: Timeouts, node: Arc<NT>,
                  log: PL, executor_handle: ChannelSyncTx<InstallStateMessage<S>>) -> Result<Self>
        where Self: Sized {
        let StateTransferConfig {
            timeout_duration
        } = config;


        Ok(Self::new(node, timeout_duration, timeouts, log, executor_handle))
    }

    fn handle_state_received_from_app<V>(&mut self, view: V, state: Arc<ReadOnly<Checkpoint<S>>>) -> Result<()>
        where V: NetworkView {
        self.finalize_checkpoint(state)?;

        if self.needs_checkpoint() {
            // This will make the state transfer protocol aware of the latest state
            if let CstStatus::Nil = self.process_message(view, CstProgress::Nil) {} else {
                return Err("Process message while needing checkpoint returned something else than nil")
                    .wrapped(ErrorKind::Cst);
            }
        }

        Ok(())
    }
}

type Ser<ST: StateTransferProtocol<S, NT, PL>, S, NT, PL> = <ST as StateTransferProtocol<S, NT, PL>>::Serialization;

// TODO: request timeouts
impl<S, NT, PL> CollabStateTransfer<S, NT, PL>
    where
        S: MonolithicState + 'static,
        PL: MonolithicStateLog<S> + 'static,
        NT: StateTransferSendNode<CSTMsg<S>> + 'static
{
    /// Create a new instance of `CollabStateTransfer`.
    pub fn new(node: Arc<NT>, base_timeout: Duration, timeouts: Timeouts, persistent_log: PL, install_channel: ChannelSyncTx<InstallStateMessage<S>>) -> Self {
        Self {
            current_checkpoint_state: CheckpointState::None,
            base_timeout,
            curr_timeout: base_timeout,
            timeouts,
            node,
            received_states: collections::hash_map(),
            received_state_ids: collections::hash_map(),
            phase: ProtoPhase::Init,
            curr_seq: SeqNo::ZERO,
            persistent_log,
            install_channel,
        }
    }

    /// Checks if the CST layer is waiting for a local checkpoint to
    /// complete.
    ///
    /// This is used when a node is sending state to a peer.
    pub fn needs_checkpoint(&self) -> bool {
        matches!(self.phase, ProtoPhase::WaitingCheckpoint(_))
    }

    fn process_request_seq<>(
        &mut self,
        header: Header,
        message: CstMessage<S>)
        where {
        let seq = match &self.current_checkpoint_state {
            CheckpointState::PartialWithEarlier { seq, earlier, } => {
                Some((earlier.sequence_number(), earlier.digest().clone()))
            }
            CheckpointState::Complete(seq) => {
                Some((seq.sequence_number(), seq.digest().clone()))
            }
            _ => {
                None
            }
        };

        let kind = CstMessageKind::ReplyStateCid(seq.clone());

        let reply = CstMessage::new(message.sequence_number(), kind);

        debug!("{:?} // Replying to {:?} seq {:?} with seq no {:?}", self.node.id(),
            header.from(), message.sequence_number(), seq);

        self.node.send(reply, header.from(), true);
    }


    /// Process the entire list of pending state transfer requests
    /// This will only reply to the latest request sent by each of the replicas
    fn process_pending_state_requests(&mut self)
        where {
        let waiting = std::mem::replace(&mut self.phase, ProtoPhase::Init);

        if let ProtoPhase::WaitingCheckpoint(reqs) = waiting {
            let mut map: HashMap<NodeId, StoredMessage<CstMessage<S>>> = collections::hash_map();

            for request in reqs {
                // We only want to reply to the most recent requests from each of the nodes
                if map.contains_key(&request.header().from()) {
                    map.entry(request.header().from()).and_modify(|x| {
                        if x.message().sequence_number() < request.message().sequence_number() {
                            //Dispose of the previous request
                            let _ = std::mem::replace(x, request);
                        }
                    });

                    continue;
                } else {
                    map.insert(request.header().from(), request);
                }
            }

            map.into_values().for_each(|req| {
                let (header, message) = req.into_inner();

                self.process_request_state(header, message);
            });
        }
    }

    fn process_request_state(
        &mut self,
        header: Header,
        message: CstMessage<S>,
    ) where
    {
        match &mut self.phase {
            ProtoPhase::Init => {}
            ProtoPhase::WaitingCheckpoint(waiting) => {
                waiting.push(StoredMessage::new(header, message));

                return;
            }
            _ => {
                // We can't reply to state requests when requesting state ourselves
                return;
            }
        }

        let state = match &self.current_checkpoint_state {
            CheckpointState::PartialWithEarlier { earlier, seq } => { earlier.clone() }
            CheckpointState::Complete(checkpoint) => {
                checkpoint.clone()
            }
            _ => {
                if let ProtoPhase::WaitingCheckpoint(waiting) = &mut self.phase {
                    waiting.push(StoredMessage::new(header, message));
                } else {
                    self.phase = ProtoPhase::WaitingCheckpoint(vec![StoredMessage::new(header, message)]);
                }

                return;
            }
        };

        let reply = CstMessage::new(
            message.sequence_number(),
            CstMessageKind::ReplyState(RecoveryState {
                checkpoint: state,
            }),
        );

        self.node.send(reply, header.from(), true).unwrap();
    }

    /// Advances the state of the CST state machine.
    pub fn process_message<V>(
        &mut self,
        view: V,
        progress: CstProgress<S>,
    ) -> CstStatus<S>
        where V: NetworkView
    {
        match self.phase {
            ProtoPhase::WaitingCheckpoint(_) => {
                self.process_pending_state_requests();

                CstStatus::Nil
            }
            ProtoPhase::Init => {
                let (header, message) = getmessage!(progress, CstStatus::Nil);

                match message.kind() {
                    CstMessageKind::RequestStateCid => {
                        self.process_request_seq(header, message);
                    }
                    CstMessageKind::RequestState => {
                        self.process_request_state(header, message);
                    }
                    // we are not running cst, so drop any reply msgs
                    //
                    // TODO: maybe inspect cid msgs, and passively start
                    // the state transfer protocol, by returning
                    // CstStatus::RequestState
                    _ => (),
                }

                CstStatus::Nil
            }
            ProtoPhase::ReceivingCid(i) => {
                let (header, message) = getmessage!(progress, CstStatus::RequestStateCid);

                debug!("{:?} // Received Cid with {} responses from {:?} for CST Seq {:?} vs Ours {:?}", self.node.id(),
                   i, header.from(), message.sequence_number(), self.curr_seq);

                // drop cst messages with invalid seq no
                if message.sequence_number() != self.curr_seq {
                    debug!("{:?} // Wait what? {:?} {:?}", self.node.id(), self.curr_seq, message.sequence_number());
                    // FIXME: how to handle old or newer messages?
                    // BFT-SMaRt simply ignores messages with a
                    // value of `queryID` different from the current
                    // `queryID` a replica is tracking...
                    // we will do the same for now
                    //
                    // TODO: implement timeouts to fix cases like this
                    return CstStatus::Running;
                }

                match message.kind() {
                    CstMessageKind::ReplyStateCid(state_cid) => {
                        if let Some((cid, digest)) = state_cid {
                            debug!("{:?} // Received state cid {:?} with digest {:?} from {:?} with seq {:?}",
                            self.node.id(), state_cid, digest, header.from(), cid);

                            let received_state_cid = self.received_state_ids.entry(digest.clone()).or_insert_with(|| {
                                ReceivedStateCid {
                                    cid: *cid,
                                    count: 0,
                                }
                            });

                            if *cid > received_state_cid.cid {
                                info!("{:?} // Received newer state for old cid {:?} vs new cid {:?} with digest {:?}.",
                                    self.node.id(), received_state_cid.cid, *cid, digest);

                                received_state_cid.cid = *cid;
                                received_state_cid.count = 1;
                            } else if *cid == received_state_cid.cid {
                                info!("{:?} // Received matching state for cid {:?} with digest {:?}. Count {}",
                                self.node.id(), received_state_cid.cid, digest, received_state_cid.count + 1);

                                received_state_cid.count += 1;
                            }
                        } else {


                            debug!("{:?} // Received blank state cid from node {:?}", self.node.id(), header.from());
                        }
                    }
                    CstMessageKind::RequestStateCid => {
                        self.process_request_seq(header, message);

                        return CstStatus::Running;
                    }
                    CstMessageKind::RequestState => {
                        self.process_request_state(header, message);

                        return CstStatus::Running;
                    }
                    // drop invalid message kinds
                    _ => return CstStatus::Running,
                }

                // check if we have gathered enough cid
                // replies from peer nodes
                //
                // TODO: check for more than one reply from the same node
                let i = i + 1;

                debug!("{:?} // Quorum count {}, i: {}, cst_seq {:?}. Current Latest Cid: {:?}",
                        self.node.id(), view.quorum(), i,
                        self.curr_seq, self.received_state_ids);

                if i >= view.quorum() {
                    self.phase = ProtoPhase::Init;

                    // reset timeout, since req was successful
                    self.curr_timeout = self.base_timeout;

                    let mut received_state_ids: Vec<_> = self.received_state_ids.iter().map(|(digest, cid)| {
                        (digest, cid.cid, cid.count)
                    }).collect();

                    received_state_ids.sort_by(|(_, _, count), (_, _, count2)| {
                        count.cmp(count2).reverse()
                    });

                    if let Some((digest, seq, count)) = received_state_ids.first() {
                        if *count >= view.quorum() {
                            info!("{:?} // Received quorum of states for CST Seq {:?} with digest {:?} and seq {:?}",
                                self.node.id(), self.curr_seq, digest, seq);

                            return CstStatus::SeqNo(*seq);
                        } else {
                            warn!("Received quorum state messages but we still don't have a quorum of states? Faulty replica? {:?}", self.received_state_ids)
                        }
                    } else {
                        // If we are completely blank, then no replicas have state, so we can initialize

                        warn!("We have received a quorum of blank messages, which means we are probably at the start");
                        return CstStatus::SeqNo(SeqNo::ZERO);
                    }

                    // we don't need the latest cid to be available in at least
                    // f+1 replicas since the replica has the proof that the system
                    // has decided
                }

                self.phase = ProtoPhase::ReceivingCid(i);

                CstStatus::Running
            }
            ProtoPhase::ReceivingState(i) => {
                let (header, mut message) = getmessage!(progress, CstStatus::RequestState);

                if message.sequence_number() != self.curr_seq {
                    // NOTE: check comment above, on ProtoPhase::ReceivingCid
                    return CstStatus::Running;
                }

                let state = match message.take_state() {
                    Some(state) => state,
                    // drop invalid message kinds
                    None => return CstStatus::Running,
                };

                let state_digest = state.checkpoint.digest().clone();

                debug!("{:?} // Received state with digest {:?}, is contained in map? {}", self.node.id(),
                state_digest, self.received_states.contains_key(&state_digest));

                if self.received_states.contains_key(&state_digest) {
                    let current_state = self.received_states.get_mut(&state_digest).unwrap();

                    let current_state_seq: SeqNo = current_state.state.checkpoint().sequence_number();
                    let recv_state_seq: SeqNo = state.checkpoint().sequence_number();

                    match recv_state_seq.cmp(&current_state_seq) {
                        Ordering::Less | Ordering::Equal => {
                            // we have just verified that the state is the same, but the decision log is
                            // smaller than the one we have already received
                            current_state.count += 1;
                        }
                        Ordering::Greater => {
                            current_state.state = state;
                            // We have also verified that the state is the same but the decision log is
                            // Larger, so we want to store the newest one. However we still want to increment the count
                            // We can do this since to be in the decision log, a replica must have all of the messages
                            // From at least 2f+1 replicas, so we know that the log is valid
                            current_state.count += 1;
                        }
                    }
                } else {
                    self.received_states.insert(state_digest, ReceivedState { count: 1, state });
                }

                // check if we have gathered enough state
                // replies from peer nodes
                //
                // TODO: check for more than one reply from the same node
                let i = i + 1;

                if i <= view.f() {
                    self.phase = ProtoPhase::ReceivingState(i);
                    return CstStatus::Running;
                }

                // NOTE: clear saved states when we return;
                // this is important, because each state
                // may be several GBs in size

                // check if we have at least f+1 matching states
                let digest = {
                    let received_state = self.received_states.iter().max_by_key(|(_, st)| st.count);

                    match received_state {
                        Some((digest, _)) => digest.clone(),
                        None => {
                            return if i >= view.quorum() {
                                self.received_states.clear();

                                debug!("{:?} // No matching states found, clearing", self.node.id());
                                CstStatus::RequestState
                            } else {
                                CstStatus::Running
                            };
                        }
                    }
                };

                let received_state = {
                    let received_state = self.received_states.remove(&digest);
                    self.received_states.clear();
                    received_state
                };

                // reset timeout, since req was successful
                self.curr_timeout = self.base_timeout;

                // return the state
                let f = view.f();

                match received_state {
                    Some(ReceivedState { count, state }) if count > f => {
                        self.phase = ProtoPhase::Init;

                        info!("{:?} // Received quorum of states for CST Seq {:?} with digest {:?}, returning the state to the replica",
                            self.node.id(), self.curr_seq, digest);

                        CstStatus::State(state)
                    }
                    _ => {
                        debug!("{:?} // No states with more than f {} count", self.node.id(), f);

                        CstStatus::RequestState
                    }
                }
            }
        }
    }


    /// End the state of an on-going checkpoint.
    ///
    /// This method should only be called when `finalize_request()` reports
    /// `Info::BeginCheckpoint`, and the requested application state is received
    /// on the core server task's master channel.
    pub fn finalize_checkpoint(&mut self, checkpoint: Arc<ReadOnly<Checkpoint<S>>>) -> Result<()> where
        PL: MonolithicStateLog<S> {
        match &self.current_checkpoint_state {
            CheckpointState::None => {
                Err("No checkpoint has been initiated yet").wrapped(ErrorKind::MsgLog)
            }
            CheckpointState::Complete(_) => {
                Err("Checkpoint already finalized").wrapped(ErrorKind::MsgLog)
            }
            CheckpointState::Partial { seq: _ } | CheckpointState::PartialWithEarlier { seq: _, .. } => {
                let checkpoint_state = CheckpointState::Complete(checkpoint.clone());

                self.current_checkpoint_state = checkpoint_state;

                self.persistent_log.write_checkpoint(OperationMode::NonBlockingSync(None), checkpoint)?;

                Ok(())
            }
        }
    }

    fn curr_seq(&mut self) -> SeqNo {
        self.curr_seq
    }

    fn next_seq(&mut self) -> SeqNo {
        self.curr_seq = self.curr_seq.next();

        self.curr_seq
    }

    /// Handle a timeout received from the timeouts layer.
    /// Returns a bool to signify if we must move to the Retrieving state
    /// If the timeout is no longer relevant, returns false (Can remain in current phase)
    pub fn cst_request_timed_out<V>(&mut self, seq: SeqNo, view: V) -> bool
        where V: NetworkView {
        let status = self.timed_out(seq);

        match status {
            CstStatus::RequestStateCid => {
                self.request_latest_consensus_seq_no(view);

                true
            }
            CstStatus::RequestState => {
                self.request_latest_state(view);

                true
            }
            // nothing to do
            _ => false,
        }
    }

    fn timed_out(&mut self, seq: SeqNo) -> CstStatus<S> {
        if seq != self.curr_seq {
            // the timeout we received is for a request
            // that has already completed, therefore we ignore it
            //
            // TODO: this check is probably not necessary,
            // as we have likely already updated the `ProtoPhase`
            // to reflect the fact we are no longer receiving state
            // from peer nodes
            return CstStatus::Nil;
        }

        self.next_seq();

        match self.phase {
            // retry requests if receiving state and we have timed out
            ProtoPhase::ReceivingCid(_) => {
                self.curr_timeout *= 2;
                CstStatus::RequestStateCid
            }
            ProtoPhase::ReceivingState(_) => {
                self.curr_timeout *= 2;
                CstStatus::RequestState
            }
            // ignore timeouts if not receiving any kind
            // of state from peer nodes
            _ => CstStatus::Nil,
        }
    }

    /// Used by a recovering node to retrieve the latest sequence number
    /// attributed to a client request by the consensus layer.
    pub fn request_latest_consensus_seq_no<V>(
        &mut self,
        view: V,
    ) where V: NetworkView
    {
        // Reset the map of received state ids
        self.received_state_ids.clear();

        self.next_seq();

        let cst_seq = self.curr_seq();

        info!("{:?} // Requesting latest state seq no with seq {:?}", self.node.id(), cst_seq);

        self.timeouts.timeout_cst_request(self.curr_timeout,
                                          view.quorum() as u32,
                                          cst_seq);

        self.phase = ProtoPhase::ReceivingCid(0);

        let message = CstMessage::new(
            cst_seq,
            CstMessageKind::RequestStateCid,
        );

        let targets = view.quorum_members().clone().into_iter().filter(|id| *id != self.node.id());

        self.node.broadcast(message, targets);
    }

    /// Used by a recovering node to retrieve the latest state.
    pub fn request_latest_state<V>(
        &mut self, view: V,
    ) where V: NetworkView
    {
        // reset hashmap of received states
        self.received_states.clear();

        self.next_seq();

        let cst_seq = self.curr_seq();

        info!("{:?} // Requesting latest state with cst msg seq {:?}", self.node.id(), cst_seq);

        self.timeouts.timeout_cst_request(self.curr_timeout,
                                          view.quorum() as u32,
                                          cst_seq);

        self.phase = ProtoPhase::ReceivingState(0);

        //TODO: Maybe attempt to use followers to rebuild state and avoid
        // Overloading the replicas
        let message = CstMessage::new(cst_seq, CstMessageKind::RequestState);
        let targets = view.quorum_members().clone().into_iter().filter(|id| *id != self.node.id());

        self.node.broadcast(message, targets);
    }
}

impl<S, NT, PL> PersistableStateTransferProtocol for CollabStateTransfer<S, NT, PL>
    where S: MonolithicState + 'static {}


impl<S> Orderable for CheckpointState<S> {
    fn sequence_number(&self) -> SeqNo {
        match self {
            CheckpointState::None => {
                SeqNo::ZERO
            }
            CheckpointState::Partial { seq } => {
                SeqNo::ZERO
            }
            CheckpointState::PartialWithEarlier { earlier, .. } => {
                earlier.sequence_number()
            }
            CheckpointState::Complete(arc) => {
                arc.sequence_number()
            }
        }
    }
}