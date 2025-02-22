//
// Copyright 2020 The Project Oak Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//

//! Oak Runtime implementation
//!
//! # Features
//!
//! The `oak-unsafe` feature enables various debugging features, including
//! data structure introspection functionality. This feature should only
//! be enabled in development, as it destroys the privacy guarantees of the
//! platform by providing easy channels for the exfiltration of private data.

use crate::{
    channel::{with_reader_channel, with_writer_channel, Channel},
    message::Message,
    metrics::Metrics,
    node::NodeIsolation,
    permissions::PermissionsConfiguration,
    proto::oak::introspection_events::{
        event::EventDetails, ChannelCreated, Direction, Event, HandleCreated, HandleDestroyed,
        MessageDequeued, MessageEnqueued, NodeCreated, NodeDestroyed,
    },
    tls::Certificate,
};
use auth::oidc_utils::ClientInfo;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering::SeqCst};
use itertools::Itertools;
use log::{debug, error, info, trace, warn};
use node::{CreatedNode, NodeFactory};
use oak_abi::{
    label::{top, Label, Tag},
    proto::oak::application::{ApplicationConfiguration, ConfigMap, NodeConfiguration},
    ChannelReadStatus, OakStatus,
};
use oak_io::Message as NodeMessage;
use oak_sign::SignatureBundle;
use prometheus::proto::MetricFamily;
use prost::Message as _;
use rand::RngCore;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    string::String,
    sync::{Arc, Mutex, RwLock},
    thread,
    thread::JoinHandle,
};
use tokio::sync::oneshot;
use tonic::transport::Identity;

pub use channel::{ChannelHalf, ChannelHalfDirection};
pub use config::configure_and_run;
pub use proxy::RuntimeProxy;

pub mod auth;
mod channel;
pub mod config;
#[cfg(feature = "oak-unsafe")]
mod graph;
#[cfg(feature = "oak-unsafe")]
mod introspect;
mod introspection_events;
mod io;
mod message;
mod metrics;
mod node;
pub mod permissions;
mod proto;
mod proxy;
#[cfg(test)]
mod tests;
pub mod time;
pub mod tls;

/// Configuration options that govern the behaviour of the Runtime and the Oak Application running
/// inside it.
#[derive(Default, Clone)]
pub struct RuntimeConfiguration {
    /// Port to run a metrics server on, if provided.
    pub metrics_port: Option<u16>,
    /// Port to run an introspection server on, if provided.
    pub introspect_port: Option<u16>,
    /// Credentials filename for KMS integration, if provided.
    pub kms_credentials: Option<std::path::PathBuf>,
    /// Security options for server pseudo-nodes.
    pub secure_server_configuration: SecureServerConfiguration,
    /// Application configuration.
    pub app_config: ApplicationConfiguration,
    /// Permissions configuration.
    pub permissions_config: PermissionsConfiguration,
    /// Table that contains signatures and public keys corresponding to Oak modules.
    pub sign_table: SignatureTable,
    /// Start-of-day configuration to feed to the running Application.
    pub config_map: ConfigMap,
}

/// Configuration options related to gRPC pseudo-Nodes.
///
/// `Debug` is intentionally not implemented in order to avoid accidentally logging secrets.
#[derive(Default, Clone)]
pub struct GrpcConfiguration {
    /// TLS identity to use for all gRPC Server Nodes.
    pub grpc_server_tls_identity: Option<Identity>,

    /// OpenID Connect Authentication client information.
    pub oidc_client_info: Option<ClientInfo>,

    /// PEM formatted root TLS certificate to use for all gRPC Client Nodes.
    pub grpc_client_root_tls_certificate: Option<Certificate>,
}

/// Configuration options table related to Wasm module signatures.
#[derive(Default, Clone, Debug)]
pub struct SignatureTable {
    /// Map from Oak module hashes to corresponding signatures.
    pub values: HashMap<String, Vec<SignatureBundle>>,
}

/// Configuration options related to HTTP pseudo-Nodes.
///
/// `Debug` is intentionally not implemented in order to avoid accidentally logging secrets.
#[derive(Default, Clone)]
pub struct HttpConfiguration {
    /// TLS identity to use for all HTTP Server Nodes.
    pub tls_config: crate::tls::TlsConfig,
    /// PEM formatted root TLS certificate to use for all HTTP Client Nodes.
    pub http_client_root_tls_certificate: Option<Certificate>,
}

/// Configuration options for secure HTTP and gRPC pseudo-Nodes.
#[derive(Default, Clone)]
pub struct SecureServerConfiguration {
    pub grpc_config: Option<GrpcConfiguration>,
    pub http_config: Option<HttpConfiguration>,
}

struct NodeStopper {
    node_name: String,

    /// Handle used for joining the Node thread.
    join_handle: JoinHandle<()>,

    /// A notification sender object whose receiver is sent to the Node.
    /// The agreement is that the Runtime will notify the Node upon termination
    /// and then start waiting on the join handle. It's up to the Node to figure
    /// out how to actually terminate when receiving a notification.
    notify_sender: oneshot::Sender<()>,
}

impl NodeStopper {
    /// Sends a notification to the Node and joins its thread.
    fn stop_node(self, node_id: NodeId) -> thread::Result<()> {
        let node_debug_id = self.get_debug_id(node_id);
        self.notify_sender
            .send(())
            // Notification errors are discarded since not all of the Nodes save
            // and use the [`oneshot::Receiver`].
            .unwrap_or_else(|()| {
                debug!("{} already dropped `notify_receiver`.", node_debug_id);
            });
        debug!("join thread for node {}...", node_debug_id);
        let result = self.join_handle.join();
        debug!("join thread for node {}...done", node_debug_id);
        result
    }

    /// Returns a unique debug_id used in the debug output, consisting out of
    /// the provided [`NodeId`], and the Node name.
    fn get_debug_id(&self, node_id: NodeId) -> String {
        construct_debug_id(&self.node_name, node_id)
    }
}

impl std::fmt::Debug for NodeStopper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{{node_name='{}', join_handle={:?}, notify_sender={:?}}}",
            self.node_name, self.join_handle, self.notify_sender,
        )
    }
}

struct NodeInfo {
    /// The name for the Node.
    ///
    /// The name does not have to be unique and can be empty. In logs it is
    /// combined with the id to form a unique debug_id.
    name: String,

    /// Name for the type of this Node, for metrics output.
    node_type: &'static str,

    /// The Label associated with this Node.
    ///
    /// This is set at Node creation time and does not change after that.
    ///
    /// See https://github.com/project-oak/oak/blob/main/docs/concepts.md#labels
    label: Label,

    /// The downgrading privilege of this Node.
    privilege: NodePrivilege,

    /// Map of ABI handles to channels.
    abi_handles: HashMap<oak_abi::Handle, ChannelHalf>,

