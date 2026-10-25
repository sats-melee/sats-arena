//! Read-only NWC diagnosis. A URI arrives on stdin; never print wallet errors.
use nwc::{NostrWalletConnect, prelude::NostrWalletConnectUri};
use serde::Deserialize;
use std::{
    io::{self, Read},
    time::{Duration, Instant},
};
#[derive(Deserialize)]
struct Config {
    nwc_uri: String,
}
#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let mut input = String::new();
    io::stdin().take(16384).read_to_string(&mut input)?;
    let config: Config =
        serde_json::from_str(&input).map_err(|_| anyhow::anyhow!("Invalid input"))?;
    let uri = NostrWalletConnectUri::parse(&config.nwc_uri)
        .map_err(|_| anyhow::anyhow!("Invalid URI"))?;
    for cipher in ["negotiated", "nip44", "nip04"] {
        let builder = NostrWalletConnect::builder(uri.clone()).timeout(Duration::from_secs(30));
        let client = match cipher {
            "nip44" => builder.force_nip44_v2(),
            "nip04" => builder.force_nip04(),
            _ => builder,
        }
        .build();
        let start = Instant::now();
        let result =
            tokio::time::timeout(Duration::from_secs(35), async { client.get_info().await }).await;
        let status = match result {
            Ok(Ok(info)) => format!("reply received; methods={}", info.methods.len()),
            Ok(Err(error)) => format!("request failed ({:?})", error.kind()),
            Err(_) => "timeout".into(),
        };
        println!("{cipher}: {status}; {} ms", start.elapsed().as_millis());
        client.shutdown().await;
    }
    Ok(())
}
