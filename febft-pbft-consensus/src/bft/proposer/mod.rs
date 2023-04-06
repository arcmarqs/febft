pub mod follower_proposer;

use log::{error, warn, debug, info, trace};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use futures::StreamExt;
use febft_common::ordering::{Orderable, SeqNo};
use febft_common::threadpool;
use febft_communication::message::{NetworkMessage, NetworkMessageKind, StoredMessage};
use febft_communication::Node;
use febft_execution::app::{Request, Service, UnorderedBatch};
use febft_execution::ExecutorHandle;
use febft_messages::messages::{RequestMessage, SystemMessage};
use crate::bft::consensus::ConsensusGuard;
use crate::bft::message::{ConsensusMessage, ConsensusMessageKind, ObserverMessage, PBFTMessage};
use crate::bft::msg_log::pending_decision::PendingRequestLog;
use crate::bft::observer::{ConnState, MessageType, ObserverHandle};
use crate::bft::PBFT;
use crate::bft::sync::view::{is_request_in_hash_space, ViewInfo};
use crate::bft::timeouts::{Timeouts};

use super::sync::{Synchronizer, AbstractSynchronizer};

pub type BatchType<S> = Vec<StoredMessage<RequestMessage<S>>>;

///Handles taking requests from the client pools and storing the requests in the log,
///as well as creating new batches and delivering them to the batch_channel
///Another thread will then take from this channel and propose the requests
pub struct Proposer<S: Service + 'static> {
    node_ref: Arc<Node<PBFT<S::Data>>>,

    synchronizer: Arc<Synchronizer<S>>,
    timeouts: Timeouts,

    /// The log of pending requests
    pending_decision_log: Arc<PendingRequestLog<S>>,

    consensus_guard: ConsensusGuard,
    cancelled: AtomicBool,
    //The target
    target_global_batch_size: usize,
    //Time limit for generating a batch with target_global_batch_size size
    global_batch_time_limit: u128,
    max_batch_size: usize,

    //For unordered request execution
    executor_handle: ExecutorHandle<S>,

    //Observer related stuff
    observer_handle: ObserverHandle,
}

const TIMEOUT: Duration = Duration::from_micros(10);

struct DebugStats {
    collected_per_batch_total: u64,
    collections: u32,
    batches_made: u32,
}

struct ProposeStats<S> where S: Service {
    currently_accumulated: Vec<StoredMessage<RequestMessage<Request<S>>>>,
    last_proposal: Instant,
}

///The size of the batch channel
const BATCH_CHANNEL_SIZE: usize = 128;