    /// If the Node is currently running, holds the [`NodeStopper`] (with one
    /// small exception, when the Runtime is in the process of closing down and
    /// the [`NodeStopper`] is held by the shutdown processing code).
    node_stopper: Option<NodeStopper>,
}

/// Returns a unique debug_id consisting out of the provided name and [`NodeId`].
pub fn construct_debug_id(name: &str, node_id: NodeId) -> String {
    format!("{}({})", name, node_id.0)
}

impl NodeInfo {
    /// Returns a unique debug_id used to identify the node in the debug output,
    /// consisting out of the provided [`NodeId`], and the node's name.
    fn get_debug_id(&self, node_id: NodeId) -> String {
        construct_debug_id(&self.name, node_id)
    }
}

/// The downgrading (declassification + endorsement) privilege associated with a Node instance.
///
/// See https://github.com/project-oak/oak/blob/main/docs/concepts.md#downgrades
#[derive(Debug, Default, Clone)]
pub struct NodePrivilege {
    /// Tags that may be declassified (removed from the confidentiality component of a label) by
    /// the Node.
    can_declassify_confidentiality_tags: HashSet<Tag>,

    /// Tags that may be endorsed (added to the integrity component of a label) by the Node.
    can_endorse_integrity_tags: HashSet<Tag>,
}

impl NodePrivilege {
    pub fn new(
        can_declassify_confidentiality_tags: HashSet<Tag>,
        can_endorse_integrity_tags: HashSet<Tag>,
    ) -> Self {
        Self {
            can_declassify_confidentiality_tags,
            can_endorse_integrity_tags,
        }
    }

    /// Return the infinite privilege.
    ///
    /// A Node with this privilege can downgrade any data regardless of its label. It should only
    /// be used by the trusted pseudo-nodes.
    pub(crate) fn top_privilege() -> Self {
        let mut top_tag = HashSet::new();
        top_tag.insert(top());
        NodePrivilege {
            can_declassify_confidentiality_tags: top_tag.clone(),
            can_endorse_integrity_tags: top_tag,
        }
    }

    /// Generates a new [`Label`] from `label` that is downgraded as much as possible using the
    /// current privilege.
    fn downgrade_label(&self, label: &Label) -> Label {
        let has_top_privilege = self.can_declassify_confidentiality_tags.contains(&top());

        let confidentiality_tags = if has_top_privilege {
            // Remove all the confidentiality tags if the node has the `top` privilege.
            // TODO(#1631): When we have a separate top for each sub-lattice, this check should be
            // done separately for each sub-lattice, removing only the tags belonging to that
            // sub-lattice.
            vec![]
        } else {
            // Remove all the confidentiality tags that the Node may declassify.
            label
                .confidentiality_tags
                .iter()
                .filter(|t| !self.can_declassify_confidentiality_tags.contains(t))
                .cloned()
                .collect()
        };
        Label {
            confidentiality_tags,
            // Add all the integrity tags that the Node may endorse.
            integrity_tags: label
                .integrity_tags
                .iter()
                .chain(self.can_endorse_integrity_tags.iter())
                .cloned()
                .collect(),
        }
    }
}

impl std::convert::From<NodePrivilege> for Label {
    /// Converts a [`NodePrivilege`] to a [`Label`].
    ///
    /// This is a temporary representation that maps the privilege to a label directly. In future
    /// the plan is to move to robust declassification and transparent endorsement, which would
    /// remove the need for explicitly specifying the node privilege.
    ///
    /// Robust declassification means that the privilege to declassify confidentiality tags will be
    /// implied by integrity tags on the node label itself. Transparent endorsement means that the
    /// privilege to endorse integrity tags will be implied by confidentiality tags on the node
    /// label.
    fn from(privilege: NodePrivilege) -> Self {
        Label {
            confidentiality_tags: privilege
                .can_declassify_confidentiality_tags
                .into_iter()
                .collect(),
            integrity_tags: privilege.can_endorse_integrity_tags.into_iter().collect(),
        }
    }
}

impl std::fmt::Debug for NodeInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "NodeInfo {{'{}', label={:?}, node_stopper={:?}, handles=[",
            self.name, self.label, self.node_stopper,
        )?;
        write!(
            f,
            "{}",
            self.abi_handles
                .iter()
                .map(|(handle, half)| format!("{} => {:?}", handle, half))
                .join(", ")
        )?;
        write!(f, "]}}")
    }
}

/// A unique internal identifier for a Node or pseudo-Node instance.
#[derive(PartialEq, Eq, Hash, Clone, Copy, Debug, PartialOrd, Ord)]
pub struct NodeId(pub u64);

/// Helper types to indicate whether a channel read operation has succeeded or has failed with not
/// enough `bytes_capacity` and/or `handles_capacity`.
#[derive(Debug)]
pub enum NodeReadStatus {
    Success(NodeMessage),
    NeedsCapacity(usize, usize),
}
pub enum ReadStatus {
    Success(Message),
    NeedsCapacity(usize, usize),
}
/// Helper type to indicate whether retrieving a serialized label has succeeded or has failed with
/// not enough capacity.
#[derive(Debug)]
pub enum LabelReadStatus {
    Success(Vec<u8>),
    NeedsCapacity(usize),
}

/// Indicator whether an operation is executed using the Node's label-downgrading privilege or
/// without it.
#[derive(Clone, Copy, Debug)]
enum Downgrading {
    No,
    Yes,
}

/// Information for managing an associated server.
pub struct AuxServer {
    pub name: String,
    pub join_handle: Option<JoinHandle<()>>,
    pub termination_notification_sender: Option<tokio::sync::oneshot::Sender<()>>,
}

impl AuxServer {
    /// Start a new auxiliary server, running on its own thread.
    fn new<F: FnOnce(u16, Arc<Runtime>, tokio::sync::oneshot::Receiver<()>) + 'static + Send>(
        name: &str,
        port: u16,
        runtime: Arc<Runtime>,
        f: F,
    ) -> Self {
        let (termination_notification_sender, termination_notification_receiver) =
            tokio::sync::oneshot::channel::<()>();
        info!("spawning {} server on new thread", name);
        let join_handle = thread::Builder::new()
            .name(format!("{}-server", name))
            .spawn(move || f(port, runtime, termination_notification_receiver))
            .expect("failed to spawn introspection thread");
        AuxServer {
            name: name.to_string(),
            join_handle: Some(join_handle),
            termination_notification_sender: Some(termination_notification_sender),
        }
    }
}

