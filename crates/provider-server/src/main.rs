#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Before `run`: everything it logs, including startup failures, needs a
    // subscriber already installed to be seen at all.
    provider_server::init_logging();
    provider_server::run().await
}
