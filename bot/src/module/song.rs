use crate::{
    command, currency, db, irc, module, player, stream_info, track_id, utils, utils::BoxFuture,
};
use chrono::Utc;
use futures::{future, Future, Stream as _};
use parking_lot::RwLock;
use std::sync::Arc;

const EXAMPLE_SEARCH: &'static str = "queen we will rock you";

/// Handler for the `!song` command.
pub struct Handler {
    pub db: db::Database,
    pub stream_info: Arc<RwLock<stream_info::StreamInfo>>,
    pub player: player::PlayerClient,
    pub request_help_cooldown: utils::Cooldown,
    pub subscriber_only: Arc<RwLock<bool>>,
    pub request_reward: Arc<RwLock<u32>>,
    pub spotify_max_duration: Arc<RwLock<utils::Duration>>,
    pub spotify_min_currency: Arc<RwLock<u32>>,
    pub spotify_subscriber_only: Arc<RwLock<bool>>,
    pub youtube_support: Arc<RwLock<bool>>,
    pub youtube_max_duration: Arc<RwLock<utils::Duration>>,
    pub youtube_min_currency: Arc<RwLock<u32>>,
    pub youtube_subscriber_only: Arc<RwLock<bool>>,
    pub currency: Option<Arc<currency::Currency>>,
}