impl Drop for AuxServer {
    /// Dropping an auxiliary server involves notifying it that it should terminate,
    /// then joining its thread.
    fn drop(&mut self) {
        let join_handle = self.join_handle.take();
        let termination_notification_sender = self.termination_notification_sender.take();
        if let Some(termination_notification_sender) = termination_notification_sender {
            info!("stopping {} server", self.name);
            // The auxiliary server may already have stopped, so ignore
            // errors when sending the stop notification.
            let _ = termination_notification_sender.send(());
        }
        if let Some(join_handle) = join_handle {
            let result = join_handle.join();
            info!("stopped {} server, result {:?}", self.name, result);
        }
    }
}

/// Runtime structure for configuring and running a set of Oak Nodes.
pub struct Runtime {
    terminating: AtomicBool,

    next_channel_id: AtomicU64,

    /// Runtime-specific state for each Node instance.
    node_infos: RwLock<HashMap<NodeId, NodeInfo>>,

    next_node_id: AtomicU64,

    aux_servers: Mutex<Vec<AuxServer>>,

    /// Queue of introspection events in chronological order.
    #[allow(dead_code)]
    introspection_event_queue: Mutex<VecDeque<Event>>,

    node_factory: node::ServerNodeFactory,

    pub metrics_data: Metrics,
}

/// Manual implementation of the [`Drop`] trait to ensure that all components of
/// the [`Runtime`] are stopped before the object is dropped.
impl Drop for Runtime {
    fn drop(&mut self) {
        self.stop();
        info!("Runtime instance dropped");
    }
}

// Methods which translate between ABI handles (Node-relative u64 values) and `ChannelHalf`
// values.
impl Runtime {
    /// Register a [`ChannelHalf`] with a Node, returning the new handle value for it.
    fn new_abi_handle(&self, node_id: NodeId, half: ChannelHalf) -> oak_abi::Handle {
        let mut node_infos = self.node_infos.write().unwrap();
        let node_info = node_infos.get_mut(&node_id).expect("Invalid node_id");
        loop {
            let candidate = rand::thread_rng().next_u64();
            if node_info.abi_handles.get(&candidate).is_none() {
                debug!(
                    "{:?}: new ABI handle {} maps to {:?}",
                    node_info.get_debug_id(node_id),
                    candidate,
                    half
                );

                let event_details = HandleCreated {
                    node_id: node_id.0,
                    handle: candidate,
                    channel_id: half.get_channel_id(),
                    direction: match half.direction {
                        ChannelHalfDirection::Read => Direction::Read as i32,
                        ChannelHalfDirection::Write => Direction::Write as i32,
                    },
                };

                node_info.abi_handles.insert(candidate, half);

                self.introspection_event(EventDetails::HandleCreated(event_details));

                return candidate;
            }
        }
    }
    /// Remove the handle from the Node's handle table.
    fn drop_abi_handle(&self, node_id: NodeId, handle: oak_abi::Handle) -> Result<(), OakStatus> {
        let mut node_infos = self.node_infos.write().unwrap();
        let node_info = node_infos.get_mut(&node_id).expect("Invalid node_id");

        match node_info.abi_handles.remove(&handle) {
            Some(half) => {
                self.introspection_event(EventDetails::HandleDestroyed(HandleDestroyed {
                    node_id: node_id.0,
                    handle,
                    channel_id: half.get_channel_id(),
                    direction: match half.direction {
                        ChannelHalfDirection::Read => Direction::Read as i32,
                        ChannelHalfDirection::Write => Direction::Write as i32,
                    },
                }));

                Ok(())
            }
            None => Err(OakStatus::ErrBadHandle),
        }
    }
    /// Convert an ABI handle to an internal [`ChannelHalf`].
    fn abi_to_half(
        &self,
        node_id: NodeId,
        handle: oak_abi::Handle,
    ) -> Result<ChannelHalf, OakStatus> {
        let node_infos = self.node_infos.read().unwrap();
        let node_info = node_infos.get(&node_id).expect("Invalid node_id");
        let half = node_info
            .abi_handles
            .get(&handle)
            .ok_or(OakStatus::ErrBadHandle)?;
        trace!(
            "{:?}: map ABI handle {} to {:?}",
            self.get_node_debug_id(node_id),
            handle,
            half
        );
        Ok(half.clone())
    }
    /// Convert an ABI handle to an internal [`ChannelHalf`], but fail
    /// the operation if the handle is not for the read half of the channel.
    fn abi_to_read_half(
        &self,
        node_id: NodeId,
        handle: oak_abi::Handle,
    ) -> Result<ChannelHalf, OakStatus> {
        let half = self.abi_to_half(node_id, handle)?;
        match half.direction {
            ChannelHalfDirection::Read => Ok(half),
            ChannelHalfDirection::Write => Err(OakStatus::ErrBadHandle),
        }
    }
    /// Convert an ABI handle to an internal [`ChannelHalf`], but fail
    /// the operation if the handle is not for the write half of the channel.
    fn abi_to_write_half(
        &self,
        node_id: NodeId,
        handle: oak_abi::Handle,
    ) -> Result<ChannelHalf, OakStatus> {
        let half = self.abi_to_half(node_id, handle)?;
        match half.direction {
            ChannelHalfDirection::Read => Err(OakStatus::ErrBadHandle),
            ChannelHalfDirection::Write => Ok(half),
        }
    }

    /// Return the direction of an ABI handle.
    pub(crate) fn abi_direction(
        &self,
        node_id: NodeId,
        handle: oak_abi::Handle,
    ) -> Result<ChannelHalfDirection, OakStatus> {
        let half = self.abi_to_half(node_id, handle)?;
        Ok(half.direction)
    }

    /// Return the accumulated metrics for the `Runtime`.
    pub fn gather_metrics(&self) -> Vec<MetricFamily> {
        self.metrics_data.gather()
    }
}

// Methods which handle exposed Runtime functionality.
impl Runtime {
    /// Return whether the [`Runtime`] is terminating.
    pub fn is_terminating(&self) -> bool {
        self.terminating.load(SeqCst)
    }

    /// Signal termination to a [`Runtime`] and wait for its Node threads to terminate.
    pub fn stop(&self) {
        info!("stopping runtime instance");

        // Terminate any running servers.
        self.aux_servers.lock().unwrap().drain(..);

        // Set the terminating flag; this will prevent additional Nodes from starting to wait again,
        // because `wait_on_channels` will return immediately with `OakStatus::ErrTerminated`.
        self.terminating.store(true, SeqCst);

        // Unpark any threads that are blocked waiting on any channels.
        self.notify_all_waiters();

        // Wait for the main thread of each Node to finish. Any thread that was blocked on
        // `wait_on_channels` is now unblocked and received `OakStatus::ErrTerminated`, so we wait
        // for any additional work to be finished here. This may take an arbitrary amount of time,
        // depending on the work that the Node thread has to perform, but at least we know that the
        // it will not be able to enter again in a blocking state.
        let node_stoppers = self.take_node_stoppers();
        for (node_id, node_stopper_opt) in node_stoppers {
            if let Some(node_stopper) = node_stopper_opt {
                let node_debug_id = node_stopper.get_debug_id(node_id);
                info!("stopping node {:?} ...", node_debug_id);
                if let Err(err) = node_stopper.stop_node(node_id) {
                    error!("could not stop node {:?}: {:?}", node_debug_id, err);
                }
                info!("stopping node {:?}...done", node_debug_id);
            }
        }
    }

