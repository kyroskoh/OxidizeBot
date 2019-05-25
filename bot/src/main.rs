#![feature(async_await)]
#![recursion_limit = "128"]

use failure::{format_err, ResultExt};
use setmod_bot::{
    api, bus, config, db, features::Feature, irc, module, oauth2, obs, player, prelude::*, scopes,
    secrets, settings, template, utils, web,
};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

fn opts() -> clap::App<'static, 'static> {
    clap::App::new("SetMod Bot")
        .version(setmod_bot::VERSION)
        .author("John-John Tedro <udoprog@tedro.se>")
        .about("Bot component of SetMod.")
        .arg(
            clap::Arg::with_name("config")
                .short("c")
                .long("config")
                .value_name("file")
                .help("Configuration files to use.")
                .takes_value(true),
        )
        .arg(
            clap::Arg::with_name("web-root")
                .long("web-root")
                .value_name("dir")
                .help("Directory to use as web root.")
                .takes_value(true),
        )
}

fn default_log_config() -> Result<log4rs::config::Config, failure::Error> {
    use log::LevelFilter;
    use log4rs::{
        append::console::ConsoleAppender,
        config::{Appender, Config, Logger, Root},
    };

    let stdout = ConsoleAppender::builder().build();

    let config = Config::builder()
        .appender(Appender::builder().build("stdout", Box::new(stdout)))
        .logger(Logger::builder().build("setmod", LevelFilter::Warn))
        .build(Root::builder().appender("stdout").build(LevelFilter::Warn))?;

    Ok(config)
}

/// Configure logging.
fn setup_logs(root: &Path) -> Result<(), failure::Error> {
    let file = root.join("log4rs.yaml");

    if !file.is_file() {
        let config = default_log_config()?;
        log4rs::init_config(config)?;
        log::warn!(
            "Using default since log configuration is missing: {}",
            file.display()
        );
    } else {
        log4rs::init_file(file, Default::default())?;
    }

    Ok(())
}

fn main() -> Result<(), failure::Error> {
    let opts = opts();
    let m = opts.get_matches();

    let config = m
        .value_of("config")
        .map(Path::new)
        .unwrap_or(Path::new("config.toml"))
        .to_owned();

    let root = config
        .parent()
        .ok_or_else(|| format_err!("missing parent"))?
        .to_owned();

    let web_root = m.value_of("web-root").map(PathBuf::from);

    setup_logs(&root).context("failed to setup logs")?;

    let mut runtime = tokio::runtime::Runtime::new()?;
    let result = runtime.block_on(try_main(root, web_root, config).boxed().compat());

    match result {
        Err(e) => setmod_bot::log_err!(e, "bot crashed"),
        Ok(()) => log::info!("bot was shut down"),
    }

    Ok(())
}

