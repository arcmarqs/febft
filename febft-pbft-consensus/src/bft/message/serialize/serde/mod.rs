use std::io::{Read, Write};
use atlas_common::error::*;
use crate::bft::message::serialize::ApplicationData;
use crate::bft::message::{ConsensusMessage};
use anyhow::Context;

pub fn serialize_consensus<W, D>(
    m: &ConsensusMessage<D::Request>,
    w: &mut W,
) -> Result<()> where
    W: Write + AsMut<[u8]>,
    D: ApplicationData {

    bincode::serde::encode_into_std_write(m,  w, bincode::config::standard())
        .context(format!("Failed to serialize message {} bytes len", w.as_mut().len()))?;

    Ok(())
}

pub fn deserialize_consensus<R, D>(
    r: R
) -> Result<ConsensusMessage<D::Request>> where D: ApplicationData, R: Read + AsRef<[u8]> {
    let (msg,_size) =  bincode::serde::decode_from_slice(r.as_ref(), bincode::config::standard())
        .context("Failed to deserialize message")?;

    Ok(msg)
}