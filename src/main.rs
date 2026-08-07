#[tokio::main]
async fn main() -> anyhow::Result<std::process::ExitCode> {
    ytermusic::cli::run().await
}