    /// Move all of the [`NodeStopper`] objects out of the `node_infos` tracker and return them.
    fn take_node_stoppers(&self) -> Vec<(NodeId, Option<NodeStopper>)> {
        let mut node_infos = self
            .node_infos
            .write()
            .expect("could not acquire lock on node_infos");
        node_infos
            .iter_mut()
            .map(|(id, info)| (*id, info.node_stopper.take()))
            .collect()
    }

    /// Notify all Nodes that are waiting on any channels to wake up.
    fn notify_all_waiters(&self) {
        // Hold the write lock and wake up any Node threads blocked on a `Channel`.
        let node_infos = self
            .node_infos
            .read()
            .expect("could not acquire lock on node_infos");
        for node_id in node_infos.keys().sorted() {
            let node_info = node_infos.get(node_id).unwrap();
            for (handle, half) in &node_info.abi_handles {
                debug!(
                    "waking waiters on {:?} handle {} => {:?}",
                    node_info.name, handle, half
                );
                half.wake_waiters();
            }
        }
    }

    /// Returns a clone of the [`Label`] associated with the provided `node_id`, in order to limit
    /// the scope of holding the lock on [`Runtime::node_infos`].
    ///
    /// Panics if `node_id` is invalid.
    fn get_node_label(&self, node_id: NodeId) -> Label {
        let node_infos = self
            .node_infos
            .read()
            .expect("could not acquire lock on node_infos");
        let node_info = node_infos.get(&node_id).expect("invalid node_id");
        node_info.label.clone()
    }

    /// Returns the least restrictive (i.e. least confidential, most trusted) label that this Node
    /// may downgrade `initial_label` to. This takes into account all the [downgrade
    /// privilege](NodeInfo::privilege) that the node possesses.
    fn get_node_downgraded_label(&self, node_id: NodeId, initial_label: &Label) -> Label {
        // Retrieve the set of tags that the node may downgrade.
        let node_privilege = self.get_node_privilege(node_id);
        node_privilege.downgrade_label(initial_label)
    }

    /// Returns the effective label for `initial_label` in the context of the Node, taking into
    /// account whether the downgrade privilege should be applied.
    fn get_effective_label(
        &self,
        node_id: NodeId,
        initial_label: &Label,
        downgrade: Downgrading,
    ) -> Label {
        match downgrade {
            Downgrading::Yes => self.get_node_downgraded_label(node_id, initial_label),
            Downgrading::No => initial_label.to_owned(),
        }
    }

    /// Returns a clone of the [`NodePrivilege`] of the provided Node.
    fn get_node_privilege(&self, node_id: NodeId) -> NodePrivilege {
        let node_infos = self
            .node_infos
            .read()
            .expect("could not acquire lock on node_infos");
        let node_info = node_infos.get(&node_id).expect("invalid node_id");
        node_info.privilege.clone()
    }

    /// Returns a unique debug_id used to identify the Node in the debug output,
    /// consisting out of the provided [`NodeId`], and the Node name.
    fn get_node_debug_id(&self, node_id: NodeId) -> String {
        let node_infos = self.node_infos.read().unwrap();
        node_infos
            .get(&node_id)
            .expect("Invalid node_id")
            .get_debug_id(node_id)
    }

    /// Returns a clone of the [`Label`] associated with the provided reader `channel_half`.
    ///
    /// Returns an error if `channel_half` is not a valid read half.
    fn get_reader_channel_label(&self, channel_half: &ChannelHalf) -> Result<Label, OakStatus> {
        with_reader_channel(channel_half, |channel| Ok(channel.label.clone()))
    }

    /// Returns a clone of the [`Label`] associated with the provided writer `channel_half`.
    ///
    /// Returns an error if `channel_half` is not a valid write half.
    fn get_writer_channel_label(&self, channel_half: &ChannelHalf) -> Result<Label, OakStatus> {
        with_writer_channel(channel_half, |channel| Ok(channel.label.clone()))
    }

    /// Returns the [`Label`] associated with the channel handle serialized as a byte array.
    ///
    /// If the serialized size is larger than the specified capacity, it will return a status
    /// indicating the required capacity.
    fn get_serialized_channel_label(
        &self,
        node_id: NodeId,
        handle: oak_abi::Handle,
        capacity: usize,
    ) -> Result<LabelReadStatus, OakStatus> {
        let label = self.get_channel_label(node_id, handle)?;
        serialize_label(label, capacity)
    }

    /// Returns the [`Label`] associated with the node serialized as a byte array.
    ///
    /// If the serialized size is larger than the specified capacity, it will return a status
    /// indicating the required capacity.
    fn get_serialized_node_label(
        &self,
        node_id: NodeId,
        capacity: usize,
    ) -> Result<LabelReadStatus, OakStatus> {
        serialize_label(self.get_node_label(node_id), capacity)
    }

    /// Returns the [`NodePrivilege`] associated with the node converted to a [`Label`] and
    /// serialized as a byte array.
    ///
    /// If the serialized size is larger than the specified capacity, it will return a status
    /// indicating the required capacity.
    fn get_serialized_node_privilege(
        &self,
        node_id: NodeId,
        capacity: usize,
    ) -> Result<LabelReadStatus, OakStatus> {
        serialize_label(self.get_node_privilege(node_id).into(), capacity)
    }

    /// Returns the [`Label`] associated with the channel handle.
    fn get_channel_label(
        &self,
        node_id: NodeId,
        handle: oak_abi::Handle,
    ) -> Result<Label, OakStatus> {
        let half = self.abi_to_half(node_id, handle)?;
        match half.direction {
            ChannelHalfDirection::Read => self.get_reader_channel_label(&half),
            ChannelHalfDirection::Write => self.get_writer_channel_label(&half),
        }
    }

    /// Returns whether the given Node is allowed to read from the provided channel read half,
    /// according to their respective [`Label`]s.
    fn validate_can_read_from_channel(
        &self,
        node_id: NodeId,
        channel_half: &ChannelHalf,
        downgrade: Downgrading,
    ) -> Result<(), OakStatus> {
        let channel_label = self.get_reader_channel_label(channel_half)?;
        self.validate_can_read_from_label(node_id, &channel_label, downgrade)
    }

