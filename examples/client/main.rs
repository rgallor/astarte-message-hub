/*
 * This file is part of Astarte.
 *
 * Copyright 2023 SECO Mind Srl
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *    http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 *
 * SPDX-License-Identifier: Apache-2.0
 */

//! Astarte Message Hub client example, will send the uptime every 3 seconds to Astarte.

use astarte_device_sdk::builder::DeviceBuilder;
use astarte_device_sdk::prelude::*;
use astarte_device_sdk::store::memory::MemoryStore;
use astarte_device_sdk::transport::grpc::GrpcConfig;
use astarte_device_sdk::{EventLoop, Interface};

use std::str::FromStr;
use std::time;

use clap::Parser;
use log::{debug, error, info, warn};
use tokio::select;
use tokio::signal::ctrl_c;
use tokio::task::JoinHandle;
use uuid::Uuid;

/// Create a ProtoBuf client for the Astarte message hub.
#[derive(Parser, Debug)]
#[clap(version, about)]
struct Cli {
    /// UUID to be used when registering the client as an Astarte message hub node.
    #[clap(default_value = "d1e7a6e9-cf99-4694-8fb6-997934be079c")]
    uuid: String,

    /// Endpoint of the Astarte Message Hub server instance
    #[clap(default_value = "http://[::1]:50051")]
    endpoint: String,

    /// Stop after sending COUNT messages.
    #[clap(short, long)]
    count: Option<u64>,

    /// Milliseconds to wait between messages.
    #[clap(short, long, default_value = "3000")]
    time: u64,
}

type DynError = Box<dyn std::error::Error + Send + Sync + 'static>;

#[tokio::main]
async fn main() -> Result<(), DynError> {
    env_logger::init();
    let args = Cli::parse();
    let node_id = Uuid::parse_str(&args.uuid).expect("Node id not specified");

    let grpc_cfg = GrpcConfig::from_url(node_id, args.endpoint)?;

    let (client, mut connection) = DeviceBuilder::new()
        .store(MemoryStore::new())
        .interface_directory("examples/client/interfaces")?
        .connect(grpc_cfg)
        .await?
        .build();

    let client_cl = client.clone();
    let receive_handle = tokio::spawn(async move {
        info!("start receiving messages from the Astarte Message Hub Server");
        loop {
            match client_cl.recv().await {
                Ok(event) => {
                    info!("received {event:?}");
                }
                Err(astarte_device_sdk::Error::Disconnected) => break,
                Err(err) => {
                    error!("error while receiving data from Astarte Message Hub Server: {err:?}")
                }
            }
        }
    });

    // Create a task to transmit
    let send_handle = tokio::spawn(async move {
        let now = time::SystemTime::now();
        let mut count = 0;
        // Consistent interval of 3 seconds
        let mut interval = tokio::time::interval(time::Duration::from_millis(args.time));

        while args.count.is_none() || Some(count) < args.count {
            interval.tick().await;

            info!("Publishing the uptime through the message hub.");

            let elapsed = now.elapsed().unwrap().as_secs();

            let elapsed_str = format!("Uptime for node {}: {}", args.uuid, elapsed);

            // TODO: restore the example and create proper checks for add/remove interfaces
            match count {
                0 => {
                    client
                        .send(
                            "org.astarte-platform.rust.examples.datastream.DeviceDatastream",
                            "/uptime",
                            elapsed_str.clone(),
                        )
                        .await
                        .expect("failed to send Astarte message");
                }
                1 => {
                    // Add the remaining interfaces
                    let additional_interfaces = read_additional_interfaces();
                    info!("adding {} interfaces", additional_interfaces.len());
                    client
                        .extend_interfaces(additional_interfaces)
                        .await
                        .expect("failed to extend interfaces");

                    info!("sending additional data");
                    client
                        .send(
                            "org.astarte-platform.rust.examples.datastream.AdditionalDeviceDatastream",
                            "/additional_uptime",
                            elapsed_str,
                        )
                        .await
                        .expect("failed to send Astarte message");
                    info!("sent additional data");
                }
                2 => {
                    info!("sending additional data");
                    client
                        .send(
                            "org.astarte-platform.rust.examples.datastream.AdditionalDeviceDatastream",
                            "/additional_uptime",
                            elapsed_str,
                        )
                        .await
                        .expect("failed to send Astarte message");
                    info!("sent additional data");
                }
                3 => {
                    // remove the added interfaces
                    let to_remove = [
                        "org.astarte-platform.rust.examples.datastream.AdditionalDeviceDatastream",
                    ]
                    .map(|i| i.to_string());
                    info!("removing {} interfaces", to_remove.len());
                    client
                        .remove_interfaces(to_remove)
                        .await
                        .expect("failed to extend interfaces");

                    info!("sending data to a removed interface");
                    let err = client
                        .send(
                            "org.astarte-platform.rust.examples.datastream.AdditionalDeviceDatastream",
                            "/additional_uptime",
                            elapsed_str,
                        )
                        .await.unwrap_err();
                    warn!("failed to send Astarte message: {err}");
                }
                _ => break,
            }

            count += 1;
        }

        info!("Done sending messages");
    });

    // wait for CTRL C to terminate the node execution
    select! {
        _ = ctrl_c() => {
            debug!("CTRL C received, stop sending/receiving data from Astarte");
            send_handle.abort();
            receive_handle.abort();
        }
        res = connection.handle_events() => {
            warn!("disconnected from Astarte");
            return res.map_err(Into::into);
        }
    }

    handle_task(receive_handle).await;
    handle_task(send_handle).await;

    connection.disconnect().await;

    Ok(())
}

async fn handle_task(h: JoinHandle<()>) {
    match h.await {
        Ok(()) => {}
        Err(err) if err.is_cancelled() => {}
        Err(err) => error!("join error: {err:?}"),
    };
}

fn read_additional_interfaces() -> Vec<Interface> {
    let iface_str = include_str!(
        "interfaces/additional/org.astarte-platform.rust.examples.datastream.AdditionalDeviceDatastream.json"
    );

    [iface_str]
        .into_iter()
        .map(|i| Interface::from_str(i).expect("failed to convert str to Interface"))
        .collect()
}
