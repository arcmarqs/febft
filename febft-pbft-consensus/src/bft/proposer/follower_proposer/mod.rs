use std::marker::PhantomData;
use std::sync::{Arc, atomic::AtomicBool};

use atlas_common::channel;
use atlas_common::channel::{ChannelSyncRx, ChannelSyncTx};
use atlas_communication::protocol_node::ProtocolNetworkNode;
use atlas_core::messages::StoredRequestMessage;
use atlas_core::serialize::{LogTransferMessage, ReconfigurationProtocolMessage, StateTransferMessage};
use atlas_execution::ExecutorHandle;
use atlas_execution::serialize::ApplicationData;

use crate::bft::PBFT;

pub type BatchType<D: ApplicationData> = Vec<StoredRequestMessage<D::Request>>;

///TODO:
pub struct FollowerProposer<D, ST, LP, NT, RP>
    where D: ApplicationData + 'static,
          ST: StateTransferMessage + 'static,
          LP: LogTransferMessage + 'static,
          RP: ReconfigurationProtocolMessage + 'static,
          NT: ProtocolNetworkNode<PBFT<D, ST, LP, RP>> {
    batch_channel: (ChannelSyncTx<BatchType<D>>, ChannelSyncRx<BatchType<D>>),
    //For request execution
    executor_handle: ExecutorHandle<D>,
    cancelled: AtomicBool,

    //Reference to the network node
    node_ref: Arc<NT>,

    //The target
    target_global_batch_size: usize,
    //Time limit for generating a batch with target_global_batch_size size
    global_batch_time_limit: u128,
    _phantom: PhantomData<(ST, LP)>,
}


///The size of the batch channel
const BATCH_CHANNEL_SIZE: usize = 1024;


impl<D, ST, LP, NT, RP> FollowerProposer<D, ST, LP, NT, RP>
    where D: ApplicationData + 'static,
          ST: StateTransferMessage + 'static,
          LP: LogTransferMessage + 'static,
          RP: ReconfigurationProtocolMessage + 'static,
          NT: ProtocolNetworkNode<PBFT<D, ST, LP, RP>> {
    pub fn new(
        node: Arc<NT>,
        executor: ExecutorHandle<D>,
        target_global_batch_size: usize,
        global_batch_time_limit: u128,
    ) -> Arc<Self> {
        todo!();
        let (channel_tx, channel_rx) = channel::new_bounded_sync(BATCH_CHANNEL_SIZE);

        Arc::new(Self {
            batch_channel: (channel_tx, channel_rx),
            executor_handle: executor,
            cancelled: AtomicBool::new(false),
            node_ref: node,
            target_global_batch_size,
            global_batch_time_limit,
            _phantom: Default::default(),
        })
    }
}