impl Handler {
    fn handle_request(&mut self, ctx: &mut command::Context<'_, '_>) -> Result<(), failure::Error> {
        let q = ctx.rest();

        if !ctx.next().is_some() {
            self.request_help(ctx, None);
            return Ok(());
        }

        let youtube_support = *self.youtube_support.read();

        let future: BoxFuture<Option<track_id::TrackId>, failure::Error> =
            match track_id::TrackId::parse_with_urls(q) {
                Ok(track_id) => {
                    if !youtube_support && track_id.is_youtube() {
                        let e =
                            format!("YouTube song requests are currently not enabled, sorry :(");
                        self.request_help(ctx, Some(e.as_str()));
                        return Ok(());
                    }

                    Box::new(future::ok(Some(track_id)))
                }
                Err(e) => {
                    match e {
                        // NB: fall back to searching.
                        track_id::ParseTrackIdError::MissingUriPrefix => (),
                        // show other errors.
                        e => {
                            log::warn!("bad song request by {}: {}", ctx.user.name, e);
                            let e = format!("{} :(", e);
                            self.request_help(ctx, Some(e.as_str()));
                            return Ok(());
                        }
                    }

                    log::info!("Failed to parse as URL/URI: {}: {}", q, e);
                    Box::new(self.player.search_track(q))
                }
            };

        let future = future.map_err(Some);

        let future = future.and_then({
            let user = ctx.user.as_owned_user();

            move |track_id| match track_id {
                None => {
                    user.respond("Could not find a track matching your request, sorry :(");
                    return Err(None);
                }
                Some(track_id) => return Ok(track_id),
            }
        });

        let future = future.and_then({
            let stream_info = self.stream_info.clone();
            let subscriber_only = self.subscriber_only.clone();
            let youtube_subscriber_only = self.youtube_subscriber_only.clone();
            let spotify_subscriber_only = self.spotify_subscriber_only.clone();
            let user = ctx.user.as_owned_user();
            let is_moderator = ctx.is_moderator();

            move |track_id| {
                let (track_type, by_track_id) = match track_id {
                    track_id::TrackId::Spotify(..) => ("Spotify", *spotify_subscriber_only.read()),
                    track_id::TrackId::YouTube(..) => ("YouTube", *youtube_subscriber_only.read()),
                };

                let subscriber_only = by_track_id || *subscriber_only.read();

                if !subscriber_only || is_moderator {
                    return Ok(track_id);
                }

                if !stream_info.read().is_subscriber(&user.name) {
                    user.respond(format!(
                        "You must be a subscriber for {} requests, sorry :(",
                        track_type
                    ));
                    return Err(None);
                }

                Ok(track_id)
            }
        });

        let future = future
            .and_then({
                let is_moderator = ctx.is_moderator();
                let user = ctx.user.as_owned_user();
                let player = self.player.clone();
                let spotify_max_duration = self.spotify_max_duration.clone();
                let spotify_min_currency = self.spotify_min_currency.clone();
                let youtube_max_duration = self.youtube_max_duration.clone();
                let youtube_min_currency = self.youtube_min_currency.clone();
                let currency = self.currency.clone();

                move |track_id| {
                    let max_duration = match track_id {
                        player::TrackId::Spotify(_) => Some(spotify_max_duration.read().clone()),
                        player::TrackId::YouTube(_) => Some(youtube_max_duration.read().clone()),
                    };

                    let min_currency = match track_id {
                        player::TrackId::Spotify(_) => Some(spotify_min_currency.read().clone() as i64),
                        player::TrackId::YouTube(_) => Some(youtube_min_currency.read().clone() as i64),
                    };

                    let request = player.add_track(
                        &user.target, &user.name, track_id, is_moderator, max_duration, min_currency
                    );

                    request.then(move |result| {
                        match result {
                            Ok((pos, item)) => return Ok((pos, item)),
                            Err(player::AddTrackError::PlayerClosed(reason)) => {
                                match reason {
                                    Some(reason) => {
                                        user.respond(reason.as_str());
                                    },
                                    None => {
                                        user.respond("Player is closed from further requests, sorry :(");
                                    }
                                }
                            }
                            Err(player::AddTrackError::QueueContainsTrack(pos)) => {
                                user.respond(format!(
                                    "Player already contains that track (position #{pos}).",
                                    pos = pos + 1,
                                ));
                            }
                            Err(player::AddTrackError::TooManyUserTracks(count)) => {
                                match count {
                                    0 => {
                                        user.respond("Unfortunately you are not allowed to add tracks :(");
                                    }
                                    1 => {
                                        user.respond(
                                            "<3 your enthusiasm, but you already have a track in the queue.",
                                        );
                                    }
                                    count => {
                                        user.respond(format!(
                                            "<3 your enthusiasm, but you already have {count} tracks in the queue.",
                                            count = count,
                                        ));
                                    }
                                }
                            }
                            Err(player::AddTrackError::QueueFull) => {
                                user.respond("Player is full, try again later!");
                            }
                            Err(player::AddTrackError::Duplicate(when, who, limit)) => {
                                let duration = Utc::now().signed_duration_since(when);

                                let duration = match duration.to_std() {
                                    Err(_) => None,
                                    Ok(duration) => Some(utils::compact_duration(duration)),
                                };

                                let limit = utils::compact_duration(limit);

                                let who = match who {
                                    Some(ref who) if *who == user.name => String::from(" by you"),
                                    Some(ref who) => format!(" by {}", who),
                                    None => String::from(""),
                                };

                                let duration = match duration {
                                    Some(duration) => format!(" {} ago", duration),
                                    None => String::from(" not too long ago"),
                                };

                                user.respond(format!(
                                    "That song was requested{who}{duration}, \
                                    you have to wait at least {limit} between duplicate requests!",
                                    who = who,
                                    duration = duration,
                                    limit = limit,
                                ));
                            }
                            Err(player::AddTrackError::NotEnoughCurrency { balance, required }) => {
                                let currency = match currency {
                                    Some(currency) => currency.name.to_string(),
                                    None => String::from("currency"),
                                };

                                user.respond(format!(
                                    "You don't have enough {currency} to request songs. Need {required}, but you have {balance}, sorry :(",
                                    currency = currency,
                                    required = required,
                                    balance = balance,
                                ));
                            }
                            Err(player::AddTrackError::Error(e)) => {
                                return Err(Some(e));
                            }
                        }

                        Err(None)
                    })
                }
            });

        let future = future.map_err({
            let user = ctx.user.as_owned_user();

            move |e| {
                if let Some(e) = e {
                    user.respond("There was a problem adding your song :(");
                    log_err!(e, "error when adding song");
                }

                ()
            }
        });

        let future = future.and_then::<_, BoxFuture<(), ()>>({
            let currency = self.currency.clone();
            let request_reward = *self.request_reward.read();
            let db = self.db.clone();
            let user = ctx.user.as_owned_user();

            move |(pos, item)| {
                let currency = match currency.clone() {
                    Some(ref currency) if request_reward > 0 => currency.clone(),
                    _ => {
                        user.respond(format!(
                            "Added {what} at position #{pos}!",
                            what = item.what(),
                            pos = pos + 1
                        ));

                        return Box::new(future::ok(()));
                    }
                };

                let future = db.balance_add(&user.target, &user.name, request_reward as i64);

                let future = future.then(move |result| match result {
                    Ok(()) => {
                        user.respond(format!(
                            "Added {what} at position #{pos}, here's your {amount} {currency}!",
                            what = item.what(),
                            pos = pos + 1,
                            amount = request_reward,
                            currency = currency.name,
                        ));

                        Ok(())
                    }
                    Err(e) => {
                        log_err!(e, "failed to reward user for song request");
                        Ok(())
                    }
                });

                Box::new(future)
            }
        });

        ctx.spawn(future);
        Ok(())
    }

