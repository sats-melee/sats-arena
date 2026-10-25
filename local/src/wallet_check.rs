//! Standalone launcher checks. Never open a match ledger or send a payment.
use anyhow::{Result, anyhow};
use sats_arena_core::wallet::{InvoiceState, NwcWallet, Wallet};
use serde::Deserialize;
use serde_json::{Value, json};
use std::ffi::{CStr, c_char};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    version: u32,
    backend: String,
    nwc_uri: String,
}

async fn check(uri: &str, invoice: bool) -> Result<Value> {
    let wallet = NwcWallet::connect(uri).await?;
    let result = async {
        if invoice {
            let invoice = wallet
                .create_invoice(1, "Sats Arena connection test (unpaid)", 600)
                .await?;
            let state = wallet.invoice_state(&invoice.payment_hash).await?;
            if state != InvoiceState::Pending {
                return Err(anyhow!("Test invoice was not pending"));
            }
            Ok(
                json!({"ok":true,"message":"Invoice created and lookup confirmed. No payment sent.",
                "invoice":invoice.bolt11,"amount_sats":1,"expires_in_secs":600}),
            )
        } else {
            Ok(json!({"ok":true,"message":"Wallet connected • required methods available"}))
        }
    }
    .await;
    wallet.shutdown().await;
    result
}

/// Configuration arrives over private stdin in the executable, never argv/env.
/// Only bounded, credential-free JSON is returned to the launcher.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arena_wallet_check(
    config: *const c_char,
    invoice: bool,
    out: *mut c_char,
    size: u32,
) -> bool {
    if config.is_null() || out.is_null() || size < 2 {
        return false;
    }
    let result = std::panic::catch_unwind(|| -> Result<Value> {
        let bytes = unsafe { CStr::from_ptr(config) }.to_bytes();
        if bytes.len() > 16384 {
            return Err(anyhow!("Wallet configuration too large"));
        }
        let request: Request = serde_json::from_slice(bytes)
            .map_err(|_| anyhow!("Invalid wallet check configuration"))?;
        if request.version != 1 || request.backend != "nwc" {
            return Err(anyhow!("Wallet check requires an NWC connection"));
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        runtime.block_on(check(&request.nwc_uri, invoice))
    });
    let response = match result {
        Ok(Ok(value)) => value,
        Ok(Err(error)) => json!({"ok":false,"message":error.to_string()}),
        Err(_) => json!({"ok":false,"message":"Wallet check failed"}),
    }
    .to_string();
    if response.len() + 1 > size as usize {
        return false;
    }
    unsafe {
        std::ptr::copy_nonoverlapping(response.as_ptr(), out.cast(), response.len());
        *out.add(response.len()) = 0;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;
    #[test]
    fn malformed_or_invalid_credentials_are_not_echoed() {
        for input in [
            "private-secret",
            r#"{"version":1,"backend":"nwc","nwc_uri":"nostr+walletconnect://bad?secret=DO_NOT_DISCLOSE"}"#,
        ] {
            let input = CString::new(input).unwrap();
            let mut out = [0i8; 8192];
            assert!(unsafe {
                arena_wallet_check(input.as_ptr(), false, out.as_mut_ptr(), out.len() as u32)
            });
            let response = unsafe { CStr::from_ptr(out.as_ptr()) }.to_str().unwrap();
            assert!(!response.contains("DO_NOT_DISCLOSE"));
            assert!(!response.contains("private-secret"));
            assert!(
                !serde_json::from_str::<Value>(response).unwrap()["ok"]
                    .as_bool()
                    .unwrap()
            );
        }
    }
    #[test]
    fn rejects_invalid_output_buffers() {
        assert!(!unsafe { arena_wallet_check(std::ptr::null(), false, std::ptr::null_mut(), 0) });
    }
}
