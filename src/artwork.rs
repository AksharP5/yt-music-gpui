use gpui::RenderImage;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    io::Read,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

pub struct LoadedArtwork {
    pub url: String,
    pub image: Option<Arc<RenderImage>>,
}

pub struct Artwork {
    requests: async_channel::Sender<String>,
    queued: async_channel::Receiver<String>,
    pending: HashSet<String>,
    cache: HashMap<String, CachedArtwork>,
    recent: VecDeque<String>,
}

enum CachedArtwork {
    Loaded(Arc<RenderImage>),
    Failed(Instant),
}

impl Artwork {
    pub fn new() -> (Self, async_channel::Receiver<LoadedArtwork>) {
        let (requests, receiver) = async_channel::bounded::<String>(64);
        let (results, loaded) = async_channel::unbounded();
        for n in 0..3 {
            let receiver = receiver.clone();
            let results = results.clone();
            thread::Builder::new()
                .name(format!("artwork-{n}"))
                .spawn(move || {
                    let client = reqwest::blocking::Client::builder()
                        .connect_timeout(Duration::from_secs(5))
                        .timeout(Duration::from_secs(12))
                        .build();
                    while let Ok(url) = receiver.recv_blocking() {
                        let image = client
                            .as_ref()
                            .ok()
                            .and_then(|client| load(client, &url).ok())
                            .map(Arc::new);
                        if results.send_blocking(LoadedArtwork { url, image }).is_err() {
                            break;
                        }
                    }
                })
                .expect("start artwork worker");
        }
        (
            Self {
                requests,
                queued: receiver,
                pending: HashSet::new(),
                cache: HashMap::new(),
                recent: VecDeque::new(),
            },
            loaded,
        )
    }

    pub fn get(&mut self, url: &str) -> Option<Arc<RenderImage>> {
        if let Some(cached) = self.cache.get(url) {
            if !matches!(cached, CachedArtwork::Failed(retry_at) if *retry_at <= Instant::now()) {
                self.recent.retain(|key| key != url);
                self.recent.push_back(url.to_owned());
                return match cached {
                    CachedArtwork::Loaded(image) => Some(image.clone()),
                    CachedArtwork::Failed(_) => None,
                };
            }
            self.cache.remove(url);
            self.recent.retain(|key| key != url);
        }
        if !self.pending.contains(url) && self.requests.try_send(url.to_owned()).is_ok() {
            self.pending.insert(url.to_owned());
        }
        None
    }

    pub fn cancel_pending(&mut self) {
        while let Ok(url) = self.queued.try_recv() {
            self.pending.remove(&url);
        }
    }

    pub fn insert(&mut self, loaded: LoadedArtwork) -> Option<Arc<RenderImage>> {
        self.pending.remove(&loaded.url);
        self.recent.push_back(loaded.url.clone());
        self.cache.insert(
            loaded.url,
            match loaded.image {
                Some(image) => CachedArtwork::Loaded(image),
                None => CachedArtwork::Failed(Instant::now() + Duration::from_secs(30)),
            },
        );
        if self.cache.len() > 96
            && let Some(oldest) = self.recent.pop_front()
            && let Some(CachedArtwork::Loaded(image)) = self.cache.remove(&oldest)
        {
            return Some(image);
        }
        None
    }
}

