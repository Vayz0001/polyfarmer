//! Thin binary entrypoint — all orchestration lives in [`polyfarmer::app::run`].

#[tokio::main]
async fn main() -> eyre::Result<()> {
    polyfarmer::app::run().await
}
