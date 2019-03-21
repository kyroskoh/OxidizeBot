use tokio_core::reactor::Core;

pub use crate::track_id::TrackId;
use crate::{config, current_song, db, secrets, spotify, themes::Themes, utils};

use chrono::Utc;
use failure::format_err;
use futures::{
    future,
    sync::{mpsc, oneshot},
    Async, Future, Poll, Stream,
};
use std::{
    collections::VecDeque,
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};
use tokio_bus::{Bus, BusReader};
use tokio_threadpool::{SpawnHandle, ThreadPool};

use librespot::core::spotify_id::SpotifyId;

mod connect;
mod native;

pub trait PlayerInterface: Send {
    /// Stop playing.
    fn stop(&mut self);

    /// Start playing.
    fn play(&mut self);

    /// Pause playback.
    fn pause(&mut self);

    /// Load the given track.
    ///
    /// The oneshot is triggered when the track has completed.
    fn load(&mut self, item: &Item, offset_ms: u32) -> oneshot::Receiver<()>;

    /// Adjust the volume of the player.
    fn volume(&mut self, volume: Option<f32>);
}

#[derive(Debug)]
pub enum PlayerEvent {
    Filtered,
}

type PlayerEventStream = Box<dyn Stream<Item = PlayerEvent, Error = ()> + Send + 'static>;

#[derive(Debug, serde::Deserialize)]
pub struct Config {
    #[serde(default = "default_max_queue_length")]
    max_queue_length: u32,
    #[serde(default = "default_max_songs_per_user")]
    max_songs_per_user: u32,
    /// Device to use with connect player.
    #[serde(default)]
    device: Option<String>,
    /// Speaker to use with native player.
    #[serde(default)]
    speaker: Option<String>,
    /// Playlist to fall back on. Will otherwise use the saved songs of the user.
    #[serde(default)]
    playlist: Option<String>,
    /// Volume of player.
    #[serde(default)]
    volume: Option<u32>,
    /// Whether or not to use the connect player.
    #[serde(default)]
    connect: bool,
    /// Whether or not to echo current song.
    #[serde(default = "default_true")]
    echo_current_song: bool,
}

fn default_true() -> bool {
    true
}

fn default_max_queue_length() -> u32 {
    30
}

fn default_max_songs_per_user() -> u32 {
    2
}

#[derive(Debug, Clone)]
pub struct Item {
    pub track_id: TrackId,
    pub artists: Vec<String>,
    pub name: String,
    pub user: Option<String>,
    pub duration: Duration,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ItemData<'a> {
    paused: bool,
    track_id: &'a TrackId,
    name: String,
    artists: Option<String>,
    user: Option<&'a str>,
    duration: String,
}

impl Item {
    /// Human readable version of playback item.
    pub fn what(&self) -> String {
        if let Some(artists) = utils::human_artists(&self.artists) {
            format!("\"{}\" by {}", self.name, artists)
        } else {
            format!("\"{}\"", self.name.to_string())
        }
    }

    /// Display the short form duration of this track.
    ///
    /// e.g. 4m32s
    pub fn duration(&self) -> String {
        utils::compact_time(self.duration.as_secs())
    }

    /// Get serializable data for this item.
    pub fn data(&self, paused: bool) -> Result<ItemData<'_>, failure::Error> {
        let artists = utils::human_artists(&self.artists);

        let name = htmlescape::decode_html(&self.name)
            .map_err(|_| format_err!("failed to decode song name: {}", self.name))?;

        Ok(ItemData {
            paused,
            track_id: &self.track_id,
            name,
            artists,
            user: self.user.as_ref().map(|s| s.as_str()),
            duration: self.duration(),
        })
    }
}

#[derive(Debug)]
pub enum Command {
    // Skip the current song.
    Skip,
    // Toggle playback.
    Toggle,
    // Pause playback.
    Pause,
    // Start playback.
    Play,
    // The queue was modified.
    Modified,
    // Set the gain of the player.
    Volume(u32),
    // Play the given item as a theme at the given offset.
    Inject(Arc<Item>, Duration),
}

impl std::str::FromStr for TrackId {
    type Err = failure::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        SpotifyId::from_base62(s)
            .map(TrackId)
            .map_err(|_| format_err!("failed to parse id"))
    }
}