async fn try_main(
    root: PathBuf,
    web_root: Option<PathBuf>,
    config: PathBuf,
) -> Result<(), failure::Error> {
    log::info!("Starting SetMod Version {}", setmod_bot::VERSION);

    let thread_pool = Arc::new(tokio_threadpool::ThreadPool::new());

    if !config.is_file() {
        failure::bail!("missing configuration: {}", config.display());
    }

    let config: config::Config = fs::read_to_string(&config)
        .map_err(failure::Error::from)
        .and_then(|s| toml::de::from_str(&s).map_err(failure::Error::from))
        .with_context(|_| format_err!("failed to read configuration: {}", config.display()))?;

    let config = Arc::new(config);

    let secrets_path = match config.secrets.as_ref() {
        Some(secrets) => secrets.to_path(&root),
        None => root.join("secrets.yml"),
    };

    let mut modules = Vec::new();

    for module in &config.modules {
        modules.push(module.load(&config)?);
    }

    let secrets = secrets::Secrets::open(&secrets_path)
        .with_context(|_| format_err!("failed to load secrets: {}", secrets_path.display()))?;

    let database_url = config
        .database_url
        .as_ref()
        .map(|d| d.as_str())
        .ok_or_else(|| format_err!("require `database_url`"))?;

    let db = db::Database::open(database_url, Arc::clone(&thread_pool))?;

    let scopes_schema = scopes::Schema::load_static()?;
    let scopes = db.scopes(scopes_schema)?;

    let settings_schema = settings::Schema::load_static()?;
    let settings = db.settings(settings_schema)?;

    let bad_words = db::Words::load(db.clone())?;
    let after_streams = db::AfterStreams::load(db.clone())?;
    let commands = db::Commands::load(db.clone())?;
    let aliases = db::Aliases::load(db.clone())?;
    let promotions = db::Promotions::load(db.clone())?;
    let themes = db::Themes::load(db.clone())?;

    // TODO: remove this migration next major release.
    if !config.aliases.is_empty() {
        log::warn!("# DEPRECATION WARNING");
        log::warn!("The [[aliases]] section in the configuration is now deprecated.");
        log::warn!(
            "Please remove it in favor of the !alias command which stores aliases in the database."
        );

        if !settings
            .get::<bool>("migration/aliases-migrated")?
            .unwrap_or_default()
        {
            log::warn!("Performing a one time migration of aliases from configuration.");

            for alias in &config.aliases {
                let template = template::Template::compile(&alias.replace)?;
                aliases.edit(config.irc.channel.as_str(), &alias.r#match, template)?;
            }

            settings.set("migration/aliases-migrated", true)?;
        }
    }

    if !config.themes.themes.is_empty() {
        log::warn!("# DEPRECATION WARNING");
        log::warn!("The [themes] section in the configuration is now deprecated.");
        log::warn!("Please remove it in favor of storing theme in the database.");

        if !settings
            .get::<bool>("migration/themes-migrated")?
            .unwrap_or_default()
        {
            log::warn!("Performing a one time migration of themes from configuration.");

            for (name, theme) in &config.themes.themes {
                let track_id = theme.track.clone();
                themes.edit(config.irc.channel.as_str(), name.as_str(), track_id)?;
                themes.edit_duration(
                    config.irc.channel.as_str(),
                    name.as_str(),
                    theme.offset.clone(),
                    theme.end.clone(),
                )?;
            }

            settings.set("migration/themes-migrated", true)?;
        }
    }

    if !config.whitelisted_hosts.is_empty() {
        log::warn!("# DEPRECATION WARNING");
        log::warn!("The `whitelisted_hosts` section in the configuration is now deprecated.");
        log::warn!("Please remove it in favor of the irc/whitelisted-hosts setting.");
    }

    if let Some(path) = config.bad_words.as_ref() {
        let path = path.to_path(&root);
        bad_words
            .load_from_path(&path)
            .with_context(|_| format_err!("failed to load bad words from: {}", path.display()))?;
    };

    let global_bus = Arc::new(bus::Bus::new());
    let youtube_bus = Arc::new(bus::Bus::new());

    let mut futures = Vec::<future::BoxFuture<'static, Result<(), failure::Error>>>::new();

    let (web, future) = web::setup(
        web_root.as_ref().map(|p| p.as_path()),
        config.clone(),
        global_bus.clone(),
        youtube_bus.clone(),
        after_streams.clone(),
        db.clone(),
        settings.clone(),
        scopes.clone(),
        aliases.clone(),
        commands.clone(),
        promotions.clone(),
        themes.clone(),
    )?;

    let future = future.map_err(|e| {
        log::error!("Error in web server: {}", e);
        ()
    });

    // NB: spawn the web server on a separate thread because it's needed for the synchronous authentication flow below.
    tokio::spawn(future.boxed().compat())
        .into_future()
        .compat()
        .await
        .map_err(|_| failure::format_err!("failed to spawn web server"))?;

    if settings.get::<bool>("first-run")?.unwrap_or(true) {
        log::info!("Opening {} for the first time", web::URL);

        if let Err(e) = webbrowser::open(web::URL) {
            log::error!("failed to open browser: {}", e);
        }

        settings.set("first-run", false)?;
    }

    log::info!("Listening on: {}", web::URL);

    let token_settings = settings.scoped(&["secrets", "oauth2"]);

    let (spotify_token, future) = {
        let flow = config::new_oauth2_flow::<config::Spotify>(
            web.clone(),
            "spotify",
            &token_settings,
            &secrets,
        )?
        .with_scopes(vec![
            String::from("playlist-read-collaborative"),
            String::from("playlist-read-private"),
            String::from("user-library-read"),
            String::from("user-modify-playback-state"),
            String::from("user-read-playback-state"),
        ])
        .build(String::from("Spotify"))?;

        flow.into_token()?
    };

    futures.push(future.boxed());

    let (youtube_token, future) = {
        let flow = oauth2::youtube(web.clone(), token_settings.scoped(&["youtube"]))?
            .with_scopes(vec![String::from(
                "https://www.googleapis.com/auth/youtube.readonly",
            )])
            .build(String::from("YouTube"))?;

        flow.into_token()?
    };

    futures.push(future.boxed());

    let (streamer_token, future) = {
        let flow = config::new_oauth2_flow::<config::Twitch>(
            web.clone(),
            "twitch-streamer",
            &token_settings,
            &secrets,
        )?
        .with_scopes(vec![
            String::from("channel_editor"),
            String::from("channel_read"),
            String::from("channel:read:subscriptions"),
        ])
        .build(String::from("Twitch Streamer"))?;

        flow.into_token()?
    };

    futures.push(future.boxed());

    let (bot_token, future) = {
        let flow = config::new_oauth2_flow::<config::Twitch>(
            web.clone(),
            "twitch-bot",
            &token_settings,
            &secrets,
        )?
        .with_scopes(vec![
            String::from("channel:moderate"),
            String::from("chat:edit"),
            String::from("chat:read"),
            String::from("clips:edit"),
        ])
        .build(String::from("Twitch Bot"))?;

        flow.into_token()?
    };

    futures.push(future.boxed());

    let (shutdown, shutdown_rx) = utils::Shutdown::new();

    let youtube = Arc::new(api::YouTube::new(youtube_token.clone())?);
    let spotify = Arc::new(api::Spotify::new(spotify_token.clone())?);
    let streamer_twitch = api::Twitch::new(streamer_token.clone())?;

    let player = match config.player {
        // Only setup if the song feature is enabled.
        Some(ref player) if config.features.test(Feature::Song) => {
            let (player, future) = player::run(
                db.clone(),
                spotify.clone(),
                youtube.clone(),
                config.clone(),
                player.clone(),
                global_bus.clone(),
                youtube_bus.clone(),
                settings.clone(),
                themes.clone(),
            )?;

            futures.push(future.boxed());

            if let Some(api_url) = config.api_url.as_ref() {
                futures.push(api::setbac::run(api_url, &player, streamer_token.clone())?.boxed());
            }

            web.set_player(player.client());

            // load the song module if we have a player configuration.
            let module = module::song::Config::default();
            modules.push(Box::new(module::song::Module::load(&module, &player)?));

            Some(player)
        }
        _ => None,
    };

    if config.features.test(Feature::Command) {
        let module = module::command_admin::Config::default();
        modules.push(Box::new(module::command_admin::Module::load(&module)?));
    }

    let module = module::admin::Config::default();
    modules.push(Box::new(module::admin::Module::load(&module)?));

    let module = module::alias_admin::Config::default();
    modules.push(Box::new(module::alias_admin::Module::load(&module)?));

    let module = module::theme_admin::Config::default();
    modules.push(Box::new(module::theme_admin::Module::load(&module)?));

    let mut obs = None;

    if let Some(config) = config.obs.clone() {
        let (client, future) = obs::setup(config)?;
        futures.push(future.boxed());
        obs = Some(client);
    }

    let currency = config
        .currency
        .clone()
        .map(|c| c.into_currency(db.clone(), streamer_twitch.clone()));

    let bot_twitch = api::Twitch::new(bot_token.clone())?;

    let irc = irc::Irc {
        db: db,
        youtube,
        streamer_twitch,
        bot_twitch,
        config,
        currency,
        token: bot_token,
        commands,
        aliases,
        promotions,
        themes,
        bad_words,
        after_streams,
        global_bus,
        modules,
        shutdown,
        settings,
        player,
        obs,
        scopes,
    };

    futures.push(irc.run().boxed());

    let stuff = async move { future::try_join_all(futures).await.map_err(Some) };

    let shutdown_rx = async move {
        match shutdown_rx.await {
            Ok(_) => Result::<(), Option<failure::Error>>::Err(None),
            Err(_) => Result::<(), Option<failure::Error>>::Err(None),
        }
    };

    let result = future::try_join(stuff, shutdown_rx).await;

    match result {
        Ok(_) => Ok(()),
        Err(Some(e)) => Err(e),
        // Shutting down cleanly.
        Err(None) => {
            log::info!("shutting down...");
            Ok(())
        }
    }
}
