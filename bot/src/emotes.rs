use crate::{
    api::{ffz, twitch::Channel, BetterTTV, FrankerFaceZ, Twitch},
    irc,
    storage::Cache,
    template,
    utils::Duration,
};
use failure::Error;
use hashbrown::HashMap;
use smallvec::SmallVec;
use std::{mem, sync::Arc};

/// Number of badges inlined for performance reasons.
/// Should be a value larger than the typical number of badges you'd see.
const INLINED_BADGES: usize = 8;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Url {
    url: String,
    size: Option<Size>,
}

impl From<String> for Url {
    fn from(url: String) -> Self {
        Url { url, size: None }
    }
}

#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct Urls {
    small: Option<Url>,
    medium: Option<Url>,
    large: Option<Url>,
}

impl From<(u32, u32, ffz::Urls)> for Urls {
    fn from((width, height, urls): (u32, u32, ffz::Urls)) -> Self {
        let mut out = Urls::default();

        let options = vec![
            (1, &mut out.small, urls.x1),
            (2, &mut out.medium, urls.x2),
            (4, &mut out.large, urls.x4),
        ];

        for (factor, dest, url) in options {
            if let Some(url) = url {
                *dest = Some(Url {
                    url,
                    size: Some(Size {
                        width: width * factor,
                        height: height * factor,
                    }),
                });
            }
        }

        out
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Size {
    width: u32,
    height: u32,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Emote {
    urls: Urls,
}

type EmoteByCode = HashMap<String, Arc<Emote>>;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "key")]
enum Key<'a> {
    /// Twitch badges for the given room.
    TwitchSubscriberBadges { target: &'a str },
    /// Twitch badges for the given chat (channel).
    TwitchChatBadges { target: &'a str },
    /// FFZ information for a given user.
    FfzUser { name: &'a str },
    /// Emotes associated with a single room.
    RoomEmotes { target: &'a str },
    /// Global emotes.
    GlobalEmotes,
}

struct Inner {
    cache: Cache,
    ffz: FrankerFaceZ,
    bttv: BetterTTV,
    twitch: Twitch,
}

#[derive(Clone)]
pub struct Emotes {
    inner: Arc<Inner>,
}

impl Emotes {
    /// Construct a new emoticon handler.
    pub fn new(cache: Cache, twitch: Twitch) -> Result<Self, Error> {
        Ok(Self {
            inner: Arc::new(Inner {
                cache: cache.namespaced("emotes"),
                ffz: FrankerFaceZ::new()?,
                bttv: BetterTTV::new()?,
                twitch,
            }),
        })
    }

    /// Extend the given emote set.
    fn extend_ffz_set(emotes: &mut EmoteByCode, s: ffz::Set) {
        for e in s.emoticons {
            let urls = (e.width, e.height, e.urls).into();
            emotes.insert(e.name, Arc::new(Emote { urls }));
        }
    }

    /// Construct a set of room emotes from ffz.
    async fn room_emotes_from_ffz(&self, channel: &Channel) -> Result<EmoteByCode, Error> {
        let mut emotes = EmoteByCode::default();

        let (global, room) = futures::future::try_join(
            self.inner.ffz.set_global(),
            self.inner.ffz.room(&channel.name),
        )
        .await?;

        for (_, s) in global.sets {
            Self::extend_ffz_set(&mut emotes, s);
        }

        if let Some(room) = room {
            for (_, s) in room.sets {
                Self::extend_ffz_set(&mut emotes, s);
            }
        }

        Ok(emotes)
    }

    /// Construct a set of room emotes from bttv.
    async fn room_emotes_from_bttv(&self, channel: &Channel) -> Result<EmoteByCode, Error> {
        let mut emotes = EmoteByCode::default();

        let channel = match self.inner.bttv.channels(&channel.name).await? {
            Some(channel) => channel,
            None => return Ok(emotes),
        };

        let url_template = template::Template::compile(&channel.url_template)?;

        for e in channel.emotes {
            let mut urls = Urls::default();

            let options = vec![
                (&mut urls.small, "1x"),
                (&mut urls.medium, "2x"),
                (&mut urls.large, "3x"),
            ];

            for (dest, size) in options.into_iter() {
                let url = url_template.render_to_string(Args {
                    id: e.id.as_str(),
                    image: size,
                })?;

                *dest = Some(Url { url, size: None });
            }

            emotes.insert(e.code, Arc::new(Emote { urls }));
        }

        return Ok(emotes);

        #[derive(Debug, serde::Serialize)]
        struct Args<'a> {
            id: &'a str,
            image: &'a str,
        }
    }

    /// Construct a twitch emote.
    fn twitch_emote(id: u64) -> Arc<Emote> {
        let mut urls = Urls::default();

        let options = vec![
            (&mut urls.small, "1.0"),
            (&mut urls.medium, "2.0"),
            (&mut urls.large, "3.0"),
        ];

        for (dest, size) in options.into_iter() {
            let url = format!("//static-cdn.jtvnw.net/emoticons/v1/{}/{}", id, size);
            *dest = Some(Url { url, size: None });
        }

        Arc::new(Emote { urls })
    }

    /// Construct a set of room emotes from twitch.
    async fn emote_sets_from_twitch(&self, emote_sets: &str) -> Result<EmoteByCode, Error> {
        let result = self.inner.twitch.chat_emoticon_images(emote_sets).await?;

        let mut emotes = EmoteByCode::default();

        for (_, set) in result.emoticon_sets {
            for e in set {
                emotes.insert(e.code, Self::twitch_emote(e.id));
            }
        }

        Ok(emotes)
    }

    /// Get all room emotes.
    async fn room_emotes(&self, channel: &Channel) -> Result<Arc<EmoteByCode>, Error> {
        self.inner
            .cache
            .wrap(
                Key::RoomEmotes {
                    target: &channel.name,
                },
                Duration::hours(6),
                async {
                    let mut emotes = EmoteByCode::default();
                    let (a, b) = futures::future::try_join(
                        self.room_emotes_from_ffz(channel),
                        self.room_emotes_from_bttv(channel),
                    )
                    .await?;
                    emotes.extend(a);
                    emotes.extend(b);
                    Ok(Arc::new(emotes))
                },
            )
            .await
    }

    /// Get all user emotes.
    fn message_emotes_twitch(&self, tags: &irc::Tags, message: &str) -> Result<EmoteByCode, Error> {
        let emotes = match tags.emotes.as_ref() {
            Some(emotes) => match emotes.as_str() {
                "" => return Ok(Default::default()),
                emotes => emotes,
            },
            None => return Ok(Default::default()),
        };

        let mut out = EmoteByCode::default();

        // 300354391:8-16/28087:0-6
        for emote in emotes.split('/') {
            let mut p = emote.split(':');

            let id = match p.next() {
                Some(id) => str::parse::<u64>(id)?,
                None => continue,
            };

            let span = match p.next() {
                Some(rest) => first_span(rest),
                None => continue,
            };

            let word = match span {
                Some((s, e)) => &message[s..=e],
                None => continue,
            };

            out.insert(word.to_string(), Self::twitch_emote(id));
        }

        return Ok(out);

        fn first_span(rest: &str) -> Option<(usize, usize)> {
            let mut it = rest.split(',').next()?.split('-');

            let s = it.next()?;
            let s = str::parse::<usize>(&s).ok()?;

            let e = it.next()?;
            let e = str::parse::<usize>(&e).ok()?;

            Some((s, e))
        }
    }

    /// Get all user emotes.
    async fn global_emotes(&self) -> Result<Arc<EmoteByCode>, Error> {
        self.inner
            .cache
            .wrap(Key::GlobalEmotes, Duration::hours(72), async {
                let emotes = self.emote_sets_from_twitch("0").await?;
                Ok(Arc::new(emotes))
            })
            .await
    }

    /// Get twitch subscriber badges.
    async fn twitch_subscriber_badge(
        &self,
        channel: &Channel,
        needle: u32,
    ) -> Result<Option<Badge>, Error> {
        let badges = self
            .inner
            .cache
            .wrap(
                Key::TwitchSubscriberBadges {
                    target: &channel.name,
                },
                Duration::hours(24),
                self.inner.twitch.badges_display(&channel.id),
            )
            .await?;

        let mut badges = match badges {
            Some(badges) => badges,
            None => return Ok(None),
        };

        let subscriber = match badges.badge_sets.remove("subscriber") {
            Some(subscriber) => subscriber,
            None => return Ok(None),
        };

        let mut best = None;

        for (version, badge) in subscriber.versions {
            let version = match str::parse::<u32>(&version).ok() {
                Some(version) => version,
                None => continue,
            };

            best = match best {
                Some((v, _)) if version <= needle && version > v => Some((version, badge)),
                Some(best) => Some(best),
                None => Some((version, badge)),
            };
        }

        if let Some((_, badge)) = best {
            let mut urls = Urls::default();
            urls.small = Some(Url::from(badge.image_url_1x));
            urls.medium = Some(Url::from(badge.image_url_2x));
            urls.large = Some(Url::from(badge.image_url_4x));

            return Ok(Some(Badge {
                title: badge.title,
                urls,
                bg_color: None,
            }));
        }

        Ok(None)
    }

    /// Get ffz chat badges.
    async fn ffz_chat_badges(
        &self,
        name: &str,
    ) -> Result<SmallVec<[Badge; INLINED_BADGES]>, Error> {
        let user = self
            .inner
            .cache
            .wrap(
                Key::FfzUser { name },
                Duration::hours(24),
                self.inner.ffz.user(name),
            )
            .await?;

        let mut out = SmallVec::new();

        let user = match user {
            Some(user) => user,
            None => return Ok(out),
        };

        for (_, badge) in user.badges {
            let urls = (18u32, 18u32, badge.urls).into();

            out.push(Badge {
                title: badge.title,
                urls,
                bg_color: Some(badge.color),
            });
        }

        Ok(out)
    }

    /// Get twitch chat badges.
    async fn twitch_chat_badges(
        &self,
        channel: &Channel,
        chat_badges: impl Iterator<Item = (&str, u32)>,
    ) -> Result<SmallVec<[Badge; INLINED_BADGES]>, Error> {
        let badges = self
            .inner
            .cache
            .wrap(
                Key::TwitchChatBadges {
                    target: &channel.name,
                },
                Duration::hours(72),
                self.inner.twitch.chat_badges(&channel.id),
            )
            .await?;

        let mut out = SmallVec::new();

        let mut badges = match badges {
            Some(badges) => badges,
            None => return Ok(out),
        };

        for (name, version) in chat_badges {
            let name = match name {
                "admin" => "admin",
                "broadcaster" => "broadcaster",
                "global_mod" => "global_mod",
                "moderator" => "mod",
                "staff" => "staff",
                "turbo" => "turbo",
                "subscriber" => {
                    // NB: subscriber badges are handled separately.
                    out.extend(self.twitch_subscriber_badge(channel, version).await?);
                    continue;
                }
                "bits" => {
                    // NB: bits badges are not supported.
                    continue;
                }
                name => {
                    // NB: not supported.
                    log::trace!("Unsupported badge: {}", name);
                    continue;
                }
            };

            let badge = match badges.badges.remove(name) {
                Some(badge) => badge,
                None => continue,
            };

            let image = match badge.image {
                Some(image) => image,
                None => continue,
            };

            let mut urls = Urls::default();
            urls.small = Some(image.into());

            out.push(Badge {
                title: name.to_string(),
                urls,
                bg_color: None,
            });
        }

        Ok(out)
    }

    /// Render all room badges.
    async fn room_badges(
        &self,
        tags: &irc::Tags,
        channel: &Channel,
        name: &str,
    ) -> Result<SmallVec<[Badge; INLINED_BADGES]>, Error> {
        let mut out = SmallVec::new();

        if let Some(badges) = tags.badges.as_ref() {
            match self.twitch_chat_badges(channel, split_badges(badges)).await {
                Ok(badges) => out.extend(badges),
                Err(e) => log::warn!("failed to get twitch chat badges: {}", e),
            }
        }

        match self.ffz_chat_badges(name).await {
            Ok(badges) => out.extend(badges),
            Err(e) => log::warn!("failed to get ffz chat badges: {}", e),
        }

        return Ok(out);

        /// Split all the badges.
        fn split_badges<'a>(badges: &'a str) -> impl Iterator<Item = (&'a str, u32)> {
            badges.split(',').flat_map(|b| {
                let mut it = b.split('/');
                let badge = it.next()?;
                let version = str::parse::<u32>(it.next()?).ok()?;
                Some((badge, version))
            })
        }
    }

    pub async fn render(
        &self,
        tags: &irc::Tags,
        channel: &Channel,
        name: &str,
        message: &str,
    ) -> Result<Rendered, Error> {
        use futures::future;

        let (badges, room_emotes, global_emotes) = future::try_join3(
            self.room_badges(tags, channel, name),
            self.room_emotes(channel),
            self.global_emotes(),
        )
        .await?;
        let message_emotes = self.message_emotes_twitch(tags, message)?;

        Ok(Rendered::render(
            badges,
            message,
            &*room_emotes,
            &message_emotes,
            &*global_emotes,
        ))
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type")]
enum Item {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "emote")]
    Emote { emote: String },
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Badge {
    /// Title for badge.
    title: String,
    /// Urls to pick for badge.
    urls: Urls,
    /// Optional background color.
    bg_color: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Rendered {
    badges: SmallVec<[Badge; INLINED_BADGES]>,
    items: Vec<Item>,
    emotes: HashMap<String, Arc<Emote>>,
}

impl Rendered {
    /// Convert a text into a rendered collection.
    fn render(
        badges: SmallVec<[Badge; INLINED_BADGES]>,
        text: &str,
        room_emotes: &EmoteByCode,
        message_emotes: &EmoteByCode,
        global_emotes: &EmoteByCode,
    ) -> Rendered {
        let mut buf = text;

        let mut emotes = HashMap::new();
        let mut items = Vec::new();

        'outer: loop {
            let mut it = Words::new(buf);

            while let Some((idx, word)) = it.next() {
                let emote = match room_emotes
                    .get(word)
                    .or_else(|| message_emotes.get(word))
                    .or_else(|| global_emotes.get(word))
                {
                    Some(emote) => emote,
                    None => continue,
                };

                if !emotes.contains_key(word) {
                    emotes.insert(word.to_string(), emote.clone());
                }

                let text = &buf[..idx];

                if !text.is_empty() {
                    items.push(Item::Text {
                        text: text.to_string(),
                    });
                }

                items.push(Item::Emote {
                    emote: word.to_string(),
                });

                buf = &buf[(idx + word.len())..];
                continue 'outer;
            }

            break;
        }

        if !buf.is_empty() {
            items.push(Item::Text {
                text: buf.to_string(),
            });
        }

        Rendered {
            badges,
            items,
            emotes,
        }
    }
}

#[derive(Debug)]
pub struct Words<'a> {
    string: &'a str,
    n: usize,
}

impl<'a> Words<'a> {
    /// Split a string into words.
    pub fn new(string: &str) -> Words<'_> {
        Words { string, n: 0 }
    }
}

impl<'a> Iterator for Words<'a> {
    type Item = (usize, &'a str);

    fn next(&mut self) -> Option<Self::Item> {
        if self.string.is_empty() {
            return None;
        }

        let s = match self.string.find(|c: char| !c.is_whitespace()) {
            Some(n) => n,
            None => {
                let string = mem::replace(&mut self.string, "");
                self.n = self.n + string.len();
                return None;
            }
        };

        let e = match self.string[s..].find(char::is_whitespace) {
            Some(n) => s + n,
            None => {
                let string = mem::replace(&mut self.string, "");
                let n = self.n + s;
                self.n = self.n + string.len();
                return Some((n, &string[s..]));
            }
        };

        let string = &self.string[s..e];
        self.string = &self.string[e..];
        let s = self.n + s;
        self.n = self.n + e;
        Some((s, string))
    }
}

#[cfg(test)]
mod tests {
    use super::Words;

    #[test]
    pub fn test_words() {
        let w = Words::new("");
        assert_eq!(Vec::<(usize, &str)>::new(), w.collect::<Vec<_>>());

        let w = Words::new("Foo Bar");
        assert_eq!(vec![(0, "Foo"), (4, "Bar")], w.collect::<Vec<_>>());

        let w = Words::new(" Foo   ");
        assert_eq!(vec![(1, "Foo")], w.collect::<Vec<_>>());

        let w = Words::new("test test PrideGive");
        assert_eq!(
            vec![(0, "test"), (5, "test"), (10, "PrideGive")],
            w.collect::<Vec<_>>()
        );
    }
}