/// Run the player.
pub fn run(
    core: &mut Core,
    db: db::Database,
    spotify: Arc<spotify::Spotify>,
    config: &config::Config,
    player_config: &Config,
    secrets: &secrets::Secrets,
) -> Result<(PlaybackFuture, Player), failure::Error> {
    let (commands_tx, commands) = mpsc::unbounded();

    let ((player, events), paused) = if player_config.connect {
        (connect::setup(core, player_config, spotify.clone())?, false)
    } else {
        (native::setup(core, player_config, secrets)?, true)
    };

    let bus = Arc::new(RwLock::new(Bus::new(1024)));

    let thread_pool = Arc::new(ThreadPool::new());
    let queue = Queue::new(db.clone());

    let fallback_items = match player_config.playlist.as_ref() {
        Some(playlist) => playlist_to_items(core, spotify.clone(), playlist)?,
        None => songs_to_items(core, spotify.clone())?,
    };

    // Add tracks from database.
    for song in db.list()? {
        queue.push_back_queue(core.run(convert_item(
            &thread_pool,
            spotify.clone(),
            song.user.clone(),
            song.track_id,
        ))?);
    }

    // Blank current song file if specified.
    if let Some(current_song) = config.current_song.as_ref() {
        if let Err(e) = current_song.blank() {
            log::warn!(
                "failed to blank current songs: {}: {}",
                current_song.path.display(),
                e
            );
        }
    }

    let volume = Arc::new(RwLock::new(u32::min(
        100u32,
        player_config.volume.unwrap_or(100u32),
    )));

    let current = Arc::new(RwLock::new(None));
    let closed = Arc::new(RwLock::new(None));

    let future = PlaybackFuture {
        player,
        events,
        commands,
        queue: queue.clone(),
        bus: bus.clone(),
        pop_front: None,
        paused,
        loaded: None,
        inject: None,
        sidelined: Default::default(),
        fallback_items,
        volume: Arc::clone(&volume),
        current: current.clone(),
        current_song: config.current_song.clone(),
        echo_current_song: player_config.echo_current_song,
    };

    let player = Player {
        queue,
        max_queue_length: player_config.max_queue_length,
        max_songs_per_user: player_config.max_songs_per_user,
        spotify,
        commands_tx,
        bus,
        volume: Arc::clone(&volume),
        current: current.clone(),
        themes: config.themes.clone(),
        closed: closed.clone(),
    };

    if player_config.connect {
        player.pause()?;

        if let Some(volume) = player_config.volume {
            player.volume(volume)?;
        }
    }

    Ok((future, player))
}

/// Convert a playlist into items.
fn playlist_to_items(
    core: &mut Core,
    spotify: Arc<spotify::Spotify>,
    playlist: &str,
) -> Result<Vec<Arc<Item>>, failure::Error> {
    let mut items = Vec::new();

    let playlist = core.run(spotify.playlist(playlist))?;

    for playlist_track in core.run(spotify.page_as_stream(playlist.tracks).concat2())? {
        let track = playlist_track.track;

        let track_id = TrackId(
            SpotifyId::from_base62(&track.id)
                .map_err(|_| format_err!("bad spotify id: {}", track.id))?,
        );

        let artists = track
            .artists
            .into_iter()
            .map(|a| a.name)
            .collect::<Vec<_>>();

        items.push(Arc::new(Item {
            track_id,
            artists,
            name: track.name.to_string(),
            user: None,
            duration: Duration::from_millis(track.duration_ms.into()),
        }));
    }

    Ok(items)
}

/// Convert all songs of a user into items.
fn songs_to_items(
    core: &mut Core,
    spotify: Arc<spotify::Spotify>,
) -> Result<Vec<Arc<Item>>, failure::Error> {
    let mut items = Vec::new();

    for added_song in core.run(spotify.my_tracks_stream().concat2())? {
        let track = added_song.track;

        let track_id = TrackId(
            SpotifyId::from_base62(&track.id)
                .map_err(|_| format_err!("bad spotify id: {}", track.id))?,
        );

        let artists = track
            .artists
            .into_iter()
            .map(|a| a.name)
            .collect::<Vec<_>>();

        items.push(Arc::new(Item {
            track_id,
            artists,
            name: track.name.to_string(),
            user: None,
            duration: Duration::from_millis(track.duration_ms.into()),
        }));
    }

    Ok(items)
}

