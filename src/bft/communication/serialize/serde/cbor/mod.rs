use bytes::{Buf, BufMut};
use serde::{Serialize, Deserialize};

use crate::bft::error::*;
use crate::bft::communication::message::{ReplicaMessage, ClientMessage};

pub fn serialize_to_replica<B: BufMut>(buf: B, m: ReplicaMessage) -> Result<B> {
    let mut w = buf.writer();
    serde_cbor::to_writer(&mut w, &m)
        .map(|()| w.into_inner())
        .wrapped(ErrorKind::CommunicationSerializeSerdeBincode)
}

pub fn deserialize_from_replica<B: Buf>(buf: B) -> Result<ReplicaMessage> {
    let mut r = buf.reader();
    serde_cbor::from_reader(&mut r)
        .wrapped(ErrorKind::CommunicationSerializeSerdeBincode)
}