fn load(client: &reqwest::blocking::Client, url: &str) -> anyhow::Result<RenderImage> {
    let parsed = reqwest::Url::parse(url)?;
    anyhow::ensure!(
        parsed.scheme() == "https"
            && parsed.host_str().is_some_and(|h| h == "i.ytimg.com"
                || h.ends_with(".googleusercontent.com")
                || h.ends_with(".ggpht.com")),
        "Unsupported artwork host"
    );
    let mut bytes = Vec::new();
    client
        .get(parsed)
        .send()?
        .error_for_status()?
        .take(4 * 1024 * 1024)
        .read_to_end(&mut bytes)?;
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(4096);
    limits.max_image_height = Some(4096);
    limits.max_alloc = Some(64 * 1024 * 1024);
    reader.limits(limits);
    let mut pixels = reader.decode()?.thumbnail(320, 320).into_rgba8();
    for pixel in pixels.pixels_mut() {
        pixel.0.swap(0, 2);
    }
    Ok(RenderImage::new(vec![image::Frame::new(pixels)]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (Artwork, async_channel::Receiver<String>) {
        let (requests, queued) = async_channel::bounded(64);
        (
            Artwork {
                requests,
                queued: queued.clone(),
                pending: HashSet::new(),
                cache: HashMap::new(),
                recent: VecDeque::new(),
            },
            queued,
        )
    }

    #[test]
    fn navigation_discards_queued_artwork_and_keeps_in_flight_downloads() {
        let (mut artwork, queued) = fixture();
        let mut in_flight = Vec::new();
        for index in 0..3 {
            artwork.get(&format!("in-flight-{index}"));
            in_flight.push(queued.try_recv().unwrap());
        }
        for index in 0..64 {
            artwork.get(&format!("old-page-{index}"));
        }
        artwork.get("new-page");
        assert_eq!(queued.len(), 64);
        assert!(!artwork.pending.contains("new-page"));

        artwork.cancel_pending();
        assert!(queued.is_empty());
        assert_eq!(artwork.pending.len(), 3);
        artwork.get("new-page");
        assert_eq!(queued.try_recv().unwrap(), "new-page");
        for url in &in_flight {
            artwork.get(url);
        }
        assert!(
            queued.is_empty(),
            "in-flight downloads must not be duplicated"
        );

        artwork.insert(LoadedArtwork {
            url: in_flight[0].clone(),
            image: None,
        });
        assert!(!artwork.pending.contains(&in_flight[0]));
        artwork.get("old-page-0");
        assert_eq!(queued.try_recv().unwrap(), "old-page-0");
    }

    #[test]
    fn failed_artwork_retries_on_access_after_its_cooldown() {
        let (mut artwork, queued) = fixture();
        artwork.get("artwork");
        assert_eq!(queued.try_recv().unwrap(), "artwork");
        artwork.insert(LoadedArtwork {
            url: "artwork".into(),
            image: None,
        });
        for _ in 0..100 {
            assert!(artwork.get("artwork").is_none());
        }
        assert!(queued.is_empty());

        let CachedArtwork::Failed(retry_at) = artwork.cache.get_mut("artwork").unwrap() else {
            panic!("failed downloads must have a retry deadline");
        };
        *retry_at = Instant::now() - Duration::from_secs(1);
        artwork.get("artwork");
        artwork.get("artwork");
        assert_eq!(queued.try_recv().unwrap(), "artwork");
        assert!(queued.is_empty(), "only one retry may be in flight");

        let image = Arc::new(RenderImage::new(vec![image::Frame::new(
            image::RgbaImage::new(1, 1),
        )]));
        artwork.insert(LoadedArtwork {
            url: "artwork".into(),
            image: Some(image.clone()),
        });
        assert!(Arc::ptr_eq(&artwork.get("artwork").unwrap(), &image));
        assert!(queued.is_empty());
    }

    #[test]
    fn eviction_returns_a_decoded_image_for_gpu_cleanup() {
        let (mut artwork, _) = fixture();
        let image = Arc::new(RenderImage::new(vec![image::Frame::new(
            image::RgbaImage::new(1, 1),
        )]));
        artwork.insert(LoadedArtwork {
            url: "loaded".into(),
            image: Some(image.clone()),
        });
        for index in 0..95 {
            assert!(
                artwork
                    .insert(LoadedArtwork {
                        url: format!("failed-{index}"),
                        image: None,
                    })
                    .is_none()
            );
        }
        let evicted = artwork
            .insert(LoadedArtwork {
                url: "new".into(),
                image: None,
            })
            .unwrap();
        assert!(Arc::ptr_eq(&evicted, &image));
        assert_eq!(artwork.cache.len(), 96);
    }
}
