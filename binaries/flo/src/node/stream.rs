use s2_grpc_utils::{S2ProtoEnum, S2ProtoUnpack};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::mpsc::{channel, Receiver, Sender};
use tracing_futures::Instrument;

use flo_event::*;
use flo_net::packet::*;
use flo_net::proto::flo_node as proto;
use flo_net::stream::FloStream;
use flo_net::w3gs::{frame_to_w3gs, w3gs_to_frame};
use flo_task::{SpawnScope, SpawnScopeHandle};
use flo_types::node::NodeGameStatusSnapshot;
use flo_w3gs::packet::Packet as W3GSPacket;

use crate::error::*;
use crate::lan::LanEvent;
use crate::types::GameStatusUpdate;
use crate::types::SlotClientStatus;
use flo_w3gs::protocol::packet::Packet;

#[derive(Debug)]
pub struct NodeStream {
  scope: SpawnScope,
  state: Arc<State>,
}

impl NodeStream {
  pub async fn connect(
    addr: SocketAddr,
    token: NodeConnectToken,
    event_sender: NodeStreamEventSender,
    w3gs_sender: Sender<W3GSPacket>,
  ) -> Result<Self> {
    let scope = SpawnScope::new();
    let mut stream = FloStream::connect(addr).await?;

    stream
      .send(proto::PacketClientConnect {
        version: Some(crate::version::FLO_VERSION.into()),
        token: token.to_vec(),
      })
      .await?;

    let frame = stream.recv_frame().await?;

    let (player_id, initial_status) = flo_net::try_flo_packet! {
      frame => {
        p: proto::PacketClientConnectAccept => {
          let game_id = p.game_id;
          let player_id = p.player_id;
          tracing::debug!(
            game_id,
            player_id = player_id,
            "node connected: version = {:?}, game_status = {:?}",
            p.version,
            p.game_status,
          );
          let status = NodeGameStatusSnapshot::unpack(p)?;
          (player_id, status)
        }
        p: proto::PacketClientConnectReject => {
          return Err(Error::NodeConnectionRejected(p.reason(), p.message))
        }
      }
    };

    let (outgoing_sender, outgoing_receiver) = channel(10);

    let state = Arc::new(State {
      game_id: initial_status.game_id,
      player_id,
      outgoing_sender,
      event_sender,
      token: token.to_vec(),
    });

    tokio::spawn({
      Self::worker(
        state.clone(),
        stream,
        w3gs_sender,
        outgoing_receiver,
        initial_status,
        scope.handle(),
      )
      .instrument(tracing::debug_span!(
        "worker",
        game_id = state.game_id,
        player_id
      ))
    });

    Ok(Self { scope, state })
  }

  pub fn handle(&self) -> NodeStreamHandle {
    NodeStreamHandle {
      game_id: self.state.game_id,
      player_id: self.state.player_id,
      tx: self.state.outgoing_sender.clone(),
    }
  }

  async fn worker(
    state: Arc<State>,
    mut stream: FloStream,
    mut w3gs_sender: Sender<W3GSPacket>,
    mut outgoing_receiver: Receiver<Frame>,
    initial_status: NodeGameStatusSnapshot,
    mut scope: SpawnScopeHandle,
  ) {
    let mut event_sender = state.event_sender.clone();

    flo_log::result_ok!(
      "send NodeStreamEvent::NodeGameStatusSnapshot",
      event_sender
        .send(NodeStreamEvent::GameInitialStatus(initial_status))
        .await
    );

    loop {
      tokio::select! {
        _ = scope.left() => {
          tracing::debug!("dropped");
          break;
        }
        // packet from node
        next = stream.recv_frame() => {
          match next {
            Ok(frame) => {
              match frame.type_id {
                PacketTypeId::W3GS => {
                  let pkt = frame_to_w3gs(frame).expect("packet id checked");
                  if let Err(_) = w3gs_sender.send(pkt).await {
                    tracing::debug!("w3gs receiver gone");
                    break;
                  }
                }
                _ => {
                  if let Err(err) = Self::handle_node_frame(&mut event_sender, frame).await {
                    tracing::debug!("handle node frame: {}", err);
                    break;
                  }
                }
              }
            },
            Err(flo_net::error::Error::StreamClosed) => {
              tracing::debug!("stream closed");
              event_sender.send_or_log_as_error(NodeStreamEvent::Disconnected).await;
              break;
            },
            Err(err) => {
              tracing::error!("stream recv: {}", err);
              event_sender.send_or_log_as_error(NodeStreamEvent::Disconnected).await;
              break;
            }
          }
        }
        // outgoing packets
        next = outgoing_receiver.recv() => {
          match next {
            Some(frame) => {
              if let Err(err) = stream.send_frame(frame).await {
                tracing::error!("stream send: {}", err);
                event_sender.send_or_log_as_error(NodeStreamEvent::Disconnected).await;
                break;
              }
            },
            None => {
              tracing::debug!("outgoing sender gone");
              break;
            }
          }
        }
      }
    }
    tracing::debug!("flushing...");
    outgoing_receiver.close();
    while let Some(frame) = outgoing_receiver.recv().await {
      stream.send_frame(frame).await.ok();
    }
    stream.flush().await.ok();
    tracing::debug!("exiting...");
  }