    /// Provide a help message instructing the user how to perform song requests.
    fn request_help(&mut self, ctx: &mut command::Context<'_, '_>, reason: Option<&str>) {
        if !self.request_help_cooldown.is_open() {
            if let Some(reason) = reason {
                ctx.respond(reason);
            }

            return;
        }

        let mut response = format!(
            "You can request a song from Spotify with \
                {prefix} <search>, like \"{prefix} {search}\". You can also use an URI or an URL if you feel adventurous PogChamp",
            prefix = ctx.alias.unwrap_or("!song request"),
            search = EXAMPLE_SEARCH,
        );

        if let Some(reason) = reason {
            response = format!("{}. {}", reason, response);
        }

        ctx.respond(response);
    }
}

impl command::Handler for Handler {
    fn handle<'m>(&mut self, mut ctx: command::Context<'_, 'm>) -> Result<(), failure::Error> {
        match ctx.next() {
            Some("theme") => {
                ctx.check_moderator()?;

                let name = match ctx.next() {
                    Some(name) => name,
                    None => {
                        ctx.respond(format!(
                            "expected {prefix} <name> to play a theme song",
                            prefix = ctx.alias.unwrap_or("!song theme")
                        ));
                        return Ok(());
                    }
                };

                let future = self.player.play_theme(name).then({
                    let user = ctx.user.as_owned_user();

                    move |r| {
                        match r {
                            Ok(()) => {}
                            Err(player::PlayThemeError::NoSuchTheme) => {
                                user.respond("No such theme :(");
                            }
                            Err(player::PlayThemeError::Error(e)) => {
                                user.respond("There was a problem adding your song :(");
                                log_err!(e, "failed to add song");
                            }
                        }

                        Ok(())
                    }
                });

                ctx.spawn(future);
            }
            Some("promote") => {
                ctx.check_moderator()?;

                let index = match ctx.next().and_then(|n| parse_queue_position(&ctx.user, n)) {
                    Some(index) => index,
                    None => return Ok(()),
                };

                if let Some(item) = self.player.promote_song(ctx.user.name, index) {
                    ctx.respond(format!("Promoted song to head of queue: {}", item.what()));
                } else {
                    ctx.respond("No such song to promote");
                }
            }
            Some("close") => {
                ctx.check_moderator()?;

                self.player.close(match ctx.rest() {
                    "" => None,
                    other => Some(other.to_string()),
                });
                ctx.respond("Closed player from further requests.");
            }
            Some("open") => {
                ctx.check_moderator()?;
                self.player.open();
                ctx.respond("Opened player for requests.");
            }
            Some("list") => {
                if let Some(api_url) = ctx.api_url {
                    ctx.respond(format!(
                        "You can find the queue at {}/player/{}",
                        api_url, ctx.streamer
                    ));
                    return Ok(());
                }

                let mut limit = 3usize;

                if let Some(n) = ctx.next() {
                    ctx.check_moderator()?;

                    if let Ok(n) = str::parse(n) {
                        limit = n;
                    }
                }

                let items = self.player.list();

                let has_more = match items.len() > limit {
                    true => Some(items.len() - limit),
                    false => None,
                };

                display_songs(&ctx.user, has_more, items.iter().take(limit).cloned());
            }
            Some("current") => match self.player.current() {
                Some(current) => {
                    let elapsed = utils::digital_duration(&current.elapsed());
                    let duration = utils::digital_duration(&current.duration());

                    if let Some(name) = current.item.user.as_ref() {
                        ctx.respond(format!(
                            "Current song: {}, requested by {} - {elapsed} / {duration} - {url}",
                            current.item.what(),
                            name,
                            elapsed = elapsed,
                            duration = duration,
                            url = current.item.track_id.url(),
                        ));
                    } else {
                        ctx.respond(format!(
                            "Current song: {} - {elapsed} / {duration} - {url}",
                            current.item.what(),
                            elapsed = elapsed,
                            duration = duration,
                            url = current.item.track_id.url(),
                        ));
                    }
                }
                None => {
                    ctx.respond("No song :(");
                }
            },
            Some("purge") => {
                ctx.check_moderator()?;
                self.player.purge()?;
                ctx.respond("Song queue purged.");
            }
            // print when your next song will play.
            Some("when") => {
                let (your, user) = match ctx.next() {
                    Some(user) => {
                        ctx.check_moderator()?;
                        (false, user)
                    }
                    None => (true, ctx.user.name),
                };

                let user = user.to_lowercase();

                match self
                    .player
                    .find(|item| item.user.as_ref().map(|u| *u == user).unwrap_or_default())
                {
                    Some((when, ref item)) if when.as_secs() == 0 => {
                        if your {
                            ctx.respond("Your song is currently playing cmonBruh");
                        } else {
                            ctx.respond(format!(
                                "{}'s song {} is currently playing",
                                user,
                                item.what()
                            ));
                        }
                    }
                    Some((when, item)) => {
                        let when = utils::compact_duration(when);

                        if your {
                            ctx.respond(format!("Your song {} will play in {}", item.what(), when));
                        } else {
                            ctx.respond(format!(
                                "{}'s song {} will play in {}",
                                user,
                                item.what(),
                                when
                            ));
                        }
                    }
                    None => {
                        if your {
                            ctx.respond("You don't have any songs in queue :(");
                        } else {
                            ctx.respond(format!("{} doesn't have any songs in queue :(", user));
                        }
                    }
                }
            }
            Some("delete") => {
                let removed = match ctx.next() {
                    Some("last") => match ctx.next() {
                        Some(last_user) => {
                            let last_user = last_user.to_lowercase();
                            ctx.check_moderator()?;
                            self.player.remove_last_by_user(&last_user)?
                        }
                        None => {
                            ctx.check_moderator()?;
                            self.player.remove_last()?
                        }
                    },
                    Some("mine") => self.player.remove_last_by_user(&ctx.user.name)?,
                    Some(n) => {
                        ctx.check_moderator()?;

                        let n = match parse_queue_position(&ctx.user, n) {
                            Some(n) => n,
                            None => return Ok(()),
                        };

                        self.player.remove_at(n)?
                    }
                    None => {
                        ctx.respond(format!("Expected: last, last <user>, or mine"));
                        return Ok(());
                    }
                };

                match removed {
                    None => ctx.respond("No song removed, sorry :("),
                    Some(item) => ctx.respond(format!("Removed: {}!", item.what())),
                }
            }
            Some("volume") => {
                match ctx.next() {
                    // setting volume
                    Some(other) => {
                        ctx.check_moderator()?;

                        let (diff, argument) = match other.chars().next() {
                            Some('+') => (Some(true), &other[1..]),
                            Some('-') => (Some(false), &other[1..]),
                            _ => (None, other),
                        };

                        let argument = match str::parse::<u32>(argument) {
                            Ok(argument) => argument,
                            Err(_) => {
                                ctx.respond("expected whole number argument");
                                return Ok(());
                            }
                        };

                        let argument = match diff {
                            Some(true) => self.player.current_volume().saturating_add(argument),
                            Some(false) => self.player.current_volume().saturating_sub(argument),
                            None => argument,
                        };

                        // clamp the volume.
                        let argument = u32::min(100, argument);
                        ctx.respond(format!("Volume set to {}.", argument));
                        self.player.volume(argument)?;
                    }
                    // reading volume
                    None => {
                        ctx.respond(format!("Current volume: {}.", self.player.current_volume()));
                    }
                }
            }
            Some("skip") => {
                ctx.check_moderator()?;
                self.player.skip()?;
            }
            Some("request") => {
                self.handle_request(&mut ctx)?;
            }
            Some("toggle") => {
                ctx.check_moderator()?;
                self.player.toggle()?;
            }
            Some("play") => {
                ctx.check_moderator()?;
                self.player.play()?;
            }
            Some("pause") => {
                ctx.check_moderator()?;
                self.player.pause()?;
            }
            Some("length") => {
                let (count, duration) = self.player.length();

                match count {
                    0 => ctx.respond("No songs in queue :("),
                    1 => {
                        let length = utils::long_duration(&duration);
                        ctx.respond(format!("One song in queue with {} of play time.", length));
                    }
                    count => {
                        let length = utils::long_duration(&duration);
                        ctx.respond(format!(
                            "{} songs in queue with {} of play time.",
                            count, length
                        ));
                    }
                }
            }
            None | Some(_) => {
                ctx.respond(format!(
                    "Expected argument to {prefix} command.",
                    prefix = ctx.alias.unwrap_or("!song"),
                ));
            }
        }

