mod api;
mod auth;
mod client_api;
mod config;
mod crypto;
mod db;
mod http_server;
mod models;
mod state;

use anyhow::Context;
use clap::Parser;
use config::Args;
use state::AppState;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    init_log(&args.log_path)?;

    let db = db::Database::connect(&args.db).await?;
    db.init().await?;
    db.ensure_admin().await?;
    db.ensure_tls_cert().await?;
    let (cert_pem, key_pem) = db.tls_pem().await?;

    let state = AppState::new(db, args.clone());
    let app = api::router(state.clone());
    let console_app = client_api::router(state);

    let web_addr = args
        .listen
        .parse()
        .with_context(|| format!("invalid --listen {}", args.listen))?;
    let console_addr = args
        .console_listen
        .parse()
        .with_context(|| format!("invalid --console-listen {}", args.console_listen))?;

    log::info!("vnt-hub web listen {}", web_addr);
    log::info!("vnt-hub client console listen {}", console_addr);

    tokio::try_join!(
        http_server::serve_auto_tls(web_addr, app, cert_pem.clone(), key_pem.clone()),
        http_server::serve_auto_tls(console_addr, console_app, cert_pem, key_pem),
    )?;
    Ok(())
}

fn init_log(log_path: &str) -> anyhow::Result<()> {
    if log_path == "console" {
        let config = log4rs::config::Config::builder()
            .appender(log4rs::config::Appender::builder().build(
                "stdout",
                Box::new(log4rs::append::console::ConsoleAppender::builder().build()),
            ))
            .build(
                log4rs::config::Root::builder()
                    .appender("stdout")
                    .build(log::LevelFilter::Info),
            )?;
        let _ = log4rs::init_config(config);
        return Ok(());
    }
    if log_path == "/dev/null" {
        return Ok(());
    }
    let _ = log4rs::init_file(log_path, Default::default());
    Ok(())
}