/// Converts a track into an Item.
fn convert_item(
    thread_pool: &ThreadPool,
    spotify: Arc<spotify::Spotify>,
    user: Option<String>,
    track_id: TrackId,
) -> impl Future<Item = Arc<Item>, Error = failure::Error> {
    let track_id_string = track_id.0.to_base62();

    thread_pool
        .spawn_handle(future::lazy(move || spotify.track(&track_id_string)))
        .map(move |full_track| {
            let artists = full_track
                .artists
                .into_iter()
                .map(|a| a.name)
                .collect::<Vec<_>>();

            Arc::new(Item {
                track_id,
                artists,
                name: full_track.name,
                user,
                duration: Duration::from_millis(full_track.duration_ms.into()),
            })
        })
}

/// The origin of a song being played.
#[derive(Debug, Clone, Copy)]
pub enum Origin {
    Injected,
    Fallback,
    Queue,
}

/// Events emitted by the player.
#[derive(Debug, Clone)]
pub enum Event {
    Empty,
    Playing(bool, Origin, Arc<Item>),
    Pausing,
    /// queue was modified in some way.
    Modified,
}

/// A handler for the player.
#[derive(Clone)]
pub struct Player {
    queue: Queue,
    max_queue_length: u32,
    max_songs_per_user: u32,
    spotify: Arc<spotify::Spotify>,
    commands_tx: mpsc::UnboundedSender<Command>,
    bus: Arc<RwLock<Bus<Event>>>,
    volume: Arc<RwLock<u32>>,
    /// Current song that is loaded.
    current: Arc<RwLock<Option<Arc<Item>>>>,
    /// Theme songs.
    themes: Arc<Themes>,
    /// Player is closed for more requests.
    closed: Arc<RwLock<Option<Option<Arc<String>>>>>,
}

impl Player {
    /// The client components of the player.
    pub fn client(&self) -> PlayerClient {
        PlayerClient {
            queue: self.queue.clone(),
            thread_pool: Arc::new(ThreadPool::new()),
            max_queue_length: self.max_queue_length,
            max_songs_per_user: self.max_songs_per_user,
            spotify: self.spotify.clone(),
            commands_tx: self.commands_tx.clone(),
            volume: Arc::clone(&self.volume),
            current: self.current.clone(),
            themes: self.themes.clone(),
            closed: self.closed.clone(),
        }
    }

    /// Get a receiver for player events.
    pub fn add_rx(&self) -> BusReader<Event> {
        self.bus.write().expect("poisoned").add_rx()
    }

    /// Pause playback.
    pub fn pause(&self) -> Result<(), failure::Error> {
        self.send(Command::Pause)
    }

    /// Update volume of the player.
    pub fn volume(&self, volume: u32) -> Result<(), failure::Error> {
        self.send(Command::Volume(u32::min(100u32, volume)))
    }

    /// Send the given command.
    fn send(&self, command: Command) -> Result<(), failure::Error> {
        self.commands_tx
            .unbounded_send(command)
            .map_err(|_| format_err!("failed to send command"))
    }
}

/// All parts of a Player that can be shared between threads.
#[derive(Clone)]
pub struct PlayerClient {
    queue: Queue,
    thread_pool: Arc<ThreadPool>,
    max_queue_length: u32,
    max_songs_per_user: u32,
    spotify: Arc<spotify::Spotify>,
    commands_tx: mpsc::UnboundedSender<Command>,
    /// Current volume.
    volume: Arc<RwLock<u32>>,
    /// Current song that is loaded.
    current: Arc<RwLock<Option<Arc<Item>>>>,
    /// Theme songs.
    themes: Arc<Themes>,
    /// Player is closed for more requests.
    closed: Arc<RwLock<Option<Option<Arc<String>>>>>,
}

impl PlayerClient {
    /// Send the given command.
    fn send(&self, command: Command) -> Result<(), failure::Error> {
        self.commands_tx
            .unbounded_send(command)
            .map_err(|_| format_err!("failed to send command"))
    }

    /// Get the next N songs in queue.
    pub fn list(&self) -> Vec<Arc<Item>> {
        let current = self.current.read().expect("poisoned");
        let queue = self.queue.queue.read().expect("poisoned");

        current
            .iter()
            .cloned()
            .chain(queue.iter().cloned())
            .collect()
    }

    /// Promote the given song to the head of the queue.
    pub fn promote_song(&self, user: &str, n: usize) -> Option<Arc<Item>> {
        let promoted = self.queue.promote_song(user, n);

        if promoted.is_some() {
            self.modified();
        }

        promoted
    }