        Ok(())
    }
}

#[derive(Debug, serde::Deserialize)]
pub struct Config {
    #[serde(default = "default_cooldown")]
    help_cooldown: utils::Cooldown,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            help_cooldown: default_cooldown(),
        }
    }
}

fn default_cooldown() -> utils::Cooldown {
    utils::Cooldown::from_duration(utils::Duration::seconds(5))
}

pub struct Module {
    help_cooldown: utils::Cooldown,
    player: player::PlayerClient,
}

impl Module {
    pub fn load(module: &Config, player: &player::Player) -> Result<Self, failure::Error> {
        Ok(Module {
            help_cooldown: module.help_cooldown.clone(),
            player: player.client(),
        })
    }
}

impl module::Module for Module {
    fn ty(&self) -> &'static str {
        "song"
    }

    /// Set up command handlers for this module.
    fn hook(
        &self,
        module::HookContext {
            core,
            db,
            stream_info,
            irc_config,
            handlers,
            futures,
            sender,
            settings,
            currency,
            ..
        }: module::HookContext<'_>,
    ) -> Result<(), failure::Error> {
        let chat_feedback = settings.sync_var(core, "song/chat-feedback", true)?;

        futures.push(Box::new(player_feedback_loop(
            irc_config,
            self.player.clone(),
            sender.clone(),
            chat_feedback,
        )));

        let subscriber_only = settings.sync_var(core, "song/subscriber-only", false)?;

        let request_reward = settings.sync_var(core, "song/request-reward", 0)?;

        let spotify = settings.scoped(vec!["song", "spotify"]);
        let spotify_max_duration =
            spotify.sync_var(core, "max-duration", utils::Duration::seconds(60 * 10))?;

        let spotify_min_currency = spotify.sync_var(core, "min-currency", 60)?;
        let spotify_subscriber_only = spotify.sync_var(core, "subscriber-only", false)?;

        let youtube = settings.scoped(vec!["song", "youtube"]);
        let youtube_support = youtube.sync_var(core, "support", false)?;
        let youtube_max_duration =
            youtube.sync_var(core, "max-duration", utils::Duration::seconds(60 * 10))?;
        let youtube_min_currency = youtube.sync_var(core, "min-currency", 60)?;
        let youtube_subscriber_only = youtube.sync_var(core, "subscriber-only", true)?;

        handlers.insert(
            "song",
            Handler {
                db: db.clone(),
                stream_info: stream_info.clone(),
                request_help_cooldown: self.help_cooldown.clone(),
                player: self.player.clone(),
                subscriber_only,
                request_reward,
                spotify_max_duration,
                spotify_min_currency,
                spotify_subscriber_only,
                youtube_support,
                youtube_max_duration,
                youtube_min_currency,
                youtube_subscriber_only,
                currency: currency.cloned().map(Arc::new),
            },
        );
        Ok(())
    }
}

