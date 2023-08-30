use std::marker::PhantomData;
use std::sync::Arc;

use atlas_communication::message::Header;
use atlas_communication::reconfiguration_node::NetworkInformationProvider;
use atlas_core::state_transfer::networking::serialize::StateTransferMessage;
use atlas_core::state_transfer::networking::signature_ver::StateTransferVerificationHelper;
use atlas_execution::state::monolithic_state::MonolithicState;

use crate::message::CstMessage;

#[cfg(feature = "serialize_capnp")]
mod capnp;

pub struct CSTMsg<S: MonolithicState>(PhantomData<(S)>);

impl<S: MonolithicState> StateTransferMessage for CSTMsg<S> {
    type StateTransferMessage = CstMessage<S>;

    fn verify_state_message<NI, SVH>(network_info: &Arc<NI>, header: &Header, message: Self::StateTransferMessage) -> atlas_common::error::Result<(bool, Self::StateTransferMessage)>
        where NI: NetworkInformationProvider, SVH: StateTransferVerificationHelper {
        todo!()
    }

    #[cfg(feature = "serialize_capnp")]
    fn serialize_capnp(builder: atlas_capnp::cst_messages_capnp::cst_message::Builder, msg: &Self::StateTransferMessage) -> atlas_common::error::Result<()> {
        todo!()
    }

    #[cfg(feature = "serialize_capnp")]
    fn deserialize_capnp(reader: atlas_capnp::cst_messages_capnp::cst_message::Reader) -> atlas_common::error::Result<Self::StateTransferMessage> {
        todo!()
    }
}