    /// Toggle playback.
    pub fn toggle(&self) -> Result<(), failure::Error> {
        self.send(Command::Toggle)
    }

    /// Start playback.
    pub fn play(&self) -> Result<(), failure::Error> {
        self.send(Command::Play)
    }

    /// Pause playback.
    pub fn pause(&self) -> Result<(), failure::Error> {
        self.send(Command::Pause)
    }

    /// Skip the current song.
    pub fn skip(&self) -> Result<(), failure::Error> {
        self.send(Command::Skip)
    }

    /// Update volume of the player.
    pub fn volume(&self, volume: u32) -> Result<(), failure::Error> {
        self.send(Command::Volume(u32::min(100u32, volume)))
    }

    /// Get the current volume.
    pub fn current_volume(&self) -> u32 {
        *self.volume.read().expect("poisoned")
    }

    /// Close the player from more requests.
    pub fn close(&self, reason: Option<String>) {
        *self.closed.write().expect("poisoned") = Some(reason.map(Arc::new));
    }

    /// Open the player.
    pub fn open(&self) {
        *self.closed.write().expect("poisoned") = None;
    }

    /// Search for a track.
    pub fn search_track(
        &self,
        q: &str,
    ) -> impl Future<Item = Option<TrackId>, Error = failure::Error> {
        self.spotify
            .search_track(q)
            .and_then(|page| match page.items.into_iter().next() {
                Some(track) => match SpotifyId::from_base62(&track.id) {
                    Ok(track_id) => Ok(Some(TrackId(track_id))),
                    Err(_) => Err(failure::format_err!("search result returned malformed id")),
                },
                None => Ok(None),
            })
    }

    /// Play a theme track.
    pub fn play_theme(&self, name: &str) -> impl Future<Item = (), Error = PlayThemeError> {
        let fut = future::lazy({
            let themes = self.themes.clone();
            let name = name.to_string();

            move || match themes.lookup(&name) {
                Some(theme) => Ok(theme),
                None => Err(PlayThemeError::NoSuchTheme),
            }
        });

        let fut = fut.and_then({
            let thread_pool = Arc::clone(&self.thread_pool);
            let spotify = Arc::clone(&self.spotify);

            move |theme| {
                convert_item(&thread_pool, spotify, None, theme.track.clone())
                    .map(move |item| (item, theme))
                    .map_err(|e| PlayThemeError::Error(e.into()))
            }
        });

        fut.and_then({
            let commands_tx = self.commands_tx.clone();

            move |(item, theme)| {
                let duration = theme.offset.as_duration();

                commands_tx
                    .unbounded_send(Command::Inject(item, duration))
                    .map_err(|e| PlayThemeError::Error(e.into()))
            }
        })
    }

    /// Add the given track to the queue.
    ///
    /// Returns the item added.
    pub fn add_track(
        &self,
        user: &str,
        track_id: TrackId,
        is_moderator: bool,
    ) -> impl Future<Item = (usize, Arc<Item>), Error = AddTrackError> {
        // invariant checks
        let fut = future::lazy({
            let queue = self.queue.queue.clone();
            let max_queue_length = self.max_queue_length;
            let max_songs_per_user = self.max_songs_per_user;
            let closed = self.closed.clone();
            let user = user.to_string();
            let track_id = track_id.clone();

            move || {
                let q = queue.read().expect("poisoned");

                let len = q.len();

                if !is_moderator {
                    if let Some(reason) = closed.read().expect("poisoned").as_ref() {
                        return Err(AddTrackError::PlayerClosed(reason.clone()));
                    }
                }

                // NB: moderator is allowed to violate max queue length.
                if !is_moderator && len > max_queue_length as usize {
                    return Err(AddTrackError::QueueFull);
                }

                let mut user_count = 0;

                for (index, i) in q.iter().enumerate() {
                    if i.track_id == track_id {
                        return Err(AddTrackError::QueueContainsTrack(index));
                    }

                    if i.user.as_ref().map(|u| *u == user).unwrap_or_default() {
                        user_count += 1;
                    }
                }

                // NB: moderator is allowed to add more songs.
                if !is_moderator && user_count >= max_songs_per_user {
                    return Err(AddTrackError::TooManyUserTracks(max_songs_per_user));
                }

                Ok(len)
            }
        });

        let fut = fut.and_then({
            let user = user.to_string();
            let thread_pool = Arc::clone(&self.thread_pool);
            let spotify = Arc::clone(&self.spotify);

            move |len| {
                convert_item(&thread_pool, spotify, Some(user), track_id)
                    .map(move |item| (len, item))
                    .map_err(|e| AddTrackError::Error(e.into()))
            }
        });

        let fut = fut.and_then({
            let queue = self.queue.clone();

            move |(len, item)| {
                queue
                    .push_back(item.clone())
                    .map(move |_| (len, item))
                    .map_err(|e| AddTrackError::Error(e.into()))
            }
        });

        fut.and_then({
            let commands_tx = self.commands_tx.clone();

            move |(len, item)| {
                commands_tx
                    .unbounded_send(Command::Modified)
                    .map(move |_| (len, item))
                    .map_err(|e| AddTrackError::Error(e.into()))
            }
        })
    }

