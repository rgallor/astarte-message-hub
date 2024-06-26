// This file is part of Astarte.
//
// Copyright 2024 SECO Mind Srl
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//
// SPDX-License-Identifier: Apache-2.0

use std::{env::VarError, future::Future, sync::Arc};

use astarte_device_sdk::{prelude::*, Value};
use eyre::{bail, ensure, eyre, Context, OptionExt};
use interfaces::ServerAggregate;
use tempfile::tempdir;
use tokio::{sync::Barrier, task::JoinSet};
use tracing::{debug, error, instrument};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};
use uuid::{uuid, Uuid};

use crate::{
    api::Api,
    device_sdk::{init_node, Node},
    interfaces::{
        DeviceAggregate, DeviceDatastream, DeviceProperty, ServerDatastream, ServerProperty,
        ENDPOINTS, INTERFACE_NAMES,
    },
    message_hub::{init_message_hub, MsgHub},
};

pub mod api;
pub mod device_sdk;
pub mod interfaces;
pub mod message_hub;
pub mod utils;

pub const GRPC_PORT: u16 = 50051;
pub const UUID: Uuid = uuid!("acc78dae-194c-4942-8f33-9f719629e316");

fn env_filter() -> eyre::Result<EnvFilter> {
    let filter = std::env::var("RUST_LOG").or_else(|err| match err {
        VarError::NotPresent => Ok(
            "e2e_test=trace,astarte_message_hub=debug,astarte_device_sdk=debug,tower_http=debug"
                .to_string(),
        ),
        err @ VarError::NotUnicode(_) => Err(err),
    })?;

    let env_filter = EnvFilter::try_new(filter)?;

    Ok(env_filter)
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    color_eyre::install()?;

    let filter = env_filter()?;
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer())
        .with(filter)
        .try_init()?;

    let dir = tempdir()?;

    let api = Api::try_from_env()?;

    let mut tasks = JoinSet::new();

    // Barrier to sync client and server
    let barrier = Arc::new(Barrier::new(2));

    let msghub = init_message_hub(dir.path(), barrier.clone(), &mut tasks).await?;
    let node = init_node(&barrier, &mut tasks).await?;

    tasks.spawn(async move { e2e_test(api, msghub, node, barrier).await });

    while let Some(res) = tasks.join_next().await {
        match res {
            Ok(res) => {
                res.wrap_err("task failed")?;
            }
            Err(err) if err.is_cancelled() => {}
            Err(err) => {
                return Err(err).wrap_err("couldn't join task");
            }
        }
    }

    Ok(())
}

/// Retry the future multiple times
async fn retry<F, T, U>(times: usize, mut f: F) -> eyre::Result<U>
where
    F: FnMut() -> T,
    T: Future<Output = eyre::Result<U>>,
{
    for i in 1..=times {
        match (f)().await {
            Ok(o) => return Ok(o),
            Err(err) => {
                error!("failed retry {i} for: {err}");

                tokio::task::yield_now().await
            }
        }
    }

    bail!("to many attempts")
}

#[instrument(skip_all)]
async fn e2e_test(
    api: Api,
    msghub: MsgHub,
    mut node: Node,
    barrier: Arc<Barrier>,
) -> eyre::Result<()> {
    // Check that the attach worked by checking the message hub interfaces
    retry(20, || async {
        let mut interfaces = api.interfaces().await?;
        interfaces.sort_unstable();

        debug!(?interfaces);

        ensure!(
            interfaces == INTERFACE_NAMES,
            "different number of interfaces"
        );

        Ok(())
    })
    .await?;

    // Send the device data
    send_device_data(&node, &api, &barrier).await?;

    // Receive the server data
    receive_server_data(&mut node, &api).await?;

    // Disconnect the message hub and cleanup
    node.close().await?;
    msghub.close();

    Ok(())
}

