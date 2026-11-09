use gpui::RenderImage;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    io::Read,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

const CACHE_ENTRIES: usize = 96;
const CACHE_BYTES: usize = 40 * 1024 * 1024;
const WORKERS: usize = 3;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Resolution {
    Thumbnail,
    Hero,
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
struct ArtworkRequest {
    url: String,
    resolution: Resolution,
}

pub struct LoadedArtwork {
    request: ArtworkRequest,
    image: Option<Arc<RenderImage>>,
}

pub struct Artwork {
    requests: async_channel::Sender<ArtworkRequest>,
    queued: async_channel::Receiver<ArtworkRequest>,
    pending: HashSet<ArtworkRequest>,
    cache: HashMap<ArtworkRequest, CachedArtwork>,
    recent: VecDeque<ArtworkRequest>,
    cache_bytes: usize,
}

enum CachedArtwork {
    Loaded {
        image: Arc<RenderImage>,
        bytes: usize,
    },
    Failed(Instant),
}

impl Artwork {
    pub fn new() -> (Self, async_channel::Receiver<LoadedArtwork>) {
        let (requests, receiver) = async_channel::bounded::<ArtworkRequest>(64);
        let (results, loaded) = async_channel::bounded(WORKERS);
        for n in 0..WORKERS {
            let receiver = receiver.clone();
            let results = results.clone();
            thread::Builder::new()
                .name(format!("artwork-{n}"))
                .spawn(move || {
                    let client = reqwest::blocking::Client::builder()
                        .redirect(reqwest::redirect::Policy::none())
                        .connect_timeout(Duration::from_secs(5))
                        .timeout(Duration::from_secs(12))
                        .build();
                    while let Ok(request) = receiver.recv_blocking() {
                        let image = client
                            .as_ref()
                            .ok()
                            .and_then(|client| load(client, &request).ok())
                            .map(Arc::new);
                        if results
                            .send_blocking(LoadedArtwork { request, image })
                            .is_err()
                        {
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
                cache_bytes: 0,
            },
            loaded,
        )
    }

    pub fn get(&mut self, url: &str) -> Option<Arc<RenderImage>> {
        self.get_resolution(url, Resolution::Thumbnail)
    }

    pub fn get_hero(&mut self, url: &str) -> Option<Arc<RenderImage>> {
        self.get_resolution(url, Resolution::Hero)
    }

    fn get_resolution(&mut self, url: &str, resolution: Resolution) -> Option<Arc<RenderImage>> {
        let request = ArtworkRequest {
            url: url.to_owned(),
            resolution,
        };
        if let Some(cached) = self.cache.get(&request) {
            if !matches!(cached, CachedArtwork::Failed(retry_at) if *retry_at <= Instant::now()) {
                self.recent.retain(|key| key != &request);
                self.recent.push_back(request);
                return match cached {
                    CachedArtwork::Loaded { image, .. } => Some(image.clone()),
                    CachedArtwork::Failed(_) => None,
                };
            }
            self.cache.remove(&request);
            self.recent.retain(|key| key != &request);
        }
        if !self.pending.contains(&request) && self.requests.try_send(request.clone()).is_ok() {
            self.pending.insert(request);
        }
        None
    }

    pub fn cancel_pending(&mut self) {
        while let Ok(request) = self.queued.try_recv() {
            self.pending.remove(&request);
        }
    }

    pub fn insert(&mut self, loaded: LoadedArtwork) -> Vec<Arc<RenderImage>> {
        let mut evicted = Vec::new();
        self.pending.remove(&loaded.request);
        self.recent.retain(|key| key != &loaded.request);
        if let Some(CachedArtwork::Loaded { image, bytes }) = self.cache.remove(&loaded.request) {
            self.cache_bytes -= bytes;
            evicted.push(image);
        }
        self.recent.push_back(loaded.request.clone());
        self.cache.insert(
            loaded.request,
            match loaded.image {
                Some(image) => {
                    let bytes = image.as_bytes(0).map_or(0, <[u8]>::len);
                    self.cache_bytes += bytes;
                    CachedArtwork::Loaded { image, bytes }
                }
                None => CachedArtwork::Failed(Instant::now() + Duration::from_secs(30)),
            },
        );
        while self.cache.len() > CACHE_ENTRIES || self.cache_bytes > CACHE_BYTES {
            let oldest = self
                .recent
                .pop_front()
                .expect("Cached artwork has an eviction entry");
            if let Some(CachedArtwork::Loaded { image, bytes }) = self.cache.remove(&oldest) {
                self.cache_bytes -= bytes;
                evicted.push(image);
            }
        }
        evicted
    }
}

fn load(
    client: &reqwest::blocking::Client,
    request: &ArtworkRequest,
) -> anyhow::Result<RenderImage> {
    let parsed = reqwest::Url::parse(&request.url)?;
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
    decode(bytes, request.resolution)
}

fn decode(bytes: Vec<u8>, resolution: Resolution) -> anyhow::Result<RenderImage> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(4096);
    limits.max_image_height = Some(4096);
    limits.max_alloc = Some(64 * 1024 * 1024);
    reader.limits(limits);
    let (width, height) = match resolution {
        Resolution::Thumbnail => (320, 320),
        Resolution::Hero => (1600, 900),
    };
    let mut pixels = reader.decode()?.thumbnail(width, height).into_rgba8();
    for pixel in pixels.pixels_mut() {
        pixel.0.swap(0, 2);
    }
    Ok(RenderImage::new(vec![image::Frame::new(pixels)]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (Artwork, async_channel::Receiver<ArtworkRequest>) {
        let (requests, queued) = async_channel::bounded(64);
        (
            Artwork {
                requests,
                queued: queued.clone(),
                pending: HashSet::new(),
                cache: HashMap::new(),
                recent: VecDeque::new(),
                cache_bytes: 0,
            },
            queued,
        )
    }

    fn request(url: &str, resolution: Resolution) -> ArtworkRequest {
        ArtworkRequest {
            url: url.to_owned(),
            resolution,
        }
    }

    fn image(width: u32, height: u32) -> Arc<RenderImage> {
        Arc::new(RenderImage::new(vec![image::Frame::new(
            image::RgbaImage::new(width, height),
        )]))
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
        assert!(
            !artwork
                .pending
                .contains(&request("new-page", Resolution::Thumbnail))
        );

        artwork.cancel_pending();
        assert!(queued.is_empty());
        assert_eq!(artwork.pending.len(), 3);
        artwork.get("new-page");
        assert_eq!(
            queued.try_recv().unwrap(),
            request("new-page", Resolution::Thumbnail)
        );
        for request in &in_flight {
            artwork.get(&request.url);
        }
        assert!(
            queued.is_empty(),
            "in-flight downloads must not be duplicated"
        );

        artwork.insert(LoadedArtwork {
            request: in_flight[0].clone(),
            image: None,
        });
        assert!(!artwork.pending.contains(&in_flight[0]));
        artwork.get("old-page-0");
        assert_eq!(queued.try_recv().unwrap().url, "old-page-0");
    }

    #[test]
    fn failed_artwork_retries_on_access_after_its_cooldown() {
        let (mut artwork, queued) = fixture();
        artwork.get("artwork");
        let request = queued.try_recv().unwrap();
        artwork.insert(LoadedArtwork {
            request: request.clone(),
            image: None,
        });
        for _ in 0..100 {
            assert!(artwork.get("artwork").is_none());
        }
        assert!(queued.is_empty());

        let CachedArtwork::Failed(retry_at) = artwork.cache.get_mut(&request).unwrap() else {
            panic!("failed downloads must have a retry deadline");
        };
        *retry_at = Instant::now() - Duration::from_secs(1);
        artwork.get("artwork");
        artwork.get("artwork");
        assert_eq!(queued.try_recv().unwrap(), request);
        assert!(queued.is_empty(), "only one retry may be in flight");

        let image = image(1, 1);
        artwork.insert(LoadedArtwork {
            request,
            image: Some(image.clone()),
        });
        assert!(Arc::ptr_eq(&artwork.get("artwork").unwrap(), &image));
        assert!(queued.is_empty());
    }

    #[test]
    fn thumbnail_and_hero_requests_are_deduplicated_at_their_own_resolution() {
        let (mut artwork, queued) = fixture();
        for _ in 0..10 {
            artwork.get("shared-url");
            artwork.get_hero("shared-url");
        }
        assert_eq!(artwork.pending.len(), 2);
        assert_eq!(
            queued.try_recv().unwrap(),
            request("shared-url", Resolution::Thumbnail)
        );
        assert_eq!(
            queued.try_recv().unwrap(),
            request("shared-url", Resolution::Hero)
        );
        assert!(queued.is_empty());

        let thumbnail = image(320, 180);
        let hero = image(1600, 900);
        for (resolution, image) in [
            (Resolution::Thumbnail, thumbnail.clone()),
            (Resolution::Hero, hero.clone()),
        ] {
            artwork.insert(LoadedArtwork {
                request: request("shared-url", resolution),
                image: Some(image),
            });
        }
        assert!(artwork.pending.is_empty());
        assert!(Arc::ptr_eq(&artwork.get("shared-url").unwrap(), &thumbnail));
        assert!(Arc::ptr_eq(&artwork.get_hero("shared-url").unwrap(), &hero));
        assert_eq!(artwork.cache.len(), 2);
        assert!(queued.is_empty());
    }

    #[test]
    fn entry_eviction_returns_decoded_images_for_gpu_cleanup() {
        let (mut artwork, _) = fixture();
        let image = image(1, 1);
        artwork.insert(LoadedArtwork {
            request: request("loaded", Resolution::Thumbnail),
            image: Some(image.clone()),
        });
        for index in 0..95 {
            assert!(
                artwork
                    .insert(LoadedArtwork {
                        request: request(&format!("failed-{index}"), Resolution::Thumbnail),
                        image: None,
                    })
                    .is_empty()
            );
        }
        let evicted = artwork.insert(LoadedArtwork {
            request: request("new", Resolution::Thumbnail),
            image: None,
        });
        assert_eq!(evicted.len(), 1);
        assert!(Arc::ptr_eq(&evicted[0], &image));
        assert_eq!(artwork.cache.len(), CACHE_ENTRIES);
        assert_eq!(artwork.cache_bytes, 0);
    }

    #[test]
    fn hero_insertion_evicts_enough_old_thumbnails_to_stay_within_the_byte_budget() {
        let (mut artwork, _) = fixture();
        let thumbnails = (0..96).map(|_| image(320, 320)).collect::<Vec<_>>();
        for (index, image) in thumbnails.iter().enumerate() {
            assert!(
                artwork
                    .insert(LoadedArtwork {
                        request: request(&index.to_string(), Resolution::Thumbnail),
                        image: Some(image.clone()),
                    })
                    .is_empty()
            );
        }
        artwork.get("0");
        let hero = image(1600, 900);
        let evicted = artwork.insert(LoadedArtwork {
            request: request("hero", Resolution::Hero),
            image: Some(hero.clone()),
        });
        assert_eq!(evicted.len(), 8);
        for (evicted, expected) in evicted.iter().zip(&thumbnails[1..9]) {
            assert!(Arc::ptr_eq(evicted, expected));
        }
        assert!(Arc::ptr_eq(&artwork.get("0").unwrap(), &thumbnails[0]));
        assert!(Arc::ptr_eq(&artwork.get_hero("hero").unwrap(), &hero));
        assert_eq!(artwork.cache_bytes, 88 * 320 * 320 * 4 + 1600 * 900 * 4);
        assert!(artwork.cache_bytes <= CACHE_BYTES);
        assert!(artwork.cache.len() <= CACHE_ENTRIES);
    }

    #[test]
    fn replacing_a_cached_result_releases_its_previous_image_and_byte_charge() {
        let (mut artwork, _) = fixture();
        let original = image(320, 320);
        let replacement = image(160, 160);
        assert!(
            artwork
                .insert(LoadedArtwork {
                    request: request("same", Resolution::Thumbnail),
                    image: Some(original.clone()),
                })
                .is_empty()
        );
        let evicted = artwork.insert(LoadedArtwork {
            request: request("same", Resolution::Thumbnail),
            image: Some(replacement.clone()),
        });
        assert_eq!(evicted.len(), 1);
        assert!(Arc::ptr_eq(&evicted[0], &original));
        assert_eq!(artwork.cache.len(), 1);
        assert_eq!(artwork.recent.len(), 1);
        assert_eq!(artwork.cache_bytes, 160 * 160 * 4);
        assert!(Arc::ptr_eq(&artwork.get("same").unwrap(), &replacement));
    }

    #[test]
    fn decoding_keeps_thumbnail_size_and_preserves_sharper_hero_pixels() {
        let source = image::DynamicImage::ImageRgba8(image::RgbaImage::new(1920, 1080));
        let mut bytes = std::io::Cursor::new(Vec::new());
        source
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        for (resolution, expected) in [
            (Resolution::Thumbnail, (320, 180)),
            (Resolution::Hero, (1600, 900)),
        ] {
            let image = decode(bytes.get_ref().clone(), resolution).unwrap();
            let size = image.size(0);
            assert_eq!((size.width.0, size.height.0), expected);
        }
    }
}
