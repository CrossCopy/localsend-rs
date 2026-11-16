#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Unconditional: reqwest is built with `rustls-no-provider` regardless of
    // the `https` feature, so a client constructed on any code path needs the
    // process default provider to already be installed.
    localsend_rs::crypto::ensure_crypto_provider();

    use clap::Parser;
    use localsend_rs::cli::Cli;

    // Parsing and dispatch both live in the library so the desktop app, which
    // forwards its unknown arguments here, runs exactly the same commands.
    Cli::parse().run().await
}
