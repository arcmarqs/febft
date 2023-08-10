//! This module is responsible for serializing wire messages in `febft`.
//!
//! All relevant types transmitted over the wire are `serde` aware, if
//! this feature is enabled with `serialize_serde`. Slightly more exotic
//! serialization routines, for better throughput, can be utilized, such
//! as [Cap'n'Proto](https://capnproto.org/capnp-tool.html), but these are
//! expected to be implemented by the user.

use std::io::{Read, Write};
use std::marker::PhantomData;

#[cfg(feature = "serialize_serde")]
use ::serde::{Deserialize, Serialize};
use bytes::Bytes;

use atlas_common::error::*;
use atlas_communication::serialize::Serializable;
use atlas_core::persistent_log::PersistableOrderProtocol;
use atlas_core::reconfiguration_protocol::QuorumJoinCert;
use atlas_core::serialize::{OrderingProtocolMessage, ReconfigurationProtocolMessage, StatefulOrderProtocolMessage};
use atlas_execution::serialize::ApplicationData;

use crate::bft::message::{ConsensusMessage, PBFTMessage};
use crate::bft::msg_log::decisions::{DecisionLog, Proof, ProofMetadata};
use crate::bft::sync::view::ViewInfo;

#[cfg(feature = "serialize_capnp")]
pub mod capnp;

#[cfg(feature = "serialize_serde")]
pub mod serde;

/// The buffer type used to serialize messages into.
pub type Buf = Bytes;

pub fn serialize_consensus<W, D>(w: &mut W, message: &ConsensusMessage<D::Request>) -> Result<()>
    where
        W: Write + AsRef<[u8]> + AsMut<[u8]>,
        D: ApplicationData,
{
    #[cfg(feature = "serialize_capnp")]
    capnp::serialize_consensus::<W, D>(w, message)?;

    #[cfg(feature = "serialize_serde")]
    serde::serialize_consensus::<W, D>(message, w)?;

    Ok(())
}

pub fn deserialize_consensus<R, D>(r: R) -> Result<ConsensusMessage<D::Request>>
    where
        R: Read + AsRef<[u8]>,
        D: ApplicationData,
{
    #[cfg(feature = "serialize_capnp")]
        let result = capnp::deserialize_consensus::<R, D>(r)?;

    #[cfg(feature = "serialize_serde")]
        let result = serde::deserialize_consensus::<R, D>(r)?;

    Ok(result)
}

/// The serializable type, to be used to appease the compiler and it's requirements
pub struct PBFTConsensus<D: ApplicationData>(PhantomData<(D)>);

impl<D> OrderingProtocolMessage for PBFTConsensus<D>
    where D: ApplicationData, {
    type ViewInfo = ViewInfo;
    type ProtocolMessage = PBFTMessage<D::Request>;
    type LoggableMessage = ConsensusMessage<D::Request>;
    type Proof = Proof<D::Request>;
    type ProofMetadata = ProofMetadata;

    #[cfg(feature = "serialize_capnp")]
    fn serialize_capnp(builder: atlas_capnp::consensus_messages_capnp::protocol_message::Builder, msg: &Self::ProtocolMessage) -> Result<()> {
        capnp::serialize_message::<D>(builder, msg)
    }

    #[cfg(feature = "serialize_capnp")]
    fn deserialize_capnp(reader: atlas_capnp::consensus_messages_capnp::protocol_message::Reader) -> Result<Self::ProtocolMessage> {
        capnp::deserialize_message::<D>(reader)
    }

    #[cfg(feature = "serialize_capnp")]
    fn serialize_view_capnp(builder: atlas_capnp::cst_messages_capnp::view_info::Builder, msg: &Self::ViewInfo) -> Result<()> {
        todo!()
    }

    #[cfg(feature = "serialize_capnp")]
    fn deserialize_view_capnp(reader: atlas_capnp::cst_messages_capnp::view_info::Reader) -> Result<Self::ViewInfo> {
        todo!()
    }

    #[cfg(feature = "serialize_capnp")]
    fn serialize_proof_capnp(builder: atlas_capnp::cst_messages_capnp::proof::Builder, msg: &Self::Proof) -> Result<()> {
        todo!()
    }

    #[cfg(feature = "serialize_capnp")]
    fn deserialize_proof_capnp(reader: atlas_capnp::cst_messages_capnp::proof::Reader) -> Result<Self::Proof> {
        todo!()
    }
}

impl<D> StatefulOrderProtocolMessage for PBFTConsensus<D>
    where D: ApplicationData + 'static {
    type DecLog = DecisionLog<D::Request>;

    #[cfg(feature = "serialize_capnp")]
    fn serialize_declog_capnp(builder: atlas_capnp::cst_messages_capnp::dec_log::Builder, msg: &Self::DecLog) -> Result<()> {
        todo!()
    }

    #[cfg(feature = "serialize_capnp")]
    fn deserialize_declog_capnp(reader: atlas_capnp::cst_messages_capnp::dec_log::Reader) -> Result<Self::DecLog> {
        todo!()
    }
}
