use crate::{
    access::SubscriberAccessPolicy,
    config::{Config, ConfigError},
    handlers, migrations,
    services::{MessageLinkService, UserService},
};
use carapax::{
    access::{AccessExt, AccessRule, InMemoryAccessPolicy},
    longpoll::LongPoll,
    webhook,
    webhook::HyperError,
    Api, ApiError, App, Chain, Context,
};
use clap::{Parser, Subcommand};
use refinery::Error as MigrationError;
use std::{error::Error, fmt, sync::Arc};
use tokio::spawn;
use tokio_postgres::{connect as pg_connect, Client as PgClient, Error as PgError, NoTls as PgNoTls};

#[derive(Parser)]
#[clap(about, author, version)]
pub struct Arguments {
    /// Command to run
    #[clap(subcommand)]
    command: Command,
    /// Path to config
    config: String,
}

#[derive(Subcommand)]
pub enum Command {
    /// Run migrations
    Migrate,
    /// Start bot
    Start,
}

pub async fn run() -> Result<(), AppError> {
    let args = Arguments::parse();
    let config = Config::read_from_file(args.config).map_err(AppError::ReadConfig)?;

    let (mut pg_client, pg_connection) = pg_connect(&config.database_url, PgNoTls)
        .await
        .map_err(AppError::PgConnect)?;

    spawn(async move {
        if let Err(err) = pg_connection.await {
            log::error!("PostgreSQL connection error: {}", err);
        }
    });

    match args.command {
        Command::Migrate => {
            migrations::run(&mut pg_client).await.map_err(AppError::Migrate)?;
        }
        Command::Start => {
            start(config, pg_client).await?;
        }
    }

    Ok(())
}

async fn start(config: Config, pg_client: PgClient) -> Result<(), AppError> {
    let api = Api::new(&config.token).map_err(AppError::CreateApi)?;

    let pg_client = Arc::new(pg_client);
    let user_service = UserService::new(pg_client.clone());

    let admin_policy = InMemoryAccessPolicy::from(vec![AccessRule::allow_chat(config.chat_id)]);
    let subscriber_policy = SubscriberAccessPolicy::new(user_service.clone(), config.chat_id);

    let mut context = Context::default();
    context.insert(config.clone());
    context.insert(api.clone());
    context.insert(MessageLinkService::new(pg_client.clone()));
    context.insert(user_service);

    let chain = Chain::all()
        .add(handlers::middleware::setup())
        .add(handlers::admin::setup().access(admin_policy))
        .add(handlers::subscriber::setup().access(subscriber_policy));

    let app = App::new(context, chain);

    match config.webhook_address {
        Some(address) => {
            let path = config.webhook_path.unwrap_or_else(|| String::from("/"));
            webhook::run_server(address, path, app)
                .await
                .map_err(AppError::StartServer)?;
        }
        None => {
            LongPoll::new(api, app).run().await;
        }
    }

    Ok(())
}

#[derive(Debug)]
pub enum AppError {
    CreateApi(ApiError),
    Migrate(MigrationError),
    NoConfig,
    PgConnect(PgError),
    ReadConfig(ConfigError),
    StartServer(HyperError),
}

impl fmt::Display for AppError {
    fn fmt(&self, out: &mut fmt::Formatter) -> fmt::Result {
        use self::AppError::*;
        match self {
            CreateApi(err) => write!(out, "Could not create API client: {}", err),
            Migrate(err) => write!(out, "Migration error: {}", err),
            NoConfig => write!(out, "Path to configuration file is not provided"),
            PgConnect(err) => write!(out, "PostgreSQL: {}", err),
            ReadConfig(err) => write!(out, "{}", err),
            StartServer(err) => write!(out, "Could not start server for webhooks: {}", err),
        }
    }
}

impl Error for AppError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        use self::AppError::*;
        Some(match self {
            CreateApi(err) => err,
            Migrate(err) => err,
            NoConfig => return None,
            PgConnect(err) => err,
            ReadConfig(err) => err,
            StartServer(err) => err,
        })
    }
}
