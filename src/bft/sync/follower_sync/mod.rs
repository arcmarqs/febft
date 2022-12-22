use std::{marker::PhantomData, sync::Arc};

use crate::bft::{executable::{Service, Request}, crypto::hash::Digest, communication::message::{StoredMessage, ConsensusMessage, ConsensusMessageKind}, consensus::log::{Log, persistent::PersistentLogModeTrait}, globals::ReadOnly};

pub struct FollowerSynchronizer<S: Service> {
    _phantom: PhantomData<S>
}

impl<S: Service + 'static> FollowerSynchronizer<S> {

    pub fn new() -> Self {
        Self { _phantom: Default::default() }
    }

    ///Watch a batch of requests received from a Pre prepare message sent by the leader
    /// In reality we won't watch, more like the contrary, since the requests were already
    /// proposed, they won't timeout
    pub fn watch_request_batch<T>(
        &self,
        preprepare: Arc<ReadOnly<StoredMessage<ConsensusMessage<Request<S>>>>>,
        log: &Log<S, T>,
    ) -> Vec<Digest> where T: PersistentLogModeTrait {

        let requests = match preprepare.message().kind() {
            ConsensusMessageKind::PrePrepare(req) => {req},
            _ => {panic!()}
        };

        let mut digests = Vec::with_capacity(requests.len());

        //TODO: Cancel ongoing timeouts of requests that are in the batch

        for x in requests {
            let header = x.header();
            let digest = header.unique_digest();

            digests.push(digest);
        }

        //It's possible that, if the latency of the client to a given replica A is smaller than the
        //Latency to leader replica B + time taken to process request in B + Latency between A and B,
        //This replica does not know of the request and yet it is valid.
        //This means that that client would not be able to process requests from that replica, which could
        //break some of the quorum properties (replica A would always be faulty for that client even if it is
        //not, so we could only tolerate f-1 faults for clients that are in that situation)
        log.insert_batched(preprepare);

        digests
    }

}