    /// Remove the first track in the queue.
    pub fn remove_first(&self) -> Result<Option<Arc<Item>>, failure::Error> {
        Ok(None)
    }

    pub fn purge(&self) -> Result<Vec<Arc<Item>>, failure::Error> {
        let purged = self.queue.purge()?;

        if !purged.is_empty() {
            self.modified();
        }

        Ok(purged)
    }

    /// Remove the item at the given position.
    pub fn remove_at(&self, n: usize) -> Result<Option<Arc<Item>>, failure::Error> {
        let removed = self.queue.remove_at(n)?;

        if removed.is_some() {
            self.modified();
        }

        Ok(removed)
    }

    /// Remove the first track in the queue.
    pub fn remove_last(&self) -> Result<Option<Arc<Item>>, failure::Error> {
        let removed = self.queue.remove_last()?;

        if removed.is_some() {
            self.modified();
        }

        Ok(removed)
    }

    /// Remove the last track by the given user.
    pub fn remove_last_by_user(&self, user: &str) -> Result<Option<Arc<Item>>, failure::Error> {
        let removed = self.queue.remove_last_by_user(user)?;

        if removed.is_some() {
            self.modified();
        }

        Ok(removed)
    }

    /// Get the length in number of items and total number of seconds in queue.
    pub fn length(&self) -> (usize, u64) {
        let mut count = 0;
        let mut duration = Duration::default();

        if let Some(item) = self.current.read().expect("poisoned").as_ref() {
            duration += item.duration;
            count += 1;
        }

        let queue = self.queue.queue.read().expect("poisoned");

        for item in &*queue {
            duration += item.duration;
        }

        count += queue.len();
        (count, duration.as_secs())
    }

    /// Get the current song, if it is set.
    pub fn current(&self) -> Option<Arc<Item>> {
        self.current.read().expect("poisoned").clone()
    }

    /// Indicate that the queue has been modified.
    fn modified(&self) {
        if let Err(e) = self.commands_tx.unbounded_send(Command::Modified) {
            log::error!("failed to send queue modified notification: {}", e);
        }
    }
}

/// Error raised when failing to play a theme song.
pub enum PlayThemeError {
    /// No such theme song.
    NoSuchTheme,
    /// Other generic error happened.
    Error(failure::Error),
}

/// Error raised when trying to add track.
pub enum AddTrackError {
    /// Queue is full.
    QueueFull,
    /// Queue already contains track.
    QueueContainsTrack(usize),
    /// Too many user tracks.
    TooManyUserTracks(u32),
    /// Player has been closed from adding more tracks to the queue with an optional reason.
    PlayerClosed(Option<Arc<String>>),
    /// Other generic error happened.
    Error(failure::Error),
}

/// The backend of a words store.
pub trait Backend: Clone + Send + Sync {
    /// List all counters in backend.
    fn list(&self) -> Result<Vec<db::Song>, failure::Error>;

    /// Insert the given song into the backend.
    fn push_back(&self, song: &db::AddSong) -> Result<(), failure::Error>;

    /// Remove the song, but only log on issues.
    fn remove_song_log(&self, track_id: &TrackId) {
        match self.remove_song(track_id) {
            Err(e) => log::warn!(
                "{}: failed to remove song from database: {}",
                track_id.to_base62(),
                e
            ),
            Ok(false) => log::warn!("{}: no songs removed from database", track_id.to_base62()),
            Ok(true) => {}
        }
    }

