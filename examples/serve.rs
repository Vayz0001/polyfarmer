//! Preview the dashboard without booting the trading engine (no creds needed).
//!
//!   cargo run --example serve
//!   → open http://127.0.0.1:8080
//!
//! Handy for building/iterating on the UI before the engine is wired in.

#[tokio::main]
async fn main() {
    let app = polyfarmer::web::router();
    let bind = "127.0.0.1:8080";
    let listener = tokio::net::TcpListener::bind(bind).await.expect("bind");
    println!("dashboard preview → http://{bind}");
    axum::serve(listener, app).await.expect("serve");
}
