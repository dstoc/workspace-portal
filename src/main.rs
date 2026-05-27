#[tokio::main]
async fn main() {
    if let Err(err) = workspace_portal::cli::run().await {
        eprintln!("{err}");
        std::process::exit(1);
    }
}