    /// Returns whether the given Node is allowed to read from an entity with the provided
    /// [`Label`], taking into account all the [downgrade privilege](NodeInfo::privilege) the Node
    /// possesses.
    fn validate_can_read_from_label(
        &self,
        node_id: NodeId,
        source_label: &Label,
        downgrade: Downgrading,
    ) -> Result<(), OakStatus> {
        let target_label = self.get_node_label(node_id);
        let node_debug_id = self.get_node_debug_id(node_id);
        trace!(
            "{:?}: original source label: {:?}?",
            node_debug_id,
            source_label
        );

        let effective_label = self.get_effective_label(node_id, source_label, downgrade);
        trace!(
            "{:?}: effective label: {:?}?",
            node_debug_id,
            effective_label
        );
        trace!("{:?}: target label: {:?}?", node_debug_id, target_label);
        if effective_label.flows_to(&target_label) {
            trace!("{:?}: can read from {:?}", node_debug_id, source_label);
            Ok(())
        } else {
            debug!("{:?}: cannot read from {:?}", node_debug_id, source_label);
            Err(OakStatus::ErrPermissionDenied)
        }
    }

    /// Returns whether the given Node is allowed to write to the provided channel write half,
    /// according to their respective [`Label`]s.
    fn validate_can_write_to_channel(
        &self,
        node_id: NodeId,
        channel_half: &ChannelHalf,
        downgrade: Downgrading,
    ) -> Result<(), OakStatus> {
        let channel_label = self.get_writer_channel_label(channel_half)?;
        self.validate_can_write_to_label(node_id, &channel_label, downgrade)
    }

    /// Returns whether the given Node is allowed to write to an entity with the provided [`Label`],
    /// taking into account all the [downgrade privilege](NodeInfo::privilege) the Node possesses.
    fn validate_can_write_to_label(
        &self,
        node_id: NodeId,
        target_label: &Label,
        downgrade: Downgrading,
    ) -> Result<(), OakStatus> {
        let original_label = self.get_node_label(node_id);
        let node_debug_id = self.get_node_debug_id(node_id);
        trace!(
            "{:?}: original source label: {:?}?",
            node_debug_id,
            &original_label
        );
        let effective_label = self.get_effective_label(node_id, &original_label, downgrade);
        trace!(
            "{:?}: effective label: {:?}?",
            node_debug_id,
            effective_label
        );
        trace!("{:?}: target label: {:?}?", node_debug_id, target_label);
        if effective_label.flows_to(target_label) {
            trace!("{:?}: can write to {:?}", node_debug_id, target_label);
            Ok(())
        } else {
            warn!("{:?}: cannot write to {:?}", node_debug_id, target_label);
            Err(OakStatus::ErrPermissionDenied)
        }
    }

    /// Creates a new [`Channel`] and returns a `(writer, reader)` pair of [`oak_abi::Handle`]s.
    fn channel_create(
        self: &Arc<Self>,
        node_id: NodeId,
        name: &str,
        label: &Label,
        downgrade: Downgrading,
    ) -> Result<(oak_abi::Handle, oak_abi::Handle), OakStatus> {
        if self.is_terminating() {
            return Err(OakStatus::ErrTerminated);
        }

        // The label (and mere presence) of the newly created Channel is effectively public, so we
        // must ensure that the label of the calling Node flows to both "public untrusted" and to
        // the label of the Channel to be created.
        self.validate_can_write_to_label(node_id, &Label::public_untrusted(), downgrade)?;
        // We also additionally make sure that the label of the newly created Channel can be written
        // to by the current Node, since in general this may be lower than "public untrusted".
        self.validate_can_write_to_label(node_id, label, downgrade)?;

        // First get a pair of `ChannelHalf` objects.
        let channel_id = self.next_channel_id.fetch_add(1, SeqCst);
        let channel = Channel::new(channel_id, name, label, Arc::downgrade(self));
        let write_half = ChannelHalf::new(channel.clone(), ChannelHalfDirection::Write);
        let read_half = ChannelHalf::new(channel, ChannelHalfDirection::Read);
        let node_debug_id = self.get_node_debug_id(node_id);
        trace!(
            "{:?}: allocated channel with halves w={:?},r={:?}",
            node_debug_id,
            write_half,
            read_half,
        );

        // TODO(#913): Add automated tests that verify that ChannelCreated is
        // always fired prior to any other introspection events related to the
        // channel.
        self.introspection_event(EventDetails::ChannelCreated(ChannelCreated {
            channel_id,
            name: name.to_owned(),
            label: Some(label.clone()),
        }));

        // Insert them into the handle table and return the ABI handles to the caller.
        let write_handle = self.new_abi_handle(node_id, write_half);
        let read_handle = self.new_abi_handle(node_id, read_half);
        trace!(
            "{:?}: allocated handles w={}, r={} for channel",
            node_debug_id,
            write_handle,
            read_handle,
        );

        Ok((write_handle, read_handle))
    }

    /// Creates a new distinct handle to the same channel as `handle`.
    fn handle_clone(
        self: &Arc<Self>,
        node_id: NodeId,
        handle: oak_abi::Handle,
    ) -> Result<oak_abi::Handle, OakStatus> {
        if self.is_terminating() {
            return Err(OakStatus::ErrTerminated);
        }

        let cloned_half = self.abi_to_half(node_id, handle)?;
        Ok(self.new_abi_handle(node_id, cloned_half))
    }

    /// Reads the readable statuses for a slice of `ChannelHalf`s.
    fn readers_statuses(
        &self,
        node_id: NodeId,
        readers: &[ChannelHalf],
        downgrade: Downgrading,
    ) -> Vec<ChannelReadStatus> {
        readers
            .iter()
            .map(|half| {
                self.channel_status(node_id, half, downgrade)
                    .unwrap_or(ChannelReadStatus::InvalidChannel)
            })
            .collect()
    }