    /// Remove the song with the given ID.
    fn remove_song(&self, track_id: &TrackId) -> Result<bool, failure::Error>;

    /// Purge the songs database, but only log on issues.
    fn song_purge_log(&self) -> Option<usize> {
        match self.song_purge() {
            Err(e) => {
                log::warn!("failed to purge songs from database: {}", e);
                None
            }
            Ok(n) => Some(n),
        }
    }

    /// Purge the songs database and return the number of items removed.
    fn song_purge(&self) -> Result<usize, failure::Error>;

    /// Purge the songs database, but only log on issues.
    fn promote_song_log(&self, user: &str, track_id: &TrackId) -> Option<bool> {
        match self.promote_song(user, track_id) {
            Err(e) => {
                log::warn!(
                    "failed to promote song `{}` in database: {}",
                    track_id.to_base62(),
                    e
                );
                None
            }
            Ok(n) => Some(n),
        }
    }

    /// Promote the track with the given ID.
    fn promote_song(&self, user: &str, track_id: &TrackId) -> Result<bool, failure::Error>;
}

/// The playback queue.
#[derive(Clone)]
struct Queue {
    db: db::Database,
    queue: Arc<RwLock<VecDeque<Arc<Item>>>>,
    thread_pool: Arc<ThreadPool>,
}

impl Queue {
    /// Construct a new queue.
    pub fn new(db: db::Database) -> Self {
        Self {
            db,
            queue: Arc::new(RwLock::new(Default::default())),
            thread_pool: Arc::new(ThreadPool::new()),
        }
    }

    /// Get the front of the queue.
    pub fn front(&self) -> Option<Arc<Item>> {
        self.queue.read().expect("poisoned").front().cloned()
    }

    /// Pop the front of the queue.
    pub fn pop_front(&self) -> PopFrontFuture {
        let db = self.db.clone();
        let queue = self.queue.clone();

        PopFrontFuture(self.thread_pool.spawn_handle(future::lazy(move || {
            if let Some(item) = queue.write().expect("poisoned").pop_front() {
                db.remove_song_log(&item.track_id);
            }

            Ok(None)
        })))
    }

    /// Push item to back of queue.
    pub fn push_back(&self, item: Arc<Item>) -> PushBackFuture {
        let db = self.db.clone();
        let queue = self.queue.clone();

        PushBackFuture(self.thread_pool.spawn_handle(future::lazy(move || {
            db.push_back(&db::AddSong {
                track_id: item.track_id.clone(),
                added_at: Utc::now().naive_utc(),
                user: item.user.clone(),
            })?;

            queue.write().expect("poisoned").push_back(item);
            Ok(())
        })))
    }

    /// Purge the song queue.
    pub fn purge(&self) -> Result<Vec<Arc<Item>>, failure::Error> {
        let mut q = self.queue.write().expect("poisoned");

        if q.is_empty() {
            return Ok(vec![]);
        }

        let purged = std::mem::replace(&mut *q, VecDeque::new())
            .into_iter()
            .collect();
        self.db.song_purge_log();
        Ok(purged)
    }

    /// Remove the item at the given position.
    pub fn remove_at(&self, n: usize) -> Result<Option<Arc<Item>>, failure::Error> {
        let mut q = self.queue.write().expect("poisoned");

        if q.is_empty() {
            return Ok(None);
        }

        if let Some(item) = q.remove(n) {
            self.db.remove_song_log(&item.track_id);
            return Ok(Some(item));
        }

        Ok(None)
    }

    /// Remove the last element.
    pub fn remove_last(&self) -> Result<Option<Arc<Item>>, failure::Error> {
        let mut q = self.queue.write().expect("poisoned");

        if q.is_empty() {
            return Ok(None);
        }

        if let Some(item) = q.pop_back() {
            self.db.remove_song_log(&item.track_id);
            return Ok(Some(item));
        }

        Ok(None)
    }

    /// Remove the last element by user.
    pub fn remove_last_by_user(&self, user: &str) -> Result<Option<Arc<Item>>, failure::Error> {
        let mut q = self.queue.write().expect("poisoned");

        if q.is_empty() {
            return Ok(None);
        }

        if let Some(position) = q
            .iter()
            .rposition(|i| i.user.as_ref().map(|u| u == user).unwrap_or_default())
        {
            if let Some(item) = q.remove(position) {
                self.db.remove_song_log(&item.track_id);
                return Ok(Some(item));
            }
        }

        Ok(None)
    }

