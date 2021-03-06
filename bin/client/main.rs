use std::path::Path;
use std::sync::Arc;

use bindle::client::{Client, ClientError, Result};
use bindle::provider::ProviderError;
use bindle::standalone::{StandaloneRead, StandaloneWrite};
use bindle::{
    cache::{Cache, DumbCache},
    provider::Provider,
};

use clap::Clap;
use log::{info, warn};
use sha2::Digest;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

mod opts;

use opts::*;

#[tokio::main]
async fn main() -> std::result::Result<(), ClientError> {
    let opts = opts::Opts::parse();
    // TODO: Allow log level setting
    env_logger::init();

    let bindle_client = Client::new(&opts.server_url)?;
    let bindle_dir = opts
        .bindle_dir
        .unwrap_or_else(|| dirs::home_dir().unwrap().join(".bindle/bindles"));
    tokio::fs::create_dir_all(&bindle_dir).await?;
    let local = bindle::provider::file::FileProvider::new(
        bindle_dir,
        bindle::search::NoopEngine::default(),
    )
    .await;
    let proxy = bindle::proxy::Proxy::new(bindle_client.clone());
    let cache = DumbCache::new(proxy, local);

    match opts.subcmd {
        SubCommand::Info(info_opts) => {
            let inv = match info_opts.yanked {
                true => cache.get_invoice(info_opts.bindle_id),
                false => cache.get_yanked_invoice(info_opts.bindle_id),
            }
            .await
            .map_err(map_storage_error)?;
            tokio::io::stdout().write_all(&toml::to_vec(&inv)?).await?;
        }
        SubCommand::GetInvoice(gi_opts) => {
            let inv = match gi_opts.yanked {
                true => cache.get_invoice(&gi_opts.bindle_id),
                false => cache.get_yanked_invoice(&gi_opts.bindle_id),
            }
            .await
            .map_err(map_storage_error)?;
            tokio::fs::OpenOptions::new()
                .write(true)
                .create_new(true) // Make sure we aren't overwriting
                .open(&gi_opts.output)
                .await?
                .write_all(&toml::to_vec(&inv)?)
                .await?;
            println!(
                "Wrote invoice {} to {}",
                gi_opts.bindle_id,
                gi_opts.output.display()
            );
        }
        SubCommand::GetParcel(gp_opts) => get_parcel(cache, gp_opts).await?,
        SubCommand::Yank(yank_opts) => {
            bindle_client.yank_invoice(&yank_opts.bindle_id).await?;
            println!("Bindle {} yanked", yank_opts.bindle_id);
        }
        SubCommand::Search(search_opts) => {
            // TODO: Do we want to use the cache for searching?
            let matches = bindle_client.query_invoices(search_opts.into()).await?;
            tokio::io::stdout()
                .write_all(&toml::to_vec(&matches)?)
                .await?;
        }
        SubCommand::Get(get_opts) => get_all(cache, get_opts).await?,
        SubCommand::Push(push_opts) => push_all(bindle_client, push_opts).await?,
        SubCommand::PushInvoice(push_opts) => {
            let resp = bindle_client
                .create_invoice_from_file(push_opts.path)
                .await?;
            println!("Invoice {} created", resp.invoice.bindle.id);
        }
        SubCommand::PushFile(push_opts) => {
            let label =
                generate_label(&push_opts.path, push_opts.name, push_opts.media_type).await?;
            println!("Uploading file {} to server", push_opts.path.display());
            bindle_client
                .create_parcel_from_file(push_opts.bindle_id, &label.sha256, push_opts.path)
                .await?;
            println!("File successfully uploaded");
        }
        SubCommand::GenerateLabel(generate_opts) => {
            let label = generate_label(
                generate_opts.path,
                generate_opts.name,
                generate_opts.media_type,
            )
            .await?;
            println!("{}", toml::to_string_pretty(&label)?);
        }
    }

    Ok(())
}

