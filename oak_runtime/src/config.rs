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

//! Functionality covering configuration of a Runtime instance.

use crate::{io::SenderExt, Runtime, RuntimeConfiguration, RuntimeProxy};
use log::{error, info};
use oak_io::{handle::WriteHandle, OakError};
use std::sync::Arc;

/// Configures a [`Runtime`] from the given [`RuntimeConfiguration`] and begins execution.
///
/// Returns a [`RuntimeProxy`] for an initial implicit Node, and a writeable [`oak_abi::Handle`] to
/// send messages into the Runtime. Creating a new channel and passing the write [`oak_abi::Handle`]
/// into the runtime will enable messages to be read back out from the [`RuntimeProxy`].
pub fn configure_and_run(config: RuntimeConfiguration) -> Result<Arc<Runtime>, OakError> {
    let proxy = RuntimeProxy::create_runtime(
        &config.app_config,
        &config.permissions_config,
        &config.secure_server_configuration,
        &config.sign_table,
        config.kms_credentials.as_ref(),
    );
    proxy.set_as_current();
    let config_map = config.config_map.clone();
    let handle = proxy.start_runtime(config)?;

    // Pass in the config map over the initial channel.
    let sender = crate::io::Sender::new(WriteHandle { handle });
    info!("Send in initial config map");
    sender.send(config_map, &proxy)?;

    if let Err(err) = sender.close(&proxy) {
        error!("Failed to close initial handle {:?}: {:?}", handle, err);
    }

    // Now that the implicit initial Node has been used to inject the
    // Application's `ConfigMap`, drop all reference to it.
    Ok(proxy.runtime)
}