#[instrument(skip_all)]
async fn send_device_data(node: &Node, api: &Api, barrier: &Barrier) -> eyre::Result<()> {
    debug!("sending DeviceAggregate");
    node.client
        .send_object(
            DeviceAggregate::name(),
            DeviceAggregate::path(),
            DeviceAggregate::default(),
        )
        .await?;

    barrier.wait().await;

    retry(10, || async move {
        let data: DeviceAggregate = api
            .aggregate_value(DeviceAggregate::name(), DeviceAggregate::path())
            .await?
            .pop()
            .ok_or_else(|| eyre!("missing data from publish"))?;

        assert_eq!(data, DeviceAggregate::default());

        Ok(())
    })
    .await?;

    debug!("sending DeviceDatastream");
    let mut data = DeviceDatastream::default().astarte_aggregate()?;
    for &endpoint in ENDPOINTS {
        let value = data.remove(endpoint).ok_or_eyre("endpoint not found")?;

        node.client
            .send(DeviceDatastream::name(), &format!("/{endpoint}"), value)
            .await?;

        barrier.wait().await;
    }

    retry(10, || {
        let value = data.clone();

        async move {
            debug!("checking result");

            api.check_individual(DeviceDatastream::name(), &value)
                .await?;

            Ok(())
        }
    })
    .await?;

    debug!("sending DeviceProperty");
    let mut data = DeviceProperty::default().astarte_aggregate()?;
    for &endpoint in ENDPOINTS {
        let value = data.remove(endpoint).ok_or_eyre("endpoint not found")?;

        node.client
            .send(DeviceProperty::name(), &format!("/{endpoint}"), value)
            .await?;

        barrier.wait().await;
    }

    retry(10, || {
        let value = data.clone();

        async move {
            debug!("checking result");
            api.check_individual(DeviceProperty::name(), &value).await?;

            Ok(())
        }
    })
    .await?;

    debug!("unsetting DeviceProperty");
    let data = DeviceProperty::default().astarte_aggregate()?;
    for &endpoint in ENDPOINTS {
        ensure!(data.contains_key(endpoint), "endpoint not found");

        node.client
            .unset(DeviceProperty::name(), &format!("/{endpoint}"))
            .await?;

        barrier.wait().await;
    }

    retry(10, || async move {
        let data = api.property(DeviceProperty::name()).await?;
        ensure!(data.is_empty(), "property not unsetted {data:?}");

        Ok(())
    })
    .await?;

    Ok(())
}

#[instrument(skip_all)]
async fn receive_server_data(node: &mut Node, api: &Api) -> eyre::Result<()> {
    debug!("checking ServerAggregate");
    api.send_interface(
        ServerAggregate::name(),
        ServerAggregate::path(),
        ServerAggregate::default(),
    )
    .await?;

    let event = node.recv().await?;

    assert_eq!(event.interface, ServerAggregate::name());
    assert_eq!(event.path, ServerAggregate::path());

    let data = event.data.as_object().ok_or_eyre("not an object")?;
    assert_eq!(*data, ServerAggregate::default().astarte_aggregate()?);

    debug!("checking ServerDatastream");
    let data = ServerDatastream::default().astarte_aggregate()?;
    for (k, v) in data {
        api.send_individual(ServerDatastream::name(), &k, &v)
            .await?;

        let event = node.recv().await?;

        assert_eq!(event.interface, ServerDatastream::name());
        assert_eq!(event.path, format!("/{k}"));

        let data = event.data.as_individual().ok_or_eyre("not an object")?;
        assert_eq!(*data, v);
    }

    debug!("checking ServerProperty");
    let data = ServerProperty::default().astarte_aggregate()?;
    for (k, v) in data {
        api.send_individual(ServerProperty::name(), &k, &v).await?;

        let event = node.recv().await?;

        assert_eq!(event.interface, ServerProperty::name());
        assert_eq!(event.path, format!("/{k}"));

        let data = event.data.as_individual().ok_or_eyre("not an object")?;
        assert_eq!(*data, v);
    }

    debug!("checking unset for ServerProperty");
    let data = ServerProperty::default().astarte_aggregate()?;
    for k in data.keys() {
        api.unset(ServerProperty::name(), k).await?;

        let event = node.recv().await?;

        assert_eq!(event.interface, ServerProperty::name());
        assert_eq!(event.path, format!("/{k}"));

        assert_eq!(event.data, Value::Unset);
    }

    Ok(())
}