    /// Given a slice of `ChannelHalf`s representing channel read handles:
    /// - If the [`Runtime`] is terminating this will return immediately with an `ErrTerminated`
    ///   status.
    /// - If any of the channels is in an erroneous status, e.g. when a channel is orphaned, this
    ///   will immediately return with all the channels statuses set in the returned vector.
    /// - If all channels are in a good status but no messages are available on any of the channels
    ///   (i.e., all channels have status [`ChannelReadStatus::NotReady`]),
    ///   [`Runtime::wait_on_channels`] blocks until a message is available on one of the channels,
    ///   or one of the channels is orphaned. In both cases a vector of all the channels statuses
    ///   will be returned, unless the [`Runtime`] is terminating, in which case
    ///   `Err(ErrTerminated)` will be returned.
    ///
    /// Invariant: The returned vector of [`ChannelReadStatus`] values will be in 1-1
    /// correspondence with the passed-in vector of [`oak_abi::Handle`]s.
    ///
    /// See also the host ABI function
    /// [`wait_on_channels`](https://github.com/project-oak/oak/blob/main/docs/abi.md#wait_on_channels).
    fn wait_on_channels(
        &self,
        node_id: NodeId,
        read_handles: &[oak_abi::Handle],
        downgrade: Downgrading,
    ) -> Result<Vec<ChannelReadStatus>, OakStatus> {
        // Accumulate both the valid channels and their original position.
        let mut all_statuses = vec![ChannelReadStatus::InvalidChannel; read_handles.len()];
        let mut reader_pos = Vec::new();
        let mut readers = Vec::new();
        for (i, handle) in read_handles.iter().enumerate() {
            if let Ok(half) = self.abi_to_read_half(node_id, *handle) {
                reader_pos.push(i);
                readers.push(half);
            }
        }

        let thread = thread::current();

        let node_debug_id = self.get_node_debug_id(node_id);

        while !self.is_terminating() {
            // Create a new Arc each iteration to be dropped after `thread::park` e.g. when the
            // thread is resumed. When the Arc is deallocated, any remaining `Weak`
            // references in `Channel`s will be orphaned. This means thread::unpark will
            // not be called multiple times. Even if thread unpark is called spuriously
            // and we wake up early, no channel statuses will be ready and so we can
            // just continue.
            //
            // Note we read statuses directly after adding waiters, before blocking to ensure that
            // there are no messages, after we have been added as a waiter.

            let thread_ref = Arc::new(thread.clone());

            for reader in &readers {
                with_reader_channel(reader, |channel| {
                    channel.add_waiter(&thread_ref);
                    Ok(())
                })?;
            }
            let statuses = self.readers_statuses(node_id, &readers, downgrade);
            // Transcribe the status for valid channels back to the original position
            // in the list of all statuses.
            for i in 0..readers.len() {
                all_statuses[reader_pos[i]] = statuses[i];
            }

            let all_not_ready = statuses.iter().all(|&s| s == ChannelReadStatus::NotReady);

            if !all_not_ready || read_handles.is_empty() || readers.len() != read_handles.len() {
                return Ok(all_statuses);
            }

            debug!(
                "{:?}: wait_on_channels: channels not ready, parking thread {:?}",
                node_debug_id,
                thread::current()
            );

            thread::park();

            debug!(
                "{:?}: wait_on_channels: thread {:?} re-woken",
                node_debug_id,
                thread::current()
            );
        }
        Err(OakStatus::ErrTerminated)
    }

    /// Write a message to a channel. Fails with [`OakStatus::ErrChannelClosed`] if the underlying
    /// channel has been orphaned.
    fn channel_write(
        &self,
        node_id: NodeId,
        write_handle: oak_abi::Handle,
        node_msg: NodeMessage,
        downgrade: Downgrading,
    ) -> Result<(), OakStatus> {
        let half = self.abi_to_write_half(node_id, write_handle)?;
        self.validate_can_write_to_channel(node_id, &half, downgrade)?;

        let event_details = MessageEnqueued {
            node_id: node_id.0,
            channel_id: half.get_channel_id(),
            included_handles: node_msg.handles.clone(),
        };

        // Translate the Node-relative handles in the `NodeMessage` to channel halves.
        let msg = self.message_from(node_msg, node_id)?;
        let result = with_writer_channel(&half, |channel| {
            if !channel.has_readers() {
                return Err(OakStatus::ErrChannelClosed);
            }
            channel.messages.write().unwrap().push_back(msg);
            channel.wake_waiters();

            Ok(())
        });

        self.introspection_event(EventDetails::MessageEnqueued(event_details));

        result
    }

    /// Translate the Node-relative handles in the `NodeMessage` to channel halves.
    fn message_from(&self, node_msg: NodeMessage, node_id: NodeId) -> Result<Message, OakStatus> {
        Ok(Message {
            data: node_msg.bytes,
            channels: node_msg
                .handles
                .into_iter()
                .map(|handle| self.abi_to_half(node_id, handle))
                .collect::<Result<Vec<ChannelHalf>, OakStatus>>()?,
        })
    }

    /// Read a message from a channel. Fails with [`OakStatus::ErrChannelClosed`] if
    /// the underlying channel is empty and has been orphaned.
    fn channel_read(
        &self,
        node_id: NodeId,
        read_handle: oak_abi::Handle,
        downgrade: Downgrading,
    ) -> Result<Option<NodeMessage>, OakStatus> {
        let half = self.abi_to_read_half(node_id, read_handle)?;
        self.validate_can_read_from_channel(node_id, &half, downgrade)?;
        match with_reader_channel(&half, |channel| {
            match channel.messages.write().unwrap().pop_front() {
                Some(m) => Ok(Some(m)),
                None => {
                    if !channel.has_writers() {
                        Err(OakStatus::ErrChannelClosed)
                    } else {
                        Ok(None)
                    }
                }
            }
        }) {
            Err(status) => Err(status),
            Ok(None) => Ok(None),
            Ok(Some(runtime_msg)) => {
                let node_msg = self.node_message_from(runtime_msg, node_id);

                self.introspection_event(EventDetails::MessageDequeued(MessageDequeued {
                    node_id: node_id.0,
                    channel_id: half.get_channel_id(),
                    acquired_handles: node_msg.handles.clone(),
                }));

                Ok(Some(node_msg))
            }
        }
    }

    /// Determine the readable status of a channel, returning:
    /// - `Ok`([`ChannelReadStatus::ReadReady`]) if there is at least one message in the channel.
    /// - `Ok`([`ChannelReadStatus::Orphaned`]) if there are no messages and there are no writers.
    /// - `Ok`([`ChannelReadStatus::NotReady`]) if there are no messages but there are some writers.
    /// - `Ok`([`ChannelReadStatus::PermissionDenied`]) if the node does not have permission to read
    ///   from the channel.
    /// - `Err`([`OakStatus::ErrBadHandle`]) if the input handle does not indicate the read half of
    ///   a channel.
    fn channel_status(
        &self,
        node_id: NodeId,
        half: &ChannelHalf,
        downgrade: Downgrading,
    ) -> Result<ChannelReadStatus, OakStatus> {
        if let Err(OakStatus::ErrPermissionDenied) =
            self.validate_can_read_from_channel(node_id, half, downgrade)
        {
            return Ok(ChannelReadStatus::PermissionDenied);
        };
        with_reader_channel(half, |channel| {
            Ok(if channel.messages.read().unwrap().front().is_some() {
                ChannelReadStatus::ReadReady
            } else if !channel.has_writers() {
                ChannelReadStatus::Orphaned
            } else {
                ChannelReadStatus::NotReady
            })
        })
    }