  async fn handle_node_frame(event_sender: &mut NodeStreamEventSender, frame: Frame) -> Result<()> {
    flo_net::try_flo_packet! {
      frame => {
        p: proto::PacketClientUpdateSlotClientStatus => {
          tracing::debug!(game_id = p.game_id, player_id = p.player_id, "update slot client status: {:?}", p.status());
          flo_log::result_ok!(
            "send NodeStreamEvent::SlotClientStatusUpdate",
            event_sender.send(NodeStreamEvent::SlotClientStatusUpdate(S2ProtoUnpack::unpack(p)?)).await
          );
        }
        p: proto::PacketClientUpdateSlotClientStatusReject => {
          tracing::error!(game_id = p.game_id, player_id = p.player_id, "update slot client status rejected: {:?}", p.reason());
          flo_log::result_ok!(
            "send NodeStreamEvent::Disconnected",
            event_sender.send(NodeStreamEvent::Disconnected).await
          );
        }
        p: flo_net::proto::flo_node::PacketNodeGameStatusUpdate => {
          tracing::debug!(game_id = p.game_id, "update game status: {:?}", p);
          flo_log::result_ok!(
            "send NodeStreamEvent::GameStatusUpdate",
            event_sender.send(NodeStreamEvent::GameStatusUpdate(p.into())).await
          );
        }
      }
    }
    Ok(())
  }
}

#[derive(Debug, Clone)]
pub struct NodeStreamHandle {
  game_id: i32,
  player_id: i32,
  tx: Sender<Frame>,
}

impl NodeStreamHandle {
  pub async fn report_slot_status(&mut self, status: SlotClientStatus) -> Result<()> {
    self
      .tx
      .send({
        let mut pkt =
          flo_net::proto::flo_node::PacketClientUpdateSlotClientStatusRequest::default();
        pkt.set_status(status.into_proto_enum());
        pkt.encode_as_frame()?
      })
      .await
      .ok();
    Ok(())
  }

  #[inline]
  pub async fn send_w3gs(&mut self, pkt: Packet) -> Result<()> {
    self.tx.send(w3gs_to_frame(pkt)).await.ok();
    Ok(())
  }
}

#[derive(Debug)]
struct State {
  outgoing_sender: Sender<Frame>,
  event_sender: NodeStreamEventSender,
  token: Vec<u8>,
  game_id: i32,
  player_id: i32,
}

#[derive(Debug, PartialEq, Hash, Eq, Clone)]
pub struct NodeConnectToken([u8; 16]);

impl NodeConnectToken {
  pub fn from_vec(bytes: Vec<u8>) -> Option<Self> {
    if bytes.len() != 16 {
      return None;
    }
    let mut token = NodeConnectToken([0; 16]);
    token.0.copy_from_slice(&bytes[..]);
    Some(token)
  }

  pub fn to_vec(&self) -> Vec<u8> {
    self.0.to_vec()
  }
}

pub type NodeStreamEventSender = EventFromSender<LanEvent, NodeStreamEvent>;

#[derive(Debug)]
pub enum NodeStreamEvent {
  SlotClientStatusUpdate(SlotClientStatusUpdate),
  GameInitialStatus(NodeGameStatusSnapshot),
  GameStatusUpdate(GameStatusUpdate),
  Disconnected,
  // Reconnected,
}

#[derive(Debug, S2ProtoUnpack, serde::Serialize, Clone)]
#[s2_grpc(message_type(
  flo_net::proto::flo_connect::PacketGameSlotClientStatusUpdate,
  flo_net::proto::flo_node::PacketClientUpdateSlotClientStatus
))]
pub struct SlotClientStatusUpdate {
  pub player_id: i32,
  pub game_id: i32,
  #[s2_grpc(proto_enum)]
  pub status: SlotClientStatus,
}

impl FloEvent for NodeStreamEvent {
  const NAME: &'static str = "NodeStreamEvent";
}