    /// Promote the given song.
    pub fn promote_song(&self, user: &str, n: usize) -> Option<Arc<Item>> {
        let mut q = self.queue.write().expect("poisoned");

        // OK, but song doesn't exist or index is out of bound.
        if q.is_empty() || n >= q.len() {
            return None;
        }

        q.swap(0, n);

        if let Some(item) = q.get(0).cloned() {
            self.db.promote_song_log(user, &item.track_id);
            return Some(item);
        }

        None
    }

    /// Push item to back of queue without going through the database.
    fn push_back_queue(&self, item: Arc<Item>) {
        self.queue.write().expect("poisoned").push_back(item);
    }
}

struct PopFrontFuture(SpawnHandle<Option<Arc<Item>>, failure::Error>);

impl Future for PopFrontFuture {
    type Item = Option<Arc<Item>>;
    type Error = failure::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        self.0.poll()
    }
}

struct PushBackFuture(SpawnHandle<(), failure::Error>);

impl Future for PushBackFuture {
    type Item = ();
    type Error = failure::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        self.0.poll()
    }
}

struct Loaded {
    origin: Origin,
    item: Arc<Item>,
    future: oneshot::Receiver<()>,
    started_at: Instant,
    offset: Duration,
}

impl Loaded {
    /// Create a new loaded entry recording the time at which it was started.
    pub fn new(origin: Origin, item: Arc<Item>, future: oneshot::Receiver<()>) -> Self {
        Self {
            origin,
            item,
            future,
            started_at: Instant::now(),
            offset: Default::default(),
        }
    }

    /// Song was loaded with the specified offset.
    pub fn with_offset(self, offset: Duration) -> Self {
        Self { offset, ..self }
    }
}

/// Future associated with driving audio playback.
pub struct PlaybackFuture {
    player: Box<dyn PlayerInterface + 'static>,
    events: PlayerEventStream,
    commands: mpsc::UnboundedReceiver<Command>,
    queue: Queue,
    bus: Arc<RwLock<Bus<Event>>>,
    /// Future associated with popping the front control.
    pop_front: Option<PopFrontFuture>,
    /// Playback is paused.
    paused: bool,
    /// There is a song loaded into the player.
    loaded: Option<Loaded>,
    /// A song to inject to play _right now_.
    inject: Option<(Arc<Item>, Duration)>,
    /// A song that has been sidelined by another song.
    sidelined: VecDeque<(Loaded, Instant)>,
    /// Items to fall back to when there are no more songs in queue.
    fallback_items: Vec<Arc<Item>>,
    /// Current volume.
    volume: Arc<RwLock<u32>>,
    /// Current song that is loaded.
    current: Arc<RwLock<Option<Arc<Item>>>>,
    /// Path to write current song.
    current_song: Option<Arc<current_song::CurrentSong>>,
    /// Current config.
    echo_current_song: bool,
}

impl PlaybackFuture {
    /// Play what is at the front of the queue.
    fn next_song(&mut self) -> Option<Loaded> {
        use rand::Rng;

        if let Some((item, offset)) = self.inject.take() {
            // store the currently playing song in the sidelined slot.
            if let Some(loaded) = self.loaded.take() {
                self.sidelined.push_back((loaded, Instant::now()));
            }

            let future = self.player.load(&*item, offset.as_millis() as u32);
            return Some(Loaded::new(Origin::Injected, item, future).with_offset(offset));
        }

        if let Some((loaded, paused_at)) = self.sidelined.pop_front() {
            let offset = if paused_at > loaded.started_at {
                // calculate offset to start playing at
                (paused_at - loaded.started_at) + loaded.offset
            } else {
                Default::default()
            };

            let future = self.player.load(&*loaded.item, offset.as_millis() as u32);
            return Some(Loaded::new(loaded.origin, loaded.item, future).with_offset(offset));
        }

        if let Some(item) = self.queue.front() {
            self.pop_front = Some(self.queue.pop_front());
            let future = self.player.load(&*item, 0);
            return Some(Loaded::new(Origin::Queue, item, future));
        }

        if !self.paused || self.loaded.is_some() {
            let mut rng = rand::thread_rng();

            let n = rng.gen_range(0, self.fallback_items.len());

            // Pick a random item to play.
            if let Some(item) = self.fallback_items.get(n) {
                let future = self.player.load(&*item, 0);
                return Some(Loaded::new(Origin::Fallback, item.clone(), future));
            }
        }

        self.paused = true;
        None
    }