    /// Reads a message from the channel if `bytes_capacity` and `handles_capacity` are large
    /// enough to accept the message. Fails with `OakStatus::ErrChannelClosed` if the underlying
    /// channel has been orphaned _and_ is empty. If there was not enough `bytes_capacity` or
    /// `handles_capacity`, `try_read_message` returns the required capacity values in
    /// `Some(NodeReadStatus::NeedsCapacity(needed_bytes_capacity,needed_handles_capacity))`. Does
    /// not guarantee that the next call will succeed after capacity adjustments as another Node
    /// may have read the original message.
    fn channel_try_read_message(
        &self,
        node_id: NodeId,
        handle: oak_abi::Handle,
        bytes_capacity: usize,
        handles_capacity: usize,
        downgrade: Downgrading,
    ) -> Result<Option<NodeReadStatus>, OakStatus> {
        let half = self.abi_to_read_half(node_id, handle)?;
        self.validate_can_read_from_channel(node_id, &half, downgrade)?;
        let result = with_reader_channel(&half, |channel| {
            let mut messages = channel.messages.write().unwrap();
            match messages.front() {
                Some(front) => {
                    let req_bytes_capacity = front.data.len();
                    let req_handles_capacity = front.channels.len();

                    if req_bytes_capacity > bytes_capacity
                        || req_handles_capacity > handles_capacity
                    {
                        Ok(Some(ReadStatus::NeedsCapacity(
                            req_bytes_capacity,
                            req_handles_capacity,
                        )))
                    } else {
                        Ok(Some(ReadStatus::Success(messages.pop_front().expect(
                            "Front element disappeared while we were holding the write lock!",
                        ))))
                    }
                }
                None => {
                    if !channel.has_writers() {
                        Err(OakStatus::ErrChannelClosed)
                    } else {
                        Ok(None)
                    }
                }
            }
        })?;
        // Translate the result into the handle numbering space of this Node.
        Ok(match result {
            None => None,
            Some(ReadStatus::NeedsCapacity(z, c)) => Some(NodeReadStatus::NeedsCapacity(z, c)),
            Some(ReadStatus::Success(msg)) => {
                let message = self.node_message_from(msg, node_id);

                self.introspection_event(EventDetails::MessageDequeued(MessageDequeued {
                    node_id: node_id.0,
                    channel_id: half.get_channel_id(),
                    acquired_handles: message.handles.clone(),
                }));

                Some(NodeReadStatus::Success(message))
            }
        })
    }

    /// Translate a Message to include ABI handles (which are relative to this Node) rather than
    /// internal channel references.
    fn node_message_from(&self, msg: Message, node_id: NodeId) -> NodeMessage {
        NodeMessage {
            bytes: msg.data,
            handles: msg
                .channels
                .iter()
                .map(|half| self.new_abi_handle(node_id, half.clone()))
                .collect(),
        }
    }

    /// Close an [`oak_abi::Handle`], potentially orphaning the underlying [`channel::Channel`].
    fn channel_close(&self, node_id: NodeId, handle: oak_abi::Handle) -> Result<(), OakStatus> {
        // Remove the ABI handle -> half mapping; half will be dropped at end of scope.
        self.drop_abi_handle(node_id, handle)?;
        Ok(())
    }

    /// Create a fresh [`NodeId`].
    fn new_node_id(&self) -> NodeId {
        NodeId(self.next_node_id.fetch_add(1, SeqCst))
    }

    /// Remove a Node by [`NodeId`] from the [`Runtime`].
    fn remove_node_id(&self, node_id: NodeId) {
        // Close any remaining handles
        let (remaining_handles, node_type): (Vec<_>, &'static str) = {
            let node_infos = self.node_infos.read().unwrap();
            let node_info = node_infos
                .get(&node_id)
                .unwrap_or_else(|| panic!("remove_node_id: No such node_id {:?}", node_id));
            (
                node_info.abi_handles.keys().copied().collect(),
                node_info.node_type,
            )
        };

        debug!(
            "{:?}: remove_node_id() found open handles on exit: {:?}",
            self.get_node_debug_id(node_id),
            remaining_handles
        );

        for handle in remaining_handles {
            self.channel_close(node_id, handle)
                .expect("remove_node_id: Unable to close hanging channel!");
        }

        self.node_infos
            .write()
            .unwrap()
            .remove(&node_id)
            .expect("remove_node_id: Node didn't exist!");
        self.update_nodes_count_metric(node_type, -1);

        self.introspection_event(EventDetails::NodeDestroyed(NodeDestroyed {
            node_id: node_id.0,
        }))
    }

    /// Add an [`NodeId`] [`NodeInfo`] pair to the [`Runtime`]. This method temporarily holds the
    /// [`Runtime::node_infos`] write lock.
    fn add_node_info(&self, node_id: NodeId, node_info: NodeInfo) {
        let node_type = node_info.node_type;
        self.node_infos
            .write()
            .expect("could not acquire lock on node_infos")
            .insert(node_id, node_info);
        self.update_nodes_count_metric(node_type, 1);
    }

    /// Add the [`NodeStopper`] for a running Node to `NodeInfo`.
    /// The provided [`NodeId`] value must already be present in [`Runtime::node_infos`].
    fn add_node_stopper(&self, node_id: NodeId, node_stopper: NodeStopper) {
        let mut node_infos = self
            .node_infos
            .write()
            .expect("could not acquire lock on node_infos");
        match node_infos.get_mut(&node_id) {
            Some(node_info) => {
                assert!(node_info.node_stopper.is_none());
                node_info.node_stopper = Some(node_stopper);
            }
            None => {
                // If the node thread terminated before this method is invoked, its NodeInfo entry
                // may have already been deleted, in which case we just log a warning and continue.
                // See https://github.com/project-oak/oak/issues/1762.
                warn!("No NodeInfo found for node {:?}", node_id);
            }
        }
    }

    /// Create a Node within the [`Runtime`] with the specified name and based on the provided
    /// configuration. The channel identified by `initial_handle` is installed in the new Node's
    /// handle table and the new handle value is passed to the newly created Node.
    ///
    /// The caller also specifies a [`Label`], which is assigned to the newly created Node. See
    /// <https://github.com/project-oak/oak/blob/main/docs/concepts.md#labels> for more
    /// information on labels.
    ///
    /// This method is defined on [`Arc`] and not [`Runtime`] itself, so that
    /// the [`Arc`] can clone itself and be included in a [`RuntimeProxy`] object
    /// to be given to a new Node instance.
    fn node_create_and_register(
        self: Arc<Self>,
        node_id: NodeId,
        name: &str,
        config: &NodeConfiguration,
        label: &Label,
        initial_handle: oak_abi::Handle,
        downgrade: Downgrading,
    ) -> Result<(), OakStatus> {
        // This only creates a Node instance, but does not start it.
        let instance = self.node_factory.create_node(name, config).map_err(|err| {
            warn!("could not create node: {:?}", err);
            OakStatus::ErrInvalidArgs
        })?;

        // Register the instance within the `Runtime`.
        self.node_register(node_id, instance, name, label, initial_handle, downgrade)
    }