/// Parse a queue position.
fn parse_queue_position(user: &irc::User<'_>, n: &str) -> Option<usize> {
    match str::parse::<usize>(n) {
        Ok(0) => {
            user.respond("Can't mess with the current song :(");
            return None;
        }
        Ok(n) => Some(n.saturating_sub(1)),
        Err(_) => {
            user.respond("Expected whole number argument");
            return None;
        }
    }
}

/// Display the collection of songs.
fn display_songs(
    user: &irc::User<'_>,
    has_more: Option<usize>,
    it: impl IntoIterator<Item = Arc<player::Item>>,
) {
    let mut lines = Vec::new();

    for (index, item) in it.into_iter().enumerate() {
        match item.user.as_ref() {
            Some(user) => {
                lines.push(format!("#{}: {} ({user})", index, item.what(), user = user));
            }
            None => {
                lines.push(format!("#{}: {}", index, item.what()));
            }
        }
    }

    if lines.is_empty() {
        user.respond("Song queue is empty.");
        return;
    }

    if let Some(more) = has_more {
        user.respond(format!("{} ... and {} more.", lines.join("; "), more));
        return;
    }

    user.respond(format!("{}.", lines.join("; ")));
}

/// Notifications from the player.
fn player_feedback_loop(
    config: &irc::Config,
    player: player::PlayerClient,
    sender: irc::Sender,
    chat_feedback: Arc<RwLock<bool>>,
) -> impl Future<Item = (), Error = failure::Error> + Send + 'static {
    player
        .add_rx()
        .map_err(|e| failure::format_err!("failed to receive player update: {}", e))
        .for_each({
            let channel = config.channel.to_string();

            move |e| {
                match e {
                    player::Event::Detached => {
                        sender.privmsg(channel.as_str(), "Player is detached!");
                    }
                    player::Event::Playing(echo, item) => {
                        if !echo || !*chat_feedback.read() {
                            return Ok(());
                        }

                        let message = match item.user.as_ref() {
                            Some(user) => {
                                format!("Now playing: {}, requested by {}.", item.what(), user)
                            }
                            None => format!("Now playing: {}.", item.what(),),
                        };

                        sender.privmsg(channel.as_str(), message);
                    }
                    player::Event::Pausing => {
                        if !*chat_feedback.read() {
                            return Ok(());
                        }

                        sender.privmsg(channel.as_str(), "Pausing playback.");
                    }
                    player::Event::Empty => {
                        sender.privmsg(
                            channel.as_str(),
                            format!(
                                "Song queue is empty (use !song request <spotify-id> to add more).",
                            ),
                        );
                    }
                    player::Event::NotConfigured => {
                        sender.privmsg(channel.as_str(), "Player has not been configured yet!");
                    }
                    // other event we don't care about
                    _ => {}
                }

                Ok(())
            }
        })
}