    /// Write current song. Log any errors.
    fn current_song(&self) {
        let current_song = match self.current_song.as_ref() {
            Some(current_song) => current_song,
            None => return,
        };

        let result = match self.loaded.as_ref() {
            Some(loaded) => current_song.write(&loaded.item, self.paused),
            None => current_song.blank(),
        };

        if let Err(e) = result {
            log::warn!(
                "failed to write current song: {}: {}",
                current_song.path.display(),
                e
            );
        }
    }

    /// Load the next song.
    fn load_front(&mut self) {
        if let Some(loaded) = self.next_song() {
            *self.current.write().expect("poisoned") = Some(loaded.item.clone());

            if !self.paused {
                self.player.play();
                self.broadcast(Event::Playing(
                    self.echo_current_song,
                    loaded.origin,
                    loaded.item.clone(),
                ));
            } else {
                self.player.pause();
            }

            self.loaded = Some(loaded);
            self.current_song();
            return;
        }

        self.loaded = None;
        *self.current.write().expect("poisoned") = None;

        self.broadcast(Event::Empty);
        self.player.stop();
        self.current_song();
    }

    /// Broadcast an event from the player.
    fn broadcast(&self, event: Event) {
        let mut b = self.bus.write().expect("poisoned");

        if let Err(e) = b.try_broadcast(event) {
            log::error!("failed to broadcast player event: {:?}", e);
        }
    }

    /// Handle incoming command.
    fn command(&mut self, command: Command) {
        let command = match command {
            Command::Toggle if self.paused => Command::Play,
            Command::Toggle if !self.paused => Command::Pause,
            command => command,
        };

        match command {
            Command::Skip => {
                log::info!("Skipping song");
                self.load_front();
            }
            Command::Pause if !self.paused => {
                log::info!("pausing player");
                self.paused = true;
                self.player.pause();
                self.broadcast(Event::Pausing);
                self.current_song();
            }
            Command::Play if self.paused => {
                log::info!("starting player");
                self.paused = false;

                match self.loaded.as_ref() {
                    Some(loaded) => {
                        self.player.play();
                        self.broadcast(Event::Playing(
                            self.echo_current_song,
                            loaded.origin,
                            loaded.item.clone(),
                        ));
                        self.current_song();
                    }
                    None => {
                        self.load_front();
                    }
                }
            }
            Command::Modified => {
                if !self.paused && self.loaded.is_none() {
                    self.load_front();
                }

                self.broadcast(Event::Modified);
            }
            Command::Volume(volume) => {
                *self.volume.write().expect("poisoned") = volume;
                self.player.volume(Some((volume as f32) / 100f32));
            }
            Command::Inject(item, offset) => {
                self.inject = Some((item, offset));
                self.load_front();
            }
            _ => {}
        }
    }
}

impl Future for PlaybackFuture {
    type Item = ();
    type Error = failure::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            let mut not_ready = true;

            // pop is in progress, make sure that happens before anything else.
            if let Some(pop_front) = self.pop_front.as_mut() {
                if let Async::NotReady = pop_front.poll()? {
                    return Ok(Async::NotReady);
                }

                self.pop_front = None;
                not_ready = false;
            }

            if let Some(loaded) = self.loaded.as_mut() {
                match loaded.future.poll() {
                    Ok(Async::Ready(())) => {
                        log::info!("Song ended");
                        self.load_front();
                        not_ready = false;
                    }
                    Err(oneshot::Canceled) => {
                        self.loaded = None;
                        *self.current.write().expect("poisoned") = None;
                    }
                    Ok(Async::NotReady) => {}
                }
            }

            if let Async::Ready(event) = self
                .events
                .poll()
                .map_err(|_| format_err!("event stream errored"))?
            {
                let event = event.ok_or_else(|| format_err!("events stream ended"))?;

                match event {
                    other => {
                        log::trace!("player event: {:?}", other);
                    }
                }

                not_ready = false;
            }

            if let Async::Ready(command) = self
                .commands
                .poll()
                .map_err(|_| format_err!("events stream errored"))?
            {
                let command = command.ok_or_else(|| format_err!("command stream ended"))?;
                self.command(command);
                not_ready = false;
            }

            if not_ready {
                return Ok(Async::NotReady);
            }
        }
    }
}