    /// Registers the given [`CreatedNode`] instance within the [`Runtime`]. The registration fails
    /// if the labels violate the IFC rules.
    ///
    /// If `downgrade` is set to [`Downgrading::Yes`], the calling Node's downgrading privilege is
    /// taken into account when checking IFC restrictions.
    fn node_register(
        self: Arc<Self>,
        node_id: NodeId,
        created_node: CreatedNode,
        node_name: &str,
        label: &Label,
        initial_handle: oak_abi::Handle,
        downgrade: Downgrading,
    ) -> Result<(), OakStatus> {
        if self.is_terminating() {
            return Err(OakStatus::ErrTerminated);
        }

        // The label (and mere presence) of the newly created Node is effectively public, so we must
        // ensure that the label of the calling Node flows to both "public untrusted" and to the
        // label of the Node to be created.
        self.validate_can_write_to_label(node_id, &Label::public_untrusted(), downgrade)?;
        // We also additionally make sure that the label of the newly created Node can be written to
        // by the current Node, since in general this may be lower than "public untrusted".
        self.validate_can_write_to_label(node_id, label, downgrade)?;

        let instance = created_node.instance;

        let node_type = instance.node_type();
        let node_privilege = created_node.privilege;

        // If the new node is not sandboxed it can communicate externally without restriction, so we
        // should make sure that it has the privilege to downgrade its label to "public untrusted"
        // before registering and starting it.
        match instance.isolation() {
            NodeIsolation::Uncontrolled => {
                let downgraded_label = node_privilege.downgrade_label(label);
                debug!(
                    "Maximum downgraded label for node {}: {:?}",
                    node_name, &downgraded_label
                );
                if !downgraded_label.flows_to(&Label::public_untrusted()) {
                    error!(
                        "Node {} of type {} has insufficent privilege.",
                        node_name, node_type
                    );
                    return Err(OakStatus::ErrPermissionDenied);
                };
            }
            NodeIsolation::Sandboxed => {
                trace!(
                    "Node {} of type {} is sandboxed, so not checking privilege.",
                    node_name,
                    node_type
                );
            }
        }

        let reader = self.abi_to_read_half(node_id, initial_handle)?;

        let new_node_proxy = self.clone().proxy_for_new_node(node_name);
        let new_node_id = new_node_proxy.node_id;

        self.node_configure_instance(new_node_id, node_type, node_name, label, &node_privilege);
        let initial_handle = new_node_proxy
            .runtime
            .new_abi_handle(new_node_proxy.node_id, reader);

        info!(
            "{:?}: start node instance {:?} of type {} with privilege {:?}",
            self.get_node_debug_id(node_id),
            self.get_node_debug_id(new_node_id),
            node_type,
            node_privilege
        );
        let node_stopper = self.clone().node_start_instance(
            node_name,
            instance,
            new_node_proxy,
            initial_handle,
        )?;

        // Insert the now running instance to the list of running instances (by moving it), so that
        // `Node::stop` will be called on it eventually.
        self.add_node_stopper(new_node_id, node_stopper);

        Ok(())
    }

    /// Starts running a newly created Node instance on a new thread.
    /// The `node_name` parameter is only used for diagnostic/debugging output.
    fn node_start_instance(
        self: Arc<Self>,
        node_name: &str,
        node_instance: Box<dyn crate::node::Node>,
        node_proxy: RuntimeProxy,
        initial_handle: oak_abi::Handle,
    ) -> Result<NodeStopper, OakStatus> {
        // Try to start the Node instance.
        //
        // In order for this to work correctly, the `NodeInfo` entry must already exist in
        // `Runtime`, which is why we could not start this instance before the call to
        // `Runtime::add_node_info` above.
        //
        // On the other hand, we also cannot start it after the call to `Runtime::add_node_instance`
        // below, because that takes ownership of the instance itself.
        //
        // We also want no locks to be held while the instance is starting.
        let node_id = node_proxy.node_id;
        let (node_notify_sender, node_notify_receiver) = tokio::sync::oneshot::channel::<()>();
        let node_join_handle = thread::Builder::new()
            .name(node_name.to_string())
            .spawn(move || {
                node_proxy.set_as_current();
                node_instance.run(node_proxy, initial_handle, node_notify_receiver);
                // It's now safe to remove the state for this Node, as there's nothing left
                // that can invoke `Runtime` functionality for it.
                self.remove_node_id(node_id)
            })
            .expect("failed to spawn thread");
        // Note: self has been moved into the thread running the closure.

        Ok(NodeStopper {
            node_name: node_name.to_string(),
            join_handle: node_join_handle,
            notify_sender: node_notify_sender,
        })
    }

    /// Configure data structures for a Node instance.
    fn node_configure_instance(
        &self,
        node_id: NodeId,
        node_type: &'static str,
        node_name: &str,
        label: &Label,
        privilege: &NodePrivilege,
    ) {
        // TODO(#913): Add automated tests that verify that NodeCreated is
        // always fired prior to any other introspection events related to the
        // node.
        self.introspection_event(EventDetails::NodeCreated(NodeCreated {
            node_id: node_id.0,
            name: node_name.to_string(),
            label: Some(label.clone()),
        }));

        self.add_node_info(
            node_id,
            NodeInfo {
                name: node_name.to_string(),
                node_type,
                label: label.clone(),
                privilege: privilege.clone(),
                abi_handles: HashMap::new(),
                node_stopper: None,
            },
        );
    }

    /// Create a [`RuntimeProxy`] instance for a new Node, creating the new [`NodeId`]
    /// value along the way.
    fn proxy_for_new_node(self: Arc<Self>, node_name: &str) -> RuntimeProxy {
        let node_id = self.new_node_id();
        RuntimeProxy {
            runtime: self,
            node_id,
            node_name: node_name.to_string(),
        }
    }

    /// Update the node count metric with the current value.
    fn update_nodes_count_metric(&self, node_type: &'static str, delta: i64) {
        self.metrics_data
            .runtime_metrics
            .runtime_nodes_by_type
            .with_label_values(&[node_type])
            .add(delta);
    }
}

/// Searializes a [`Label`] as a byte array.
///
/// If the serialized size is larger than the specified capacity, it will return a status
/// indicating the required capacity.
fn serialize_label(label: Label, capacity: usize) -> Result<LabelReadStatus, OakStatus> {
    let size = label.encoded_len();
    if size > capacity {
        Ok(LabelReadStatus::NeedsCapacity(size))
    } else {
        let mut encoded = Vec::with_capacity(size);
        match label.encode(&mut encoded) {
            Err(error) => {
                error!("Could not encode label: {}", error);
                Err(OakStatus::ErrInternal)
            }
            Ok(()) => Ok(LabelReadStatus::Success(encoded)),
        }
    }
}