impl<S: Service + 'static> Proposer<S> {
    pub fn new(
        node: Arc<Node<PBFT<S::Data>>>,
        sync: Arc<Synchronizer<S>>,
        pending_decision_log: Arc<PendingRequestLog<S>>,
        timeouts: Timeouts,
        executor_handle: ExecutorHandle<S>,
        consensus_guard: ConsensusGuard,
        target_global_batch_size: usize,
        global_batch_time_limit: u128,
        max_batch_size: usize,
        observer_handle: ObserverHandle,
    ) -> Arc<Self> {
        Arc::new(Self {
            node_ref: node,
            synchronizer: sync,
            timeouts,
            cancelled: AtomicBool::new(false),
            consensus_guard,
            target_global_batch_size,
            global_batch_time_limit,
            observer_handle,
            executor_handle,
            pending_decision_log,
            max_batch_size,
        })
    }

    ///Start this work
    pub fn start(self: Arc<Self>) -> JoinHandle<()> {
        std::thread::Builder::new()
            .spawn(move || {

                //DEBUGGING

                let mut debug_stats = DebugStats {
                    collected_per_batch_total: 0,
                    collections: 0,
                    batches_made: 0,
                };

                //END DEBUGGING

                //The currently accumulated requests, accumulated while we wait for the next batch to propose
                let mut ordered_propose = ProposeStats {
                    currently_accumulated: Vec::with_capacity(self.target_global_batch_size),
                    last_proposal: Instant::now(),
                };

                let mut last_seq = None;

                let mut unordered_propose = ProposeStats {
                    currently_accumulated: Vec::with_capacity(self.target_global_batch_size),
                    last_proposal: Instant::now(),
                };

                loop {
                    if self.cancelled.load(Ordering::Relaxed) {
                        break;
                    }

                    //We only want to produce batches to the channel if we are the leader, as
                    //Only the leader will propose thing

                    //TODO: Maybe not use this as it can spam the lock on synchronizer?
                    let info = self.synchronizer.view();

                    let is_leader = info.leader_set().contains(&self.node_ref.id());

                    let our_slice = info.hash_space_division()
                        .get(&self.node_ref.id()).cloned().clone();

                    //We do this as we don't want to get stuck waiting for requests that might never arrive
                    //Or even just waiting for any type of request. We want to minimize the amount of time the
                    //Consensus is waiting for new requests

                    //We don't need to do this for non leader replicas, as that would cause unnecessary strain as the
                    //Thread is in an infinite loop
                    // Receive the requests from the clients and process them
                    let opt_msgs = if is_leader {
                        match self.node_ref.try_recv_from_clients() {
                            Ok(msg) => msg,
                            Err(err) => {
                                error!("{:?} // Failed to receive requests from clients because {:?}", self.node_ref.id(), err);

                                continue;
                            }
                        }
                    } else {
                        match self.node_ref.receive_from_clients(Some(Duration::from_micros(self.global_batch_time_limit as u64))) {
                            Ok(msgs) => {
                                Some(msgs)
                            }
                            Err(err) => {
                                error!("{:?} // Failed to receive requests from clients because {:?}", self.node_ref.id(), err);

                                continue;
                            }
                        }
                    };

                    let discovered_requests = opt_msgs.is_some();
                    
                    if let Some(messages) = opt_msgs {
                        debug_stats.collected_per_batch_total += messages.len() as u64;
                        debug_stats.collections += 1;
                        
                        let mut messages_to_timeout = Vec::with_capacity(messages.len());

                        for message in messages {
                            let NetworkMessage { header, message } = message;

                            let sysmsg = message.into();

                            match sysmsg {
                                SystemMessage::OrderedRequest(req) => {
                                    let rq_digest = header.unique_digest();

                                    /*let key = logg::operation_key(&header, &req);

                                    let current_seq_for_client = lock_guard.get(key)
                                        .copied()
                                        .unwrap_or(SeqNo::ZERO);

                                    if req.sequence_number() < current_seq_for_client {
                                        //Avoid repeating requests for clients
                                        continue;
                                    }*/

                                    if is_leader && is_request_in_hash_space(&rq_digest,
                                                                             our_slice.as_ref().unwrap()) {
                                        ordered_propose.currently_accumulated.push(StoredMessage::new(header, req));
                                    } else {
                                        
                                        messages_to_timeout.push(StoredMessage::new(header, req));
                                        
                                    }
                                }
                                SystemMessage::UnorderedRequest(req) => {
                                    unordered_propose.currently_accumulated.push(StoredMessage::new(header, req));
                                }
                                SystemMessage::ProtocolMessage(protocol) => {
                                    match protocol.into_inner() {
                                        PBFTMessage::ObserverMessage(msg) => {
                                            if let ObserverMessage::ObserverRegister = msg {
                                                //Avoid sending these messages to the main replica
                                                //Processing thread and just process them here instead as it
                                                //Does not delay the process
                                                let observer_message = MessageType::Conn(ConnState::Connected(header.from()));

                                                if let Err(_) = self.observer_handle.tx().send(observer_message) {
                                                    error!("Failed to send messages to the observer handle.");
                                                }
                                            }
                                        }
                                        _ => {
                                            warn!("Rogue message detected from clients")
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }

                        if !messages_to_timeout.is_empty() {
                            let mut digests = Vec::with_capacity(messages_to_timeout.len());
                            
                            messages_to_timeout.iter().for_each(|msg| {
                                digests.push(msg.header().unique_digest())
                            });

                            self.pending_decision_log.insert_batch(messages_to_timeout);
                            
                            //TODO: Handle configurable timeout durations
                            self.timeouts.timeout_client_requests(Duration::from_secs(3), digests);
                        }
                    }

                    //Lets first deal with unordered requests since it should be much quicker and easier
                    self.propose_unordered(&mut unordered_propose);

                    self.propose_ordered(is_leader,
                                         &mut ordered_propose,
                                         &mut last_seq,
                                         &mut debug_stats);

                    if !discovered_requests {
                        //Yield to prevent active waiting

                        std::thread::sleep(Duration::from_millis(5));
                    }
                }
            }).unwrap()
    }


    /// Attempt to propose an unordered request batch
    /// Fails if the batch is not large enough or the timeout
    /// Has not yet occurred
    fn propose_unordered(
        &self,
        propose: &mut ProposeStats<S>,
    ) {
        if !propose.currently_accumulated.is_empty() {
            let current_batch_size = propose.currently_accumulated.len();

            let should_exec = if current_batch_size < self.target_global_batch_size {
                let micros_since_last_batch = Instant::now()
                    .duration_since(propose.last_proposal)
                    .as_micros();

                if micros_since_last_batch <= self.global_batch_time_limit {
                    //Don't execute yet since we don't have the size and haven't timed
                    //out
                    false
                } else {
                    //We have timed out, execute the pending requests
                    true
                }
            } else {
                true
            };

            if should_exec {

                // Swap in the latest time at which a batch was executed
                let last_unordered_batch =
                    std::mem::replace(&mut propose.last_proposal, Instant::now());
                //swap in the new vec and take the previous one to the threadpool
                let new_accumulated_vec =
                    std::mem::replace(&mut propose.currently_accumulated,
                                      Vec::with_capacity(self.target_global_batch_size * 2));

                let executor_handle = self.executor_handle.clone();

                let node_id = self.node_ref.id();

                threadpool::execute(move || {
                    let mut unordered_batch =
                        UnorderedBatch::new_with_cap(new_accumulated_vec.len());

                    for request in new_accumulated_vec {
                        let (header, message) = request.into_inner();

                        unordered_batch.add(
                            header.from(),
                            message.session_id(),
                            message.sequence_number(),
                            message.into_inner_operation(),
                        );
                    }

                    if let Err(err) = executor_handle.queue_update_unordered(unordered_batch) {
                        error!(
                            "Error while proposing unordered batch of requests: {:?}",
                            err
                        );
                    }
                });
            }
        }
    }

    /// attempt to propose the ordered requests that we have collected
    fn propose_ordered(&self,
                       is_leader: bool,
                       propose: &mut ProposeStats<S>,
                       last_seq: &mut Option<SeqNo>,
                       debug: &mut DebugStats) {

        //Now let's deal with ordered requests
        if is_leader {
            let current_batch_size = propose.currently_accumulated.len();

            if current_batch_size < self.target_global_batch_size {
                let micros_since_last_batch = Instant::now().duration_since(propose.last_proposal).as_micros();

                if micros_since_last_batch <= self.global_batch_time_limit {

                    //Batch isn't large enough and time hasn't passed, don't even attempt to propose
                    return;
                }
            }

            //Attempt to propose new batch
            match self.consensus_guard.attempt_to_propose_message() {
                Ok(_) => {
                    propose.last_proposal = Instant::now();

                    debug.batches_made += 1;

                    let guard = self.consensus_guard.consensus_info().lock().unwrap();

                    let (seq, view) = &*guard;

                    match last_seq {
                        None => {}
                        Some(last_exec) => {
                            if *last_exec >= *seq {
                                //We are still in the same consensus instance,
                                //Don't produce more pre prepares
                                return;
                            }
                        }
                    }

                    *last_seq = Some(*seq);

                    let next_batch = if propose.currently_accumulated.len() > self.max_batch_size {

                        //This now contains target_global_size requests. We want this to be our next batch
                        //Currently accumulated contains the remaining messages to be sent in the next batch
                        let mut next_batch = propose.currently_accumulated.split_off(propose.currently_accumulated.len() - self.max_batch_size);

                        //So we swap that memory with the other vector memory and we have it!
                        std::mem::swap(&mut next_batch, &mut propose.currently_accumulated);

                        Some(next_batch)
                    } else {
                        None
                    };

                    let current_batch = std::mem::replace(&mut propose.currently_accumulated,
                                                          next_batch.unwrap_or(Vec::with_capacity(self.node_ref.batch_size() * 2)));


                    self.propose(*seq, view, current_batch);

                    //Stats
                    if debug.batches_made % 10000 == 0 {
                        //let duration = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis();

                        /*println!("{:?} // {:?} // {}: batches made {}: message collections {}: total requests collected.",
                                 self.node_ref.id(), duration, debug.batches_made,
                                 debug.collections, debug.collected_per_batch_total);*/


                        debug.batches_made = 0;
                        debug.collections = 0;
                        debug.collected_per_batch_total = 0;
                    }
                }
                Err(_) => {}
            }
        }
    }

    /// Proposes a new batch.
    /// (Basically broadcasts it to all of the members)
    fn propose(
        &self,
        seq: SeqNo,
        view: &ViewInfo,
        currently_accumulated: Vec<StoredMessage<RequestMessage<Request<S>>>>,
    ) {
        let message = PBFTMessage::Consensus(ConsensusMessage::new(
            seq,
            view.sequence_number(),
            ConsensusMessageKind::PrePrepare(currently_accumulated),
        ));

        let targets = view.quorum_members().iter().copied();

        self.node_ref.broadcast_signed(NetworkMessageKind::from(SystemMessage::from_protocol_message(message)), targets);
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    fn requests_received(
        &self,
        _t: DateTime<Utc>,
        reqs: Vec<StoredMessage<RequestMessage<Request<S>>>>,
    ) {}
}