async fn generate_label(
    file_path: impl AsRef<Path>,
    name: Option<String>,
    media_type: Option<String>,
) -> Result<bindle::Label> {
    let path = file_path.as_ref().to_owned();
    let mut file = tokio::fs::File::open(&path).await?;
    let media_type = media_type.unwrap_or_else(|| {
        mime_guess::from_path(&path)
            .first_or_octet_stream()
            .to_string()
    });
    info!("Using media type {}", media_type);
    // Note: Should be able to unwrap here because the file opening step would have
    // failed in conditions where this returns `None`
    let name = name.unwrap_or_else(|| path.file_name().unwrap().to_string_lossy().to_string());
    info!("Using name {}", name);
    let size = file.metadata().await?.len();
    let mut sha = bindle::async_util::AsyncSha256::new();
    tokio::io::copy(&mut file, &mut sha).await?;
    let result = sha.into_inner().expect("data lock error").finalize();

    Ok(bindle::Label {
        sha256: format!("{:x}", result),
        media_type,
        size,
        name,
        annotations: None, // TODO: allow annotations from command line
        ..bindle::Label::default()
    })
}

async fn get_parcel<C: Cache + Send + Sync + Clone>(cache: C, opts: GetParcel) -> Result<()> {
    let parcel = cache
        .get_parcel(opts.bindle_id, &opts.sha)
        .await
        .map_err(map_storage_error)?;
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true) // Make sure we aren't overwriting
        .open(&opts.output)
        .await?;
    tokio::io::copy(&mut bindle::async_util::BodyReadBuffer(parcel), &mut file).await?;
    println!("Wrote parcel {} to {}", opts.sha, opts.output.display());
    Ok(())
}

async fn push_all(client: Client, opts: Push) -> Result<()> {
    let standalone = StandaloneRead::new(opts.path, &opts.bindle_id).await?;
    standalone.push(&client).await?;
    println!("Pushed bindle {}", opts.bindle_id);
    Ok(())
}

async fn get_all<C: Cache + Send + Sync + Clone>(cache: C, opts: Get) -> Result<()> {
    let inv = match opts.yanked {
        true => cache.get_invoice(opts.bindle_id),
        false => cache.get_yanked_invoice(opts.bindle_id),
    }
    .await
    .map_err(map_storage_error)?;

    println!("Fetched invoice. Starting fetch of parcels");

    let parcels = Arc::new(Mutex::new(std::collections::HashMap::new()));
    let zero_vec = Vec::with_capacity(0);
    let is_export = opts.export.is_some();
    let parcel_fetch = inv
        .parcel
        .as_ref()
        .unwrap_or(&zero_vec)
        .iter()
        .map(|p| {
            (
                p.label.sha256.clone(),
                inv.bindle.id.clone(),
                cache.clone(),
                parcels.clone(),
            )
        })
        .map(|(sha, bindle_id, c, parcels)| async move {
            match c.get_parcel(bindle_id, &sha).await {
                Ok(p) => {
                    println!("Fetched parcel {}", sha);
                    if is_export {
                        parcels
                            .lock()
                            .await
                            .insert(sha, bindle::async_util::BodyReadBuffer(p));
                    }
                }
                Err(e) => {
                    match e {
                        ProviderError::NotFound => warn!("Parcel {} does not exist", sha),
                        ProviderError::ProxyError(err)
                            if matches!(err, ClientError::ParcelNotFound) =>
                        {
                            warn!("Parcel {} does not exist", sha)
                        }
                        // Only return an error if it isn't a not found error. By design, an invoice
                        // can contain parcels that don't yet exist
                        ProviderError::ProxyError(inner) => return Err(inner),
                        _ => {
                            return Err(ClientError::Other(format!(
                                "Unable to get parcel {}: {:?}",
                                sha, e
                            )))
                        }
                    }
                }
            }
            Ok(())
        });
    futures::future::join_all(parcel_fetch)
        .await
        .into_iter()
        .collect::<Result<Vec<_>>>()?;
    if let Some(p) = opts.export {
        let standalone = StandaloneWrite::new(p, &inv.bindle.id)?;
        standalone
            .write(
                inv,
                // All locks should be done at this point (as all futures exited), so panicing feels
                // right here as it is an unrecoverable condition
                Arc::try_unwrap(parcels)
                    .map_err(|_| ClientError::Other("Unexpected lock error".to_string()))
                    .unwrap()
                    .into_inner(),
            )
            .await?;
    }

    Ok(())
}

fn map_storage_error(e: ProviderError) -> ClientError {
    match e {
        ProviderError::Io(e) => ClientError::Io(e),
        ProviderError::ProxyError(inner) => inner,
        _ => ClientError::Other(format!("{:?}", e)),
    }
}
