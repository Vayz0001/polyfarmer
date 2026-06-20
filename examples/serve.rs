//! Preview the dashboard without booting the trading engine (no wallet needed).
//!
//!   cargo run --example serve
//!   → open http://127.0.0.1:8080  (first-run admin password is printed below)
//!
//! Uses a local `data/` dir for the credential store, like the real binary.

use std::sync::Arc;

use polyfarmer::creds::CredentialStore;
use polyfarmer::web::{router, WebState};

#[tokio::main]
async fn main() {
    let store = CredentialStore::open("data").expect("open credential store");
    if !store.is_initialized() {
        let pw = store.init_admin().expect("init admin");
        println!("\n  first-run admin password:  {pw}\n  (change it in the dashboard)\n");
    }
    let state = WebState::new(Arc::new(store));

    let bind = "127.0.0.1:8080";
    let listener = tokio::net::TcpListener::bind(bind).await.expect("bind");
    println!("dashboard preview → http://{bind}");
    axum::serve(listener, router(state)).await.expect("serve");
}
