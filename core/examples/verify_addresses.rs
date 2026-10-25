use sats_arena_core::wallet::{AddressVerifier, LightningAddress, LnurlClient};
#[tokio::main(flavor = "current_thread")]
async fn main() {
    for address in std::env::args().skip(1) {
        let result = match LightningAddress::parse(&address) {
            Ok(address) => LnurlClient::default().verify(&address).await,
            Err(error) => Err(error),
        };
        match result {
            Ok(report) => println!("{}", serde_json::to_string(&report).unwrap()),
            Err(error) => {
                eprintln!("{address}: {error:#}");
                std::process::exit(1);
            }
        }
    }
}
