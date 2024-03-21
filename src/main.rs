use anyhow::{anyhow, Result};
use clap::Parser;
use futures::TryStreamExt;
use futures::{future::ready, StreamExt};
use kube::client::Client;
use kube::runtime::reflector::store::Writer;
use kube::runtime::{reflector, watcher, watcher::Config, WatchStreamExt};

mod custom_resources;

#[derive(Parser, Debug)]
#[clap(author, version, about, long_about = None)]
struct Cli {
    /// api and version of the resource (e.g.: "networking.k8s.io/v1")
    #[clap(long)]
    apiversion: String,

    /// Kind of the resource (e.g: "Ingress")
    #[clap(long)]
    kind: String,

    /// namespace to be used
    #[clap(long)]
    namespace: Option<String>,

    /// query for the resource globally
    #[clap(long)]
    global: bool,
}

#[derive(Debug)]
struct KubeResource {
    pub resource: kube::api::ApiResource,
    pub namespaced: bool,
}

async fn build_api_resource(client: &Client, apiversion: &str, kind: &str) -> Result<KubeResource> {
    let resources_list = match apiversion {
        "v1" => client.list_core_api_resources(apiversion).await?,
        _ => client.list_api_group_resources(apiversion).await?,
    };

    let (group, version) = match apiversion {
        "v1" => ("", "v1"),
        _ => apiversion
            .split_once('/')
            .ok_or_else(|| anyhow!("cannot determine group and version for {apiversion}"))?,
    };

    let resource = resources_list
        .resources
        .iter()
        .find(|r| r.kind == kind)
        .ok_or_else(|| anyhow!("Cannot find resource {apiversion}/{kind}"))?
        .clone();

    Ok(KubeResource {
        resource: kube::api::ApiResource {
            group: group.to_string(),
            version: version.to_string(),
            api_version: apiversion.to_string(),
            kind: kind.to_string(),
            plural: resource.name,
        },
        namespaced: resource.namespaced,
    })
}

use futures::Stream;
use kube::runtime::reflector::store;
use kube::Resource;
use std::hash::Hash;
pub fn my_reflector<K, W>(mut writer: store::Writer<K>, stream: W) -> impl Stream<Item = W::Item>
where
    K: Resource + Clone,
    K::DynamicType: Eq + Hash + Clone,
    W: Stream<Item = watcher::Result<watcher::Event<K>>>,
{
    stream.inspect_ok(move |event| {
        match event {
            watcher::Event::Applied(_) => println!("apply"),
            watcher::Event::Deleted(_) => println!("deleted"),
            watcher::Event::Restarted(items) => println!("restarted {}", items.len()),
        }
        writer.apply_watcher_event(event)
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    if cli.namespace.is_some() && cli.global {
        return Err(anyhow!(
            "cannot specify a namespace and the global flag at the same time"
        ));
    }

    let client = Client::try_default().await?;

    let resource = build_api_resource(&client, &cli.apiversion, &cli.kind).await?;
    println!("{}/{} => {resource:?}", cli.apiversion, cli.kind);

    let api = if !cli.global && resource.namespaced {
        match cli.namespace {
            Some(namespace) => kube::api::Api::<kube::core::DynamicObject>::namespaced_with(
                client.clone(),
                &namespace,
                &resource.resource,
            ),
            None => return Err(anyhow!("No namespace provided for a namespaced resource")),
        }
    } else {
        kube::api::Api::<kube::core::DynamicObject>::all_with(client, &resource.resource)
    };

    let writer = Writer::new(resource.resource);
    let reader = writer.as_reader();
    let filter = Config::default();
    let stream = watcher(api, filter).map_ok(|ev| {
        ev.modify(|obj| {
            // clear managed fields to reduce memory usage
            obj.metadata.managed_fields = None;
        })
    });
    let rf = my_reflector(writer, stream);

    let infinite_watch = rf.default_backoff().touched_objects().for_each(|o| {
        dbg!(o);
        ready(())
    });
    infinite_watch.await;

    Ok(())
}
