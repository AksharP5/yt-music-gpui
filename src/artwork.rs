use anyhow::Context;
use gpui::{Pixels, RenderImage, Size};
use sha1::{Digest, Sha1};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs::{self, File},
    io::{Read, Write},
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock, PoisonError},
    thread,
    time::{Duration, Instant, SystemTime},
};

const CACHE_ENTRIES: usize = 512;
const DISK_BYTES: u64 = 256 * 1024 * 1024;
const DOWNLOAD_BYTES: u64 = 4 * 1024 * 1024;
const DISK_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);
const CACHE_BYTES: usize = 40 * 1024 * 1024;
const WORKERS: usize = 3;
/// Device-pixel sizes that downloads and decodes step up to. GPUI samples
/// textures without mipmaps, so an image drawn at under half its pixel size
/// aliases; the steps keep that ratio under 1.5 while a window resize reuses
/// the same image until it crosses a step.
const STEPS: [u32; 13] = [
    32, 48, 64, 96, 128, 192, 256, 384, 512, 768, 1024, 1536, 2048,
];
const WASH: u32 = 64;
/// The widest fixed-size video still the catalog lists (`hqdefault`).
const LISTED_STILL_WIDTH: u32 = 480;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Resolution {
    /// Enough device pixels to cover this box; a zero side is unconstrained.
    Cover { width: u32, height: u32 },
    /// A small, heavily blurred copy that GPUI stretches into a page wash.
    Wash,
}

impl Resolution {
    fn cover(size: Size<Pixels>, scale: f32) -> Self {
        let step = |length: Pixels| {
            let pixels = f32::from(length) * scale;
            if pixels <= 0. {
                return 0;
            }
            STEPS
                .into_iter()
                .find(|step| *step as f32 >= pixels)
                .unwrap_or(STEPS[STEPS.len() - 1])
        };
        Self::Cover {
            width: step(size.width),
            height: step(size.height),
        }
    }

    fn box_size(self) -> (u32, u32) {
        match self {
            Self::Cover { width, height } => (width, height),
            Self::Wash => (WASH, WASH),
        }
    }
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
    /// The last loaded size of each URL, drawn while a different size loads.
    shown: HashMap<String, Resolution>,
    scale: f32,
}

enum CachedArtwork {
    Loaded {
        image: Arc<RenderImage>,
        bytes: usize,
    },
    Failed(Instant),
}

impl Artwork {
    #[cfg(test)]
    #[cfg(unix)]
    pub(crate) fn is_idle(&self) -> bool {
        self.pending.is_empty()
    }

    #[expect(
        clippy::expect_used,
        reason = "the app cannot run without this worker, and spawning one fails only when the OS is out of threads"
    )]
    pub fn new() -> (Self, async_channel::Receiver<LoadedArtwork>) {
        let (requests, receiver) = async_channel::bounded::<ArtworkRequest>(64);
        let (results, loaded) = async_channel::bounded(WORKERS);
        let disk = Arc::new(OnceLock::new());
        for n in 0..WORKERS {
            let disk = disk.clone();
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
                            .and_then(|client| {
                                load(
                                    client,
                                    &request,
                                    disk.get_or_init(|| {
                                        Mutex::new(DiskCache::new(
                                            crate::dirs::cache().map(|cache| cache.join("artwork")),
                                        ))
                                    }),
                                )
                                .ok()
                            })
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
                shown: HashMap::new(),
                scale: 1.,
            },
            loaded,
        )
    }

    /// The window's device pixels per logical pixel, used to size every image.
    pub fn set_scale(&mut self, scale: f32) {
        self.scale = scale;
    }

    /// Artwork with enough pixels to cover `size` (logical pixels) on this
    /// display. A zero side is unconstrained. Until that size loads, a size
    /// already loaded for the URL stands in.
    pub fn get(&mut self, url: &str, size: Size<Pixels>) -> Option<Arc<RenderImage>> {
        let resolution = Resolution::cover(size, self.scale);
        self.get_resolution(url, resolution).or_else(|| {
            let shown = ArtworkRequest {
                url: url.to_owned(),
                resolution: *self.shown.get(url)?,
            };
            match self.cache.get(&shown)? {
                CachedArtwork::Loaded { image, .. } => Some(image.clone()),
                CachedArtwork::Failed(_) => None,
            }
        })
    }

    /// The blurred wash behind album and playlist headers, made once at decode
    /// time so drawing it costs one stretched texture per frame.
    pub fn get_wash(&mut self, url: &str) -> Option<Arc<RenderImage>> {
        self.get_resolution(url, Resolution::Wash)
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
        if loaded.image.is_some() && loaded.request.resolution != Resolution::Wash {
            self.shown
                .insert(loaded.request.url.clone(), loaded.request.resolution);
        }
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
            let Some(oldest) = self.recent.pop_front() else {
                break;
            };
            if let Some(CachedArtwork::Loaded { image, bytes }) = self.cache.remove(&oldest) {
                self.cache_bytes -= bytes;
                evicted.push(image);
            }
            if self.shown.get(&oldest.url) == Some(&oldest.resolution) {
                self.shown.remove(&oldest.url);
            }
        }
        evicted
    }
}

fn load(
    client: &reqwest::blocking::Client,
    request: &ArtworkRequest,
    disk: &Mutex<DiskCache>,
) -> anyhow::Result<RenderImage> {
    let sources = sources(&request.url, request.resolution);
    let (listed, better) = sources.split_last().context("Artwork has no source")?;
    better
        .iter()
        .find_map(|source| load_source(client, source, request.resolution, disk).ok())
        .map_or_else(|| load_source(client, listed, request.resolution, disk), Ok)
}

/// The URLs to try for `url` at `resolution`, best first. Google's image
/// service sizes art on request, as the web's `=w544-h544-l90-rj` URLs do.
/// Video stills come in fixed sizes, and the HD ones may not exist, so a
/// frame larger than the listed still tries them before the listed one.
fn sources(url: &str, resolution: Resolution) -> Vec<String> {
    let (width, height) = resolution.box_size();
    if let Some(sized) = google_sized(url, width, height) {
        return vec![sized];
    }
    let still = reqwest::Url::parse(url)
        .ok()
        .filter(|parsed| parsed.host_str() == Some("i.ytimg.com"))
        .and_then(
            |parsed| match parsed.path_segments()?.collect::<Vec<_>>()[..] {
                ["vi", id, file] => Some((id.to_owned(), file.to_owned())),
                _ => None,
            },
        );
    match still {
        Some((id, file))
            if width.max(height * 16 / 9) > LISTED_STILL_WIDTH
                && ["default", "mqdefault", "hqdefault", "sddefault", "hq720"]
                    .contains(&file.split('.').next().unwrap_or_default()) =>
        {
            vec![
                format!("https://i.ytimg.com/vi/{id}/maxresdefault.jpg"),
                format!("https://i.ytimg.com/vi/{id}/sddefault.jpg"),
                url.to_owned(),
            ]
        }
        _ => vec![url.to_owned()],
    }
}

/// Rewrites a Google image URL's size options (`w`, `h` or `s`) to cover a
/// box of `width` by `height` pixels at the URL's own aspect ratio, keeping
/// its crop and quality options.
fn google_sized(url: &str, width: u32, height: u32) -> Option<String> {
    let parsed = reqwest::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    if !(host.ends_with(".googleusercontent.com") || host.ends_with(".ggpht.com"))
        || parsed.query().is_some()
    {
        return None;
    }
    let (base, options) = url.rsplit_once('=')?;
    fn size(option: &str) -> Option<(char, u32)> {
        let mut chars = option.chars();
        let kind = chars
            .next()
            .filter(|kind| matches!(kind, 'w' | 'h' | 's'))?;
        let size = chars.as_str().parse().ok().filter(|size| *size > 0)?;
        Some((kind, size))
    }
    let (mut source_width, mut source_height) = (None, None);
    for (kind, size) in options.split('-').filter_map(size) {
        match kind {
            'w' => source_width = Some(size),
            'h' => source_height = Some(size),
            _ => (source_width, source_height) = (Some(size), Some(size)),
        }
    }
    let aspect = source_width? as f32 / source_height? as f32;
    let target_width = width.max((height as f32 * aspect).ceil() as u32);
    let target_height = (target_width as f32 / aspect).round() as u32;
    if target_width == 0 || target_height == 0 {
        return None;
    }
    let options = options
        .split('-')
        .map(|option| match size(option) {
            Some(('w', _)) => format!("w{target_width}"),
            Some(('h', _)) => format!("h{target_height}"),
            Some(_) => format!("s{}", target_width.max(target_height)),
            None => option.to_owned(),
        })
        .collect::<Vec<_>>()
        .join("-");
    Some(format!("{base}={options}"))
}

fn load_source(
    client: &reqwest::blocking::Client,
    url: &str,
    resolution: Resolution,
    disk: &Mutex<DiskCache>,
) -> anyhow::Result<RenderImage> {
    let parsed = reqwest::Url::parse(url)?;
    anyhow::ensure!(
        parsed.scheme() == "https"
            && parsed.host_str().is_some_and(|h| h == "i.ytimg.com"
                || h.ends_with(".googleusercontent.com")
                || h.ends_with(".ggpht.com")
                // Library art such as Liked Music and Episodes for Later.
                || (h == "www.gstatic.com" && parsed.path().starts_with("/youtube/media/ytm/"))
                // A radio's collage, which the service draws without cookies.
                || (h == "music.youtube.com" && parsed.path() == "/image/radioart")),
        "Unsupported artwork host"
    );
    let still = parsed.host_str() == Some("i.ytimg.com");
    let key = format!("{:x}", Sha1::digest(url.as_bytes()));
    let cached = disk
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .read(&key);
    if let Some(bytes) = cached {
        if let Ok(image) = decode(bytes, resolution, still) {
            return Ok(image);
        }
        disk.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&key);
    }
    let mut bytes = Vec::new();
    client
        .get(parsed)
        .send()?
        .error_for_status()?
        .take(DOWNLOAD_BYTES + 1)
        .read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() as u64 <= DOWNLOAD_BYTES,
        "Artwork response is too large"
    );
    let image = decode(bytes.clone(), resolution, still)?;
    disk.lock()
        .unwrap_or_else(PoisonError::into_inner)
        .write(&key, &bytes);
    Ok(image)
}

struct DiskEntry {
    bytes: u64,
    used: SystemTime,
}

struct DiskCache {
    directory: Option<PathBuf>,
    entries: HashMap<String, DiskEntry>,
    bytes: u64,
}

impl DiskCache {
    fn new(directory: Option<PathBuf>) -> Self {
        let mut cache = Self {
            directory,
            entries: HashMap::new(),
            bytes: 0,
        };
        if let Some(directory) = &cache.directory
            && fs::create_dir_all(directory).is_ok()
            && let Ok(entries) = fs::read_dir(directory)
        {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.len() != 40 || !name.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    continue;
                }
                let Ok(metadata) = entry.metadata() else {
                    continue;
                };
                if !metadata.is_file() {
                    continue;
                }
                let used = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                cache.bytes += metadata.len();
                cache.entries.insert(
                    name,
                    DiskEntry {
                        bytes: metadata.len(),
                        used,
                    },
                );
            }
        }
        cache.evict(DISK_BYTES);
        cache
    }

    fn read(&mut self, key: &str) -> Option<Vec<u8>> {
        let entry = self.entries.get_mut(key)?;
        if entry.bytes > DOWNLOAD_BYTES || entry.used.elapsed().unwrap_or_default() > DISK_MAX_AGE {
            self.remove(key);
            return None;
        }
        let file = File::open(self.directory.as_ref()?.join(key)).ok()?;
        let mut bytes = Vec::new();
        file.take(DOWNLOAD_BYTES + 1).read_to_end(&mut bytes).ok()?;
        if bytes.len() as u64 > DOWNLOAD_BYTES {
            return None;
        }
        entry.used = SystemTime::now();
        // Persist LRU recency across launches without rewriting encoded image bytes.
        let _ = File::open(self.directory.as_ref()?.join(key))
            .and_then(|file| file.set_modified(entry.used));
        Some(bytes)
    }

    fn write(&mut self, key: &str, bytes: &[u8]) {
        if let Some(directory) = &self.directory {
            let result = (|| -> std::io::Result<()> {
                let mut file = tempfile::NamedTempFile::new_in(directory)?;
                file.write_all(bytes)?;
                file.persist(directory.join(key))
                    .map_err(|error| error.error)?;
                Ok(())
            })();
            if result.is_err() {
                return;
            }
            if let Some(previous) = self.entries.insert(
                key.to_owned(),
                DiskEntry {
                    bytes: bytes.len() as u64,
                    used: SystemTime::now(),
                },
            ) {
                self.bytes -= previous.bytes;
            }
            self.bytes += bytes.len() as u64;
            self.evict(DISK_BYTES);
        }
    }

    fn remove(&mut self, key: &str) {
        if let Some(entry) = self.entries.remove(key) {
            self.bytes -= entry.bytes;
            if let Some(directory) = &self.directory {
                let _ = fs::remove_file(directory.join(key));
            }
        }
    }

    fn evict(&mut self, budget: u64) {
        let mut oldest = self
            .entries
            .iter()
            .map(|(key, entry)| (key.clone(), entry.used))
            .collect::<Vec<_>>();
        oldest.sort_unstable_by_key(|(_, used)| *used);
        for (key, used) in oldest {
            if self.bytes <= budget && used.elapsed().unwrap_or_default() <= DISK_MAX_AGE {
                break;
            }
            self.remove(&key);
        }
    }
}

/// Decodes at the requested size, never enlarging. A 4:3 video still is a
/// 16:9 frame letterboxed in black, so it loses the bars as on the web.
fn decode(bytes: Vec<u8>, resolution: Resolution, still: bool) -> anyhow::Result<RenderImage> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(4096);
    limits.max_image_height = Some(4096);
    limits.max_alloc = Some(64 * 1024 * 1024);
    reader.limits(limits);
    let mut image = reader.decode()?;
    if still && image.width() * 3 == image.height() * 4 {
        let height = image.width() * 9 / 16;
        image = image.crop_imm(0, (image.height() - height) / 2, image.width(), height);
    }
    let mut pixels = match resolution {
        Resolution::Cover { width, height } => {
            let scale =
                (width as f32 / image.width() as f32).max(height as f32 / image.height() as f32);
            if scale < 1. {
                image.thumbnail_exact(
                    ((image.width() as f32 * scale).round() as u32).max(1),
                    ((image.height() as f32 * scale).round() as u32).max(1),
                )
            } else {
                image
            }
        }
        Resolution::Wash => image.thumbnail(WASH, WASH),
    }
    .into_rgba8();
    if resolution == Resolution::Wash {
        // About the web's 80px blur once the wash is stretched across the page.
        pixels = image::imageops::blur(&pixels, 4.);
    }
    for pixel in pixels.pixels_mut() {
        pixel.0.swap(0, 2);
    }
    Ok(RenderImage::new(vec![image::Frame::new(pixels)]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{px, size};

    const ROW: Resolution = Resolution::Cover {
        width: 48,
        height: 48,
    };
    const HERO: Resolution = Resolution::Cover {
        width: 1536,
        height: 1536,
    };

    fn edge(pixels: f32) -> Size<Pixels> {
        size(px(pixels), px(pixels))
    }

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
                shown: HashMap::new(),
                scale: 1.,
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
    fn disk_cache_survives_restart_evicts_oldest_and_recovers_corruption() {
        let directory = tempfile::tempdir().unwrap();
        let a = "a".repeat(40);
        let b = "b".repeat(40);
        let mut disk = DiskCache::new(Some(directory.path().to_owned()));
        disk.write(&a, b"first");
        disk.write(&b, b"second");
        drop(disk);
        let mut disk = DiskCache::new(Some(directory.path().to_owned()));
        assert_eq!(disk.read(&a).unwrap(), b"first");
        disk.evict(6);
        assert!(disk.read(&a).is_some());
        assert!(disk.read(&b).is_none());
        assert!(disk.bytes <= 6);
        disk.entries.get_mut(&a).unwrap().used =
            SystemTime::now() - DISK_MAX_AGE - Duration::from_secs(1);
        assert!(disk.read(&a).is_none());
        assert!(!directory.path().join(a).exists());
    }

    #[test]
    #[ignore = "downloads three public thumbnails; warm restart has networking disabled"]
    fn live_artwork_warm_restart_avoids_downloads() {
        let directory = tempfile::tempdir().unwrap();
        let online = reqwest::blocking::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(15))
            .build()
            .unwrap();
        let disk = Mutex::new(DiskCache::new(Some(directory.path().to_owned())));
        let requests = ["default.jpg", "mqdefault.jpg", "hqdefault.jpg"]
            .map(|name| request(&format!("https://i.ytimg.com/vi/csILUDfmtqM/{name}"), ROW));
        for request in &requests {
            load(&online, request, &disk).unwrap();
        }
        assert_eq!(disk.lock().unwrap().entries.len(), 3);
        drop(disk);
        let offline = reqwest::blocking::Client::builder()
            .proxy(reqwest::Proxy::all("http://127.0.0.1:1").unwrap())
            .timeout(Duration::from_secs(1))
            .build()
            .unwrap();
        let disk = Mutex::new(DiskCache::new(Some(directory.path().to_owned())));
        for request in &requests {
            load(&offline, request, &disk).unwrap();
        }
        println!("cold_artwork_downloads=3 warm_restart_artwork_downloads=0");
        // A corrupt entry must be discarded, allowing a subsequent online retry.
        let key = format!("{:x}", Sha1::digest(requests[0].url.as_bytes()));
        fs::write(directory.path().join(&key), b"broken image").unwrap();
        assert!(load(&offline, &requests[0], &disk).is_err());
        load(&online, &requests[0], &disk).unwrap();
    }

    #[test]
    fn navigation_discards_queued_artwork_and_keeps_in_flight_downloads() {
        let (mut artwork, queued) = fixture();
        let mut in_flight = Vec::new();
        for index in 0..3 {
            artwork.get(&format!("in-flight-{index}"), edge(48.));
            in_flight.push(queued.try_recv().unwrap());
        }
        for index in 0..64 {
            artwork.get(&format!("old-page-{index}"), edge(48.));
        }
        artwork.get("new-page", edge(48.));
        assert_eq!(queued.len(), 64);
        assert!(!artwork.pending.contains(&request("new-page", ROW)));

        artwork.cancel_pending();
        assert!(queued.is_empty());
        assert_eq!(artwork.pending.len(), 3);
        artwork.get("new-page", edge(48.));
        assert_eq!(queued.try_recv().unwrap(), request("new-page", ROW));
        for request in &in_flight {
            artwork.get(&request.url, edge(48.));
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
        artwork.get("old-page-0", edge(48.));
        assert_eq!(queued.try_recv().unwrap().url, "old-page-0");
    }

    #[test]
    fn failed_artwork_retries_on_access_after_its_cooldown() {
        let (mut artwork, queued) = fixture();
        artwork.get("artwork", edge(48.));
        let request = queued.try_recv().unwrap();
        artwork.insert(LoadedArtwork {
            request: request.clone(),
            image: None,
        });
        for _ in 0..100 {
            assert!(artwork.get("artwork", edge(48.)).is_none());
        }
        assert!(queued.is_empty());

        let CachedArtwork::Failed(retry_at) = artwork.cache.get_mut(&request).unwrap() else {
            panic!("failed downloads must have a retry deadline");
        };
        *retry_at = Instant::now().checked_sub(Duration::from_secs(1)).unwrap();
        artwork.get("artwork", edge(48.));
        artwork.get("artwork", edge(48.));
        assert_eq!(queued.try_recv().unwrap(), request);
        assert!(queued.is_empty(), "only one retry may be in flight");

        let image = image(1, 1);
        artwork.insert(LoadedArtwork {
            request,
            image: Some(image.clone()),
        });
        assert!(Arc::ptr_eq(
            &artwork.get("artwork", edge(48.)).unwrap(),
            &image
        ));
        assert!(queued.is_empty());
    }

    #[test]
    fn thumbnail_and_hero_requests_are_deduplicated_at_their_own_resolution() {
        let (mut artwork, queued) = fixture();
        for _ in 0..10 {
            artwork.get("shared-url", edge(48.));
            artwork.get("shared-url", edge(1200.));
        }
        assert_eq!(artwork.pending.len(), 2);
        assert_eq!(queued.try_recv().unwrap(), request("shared-url", ROW));
        assert_eq!(queued.try_recv().unwrap(), request("shared-url", HERO));
        assert!(queued.is_empty());

        let thumbnail = image(320, 180);
        let hero = image(1600, 900);
        for (resolution, image) in [(ROW, thumbnail.clone()), (HERO, hero.clone())] {
            artwork.insert(LoadedArtwork {
                request: request("shared-url", resolution),
                image: Some(image),
            });
        }
        assert!(artwork.pending.is_empty());
        assert!(Arc::ptr_eq(
            &artwork.get("shared-url", edge(48.)).unwrap(),
            &thumbnail
        ));
        assert!(Arc::ptr_eq(
            &artwork.get("shared-url", edge(1200.)).unwrap(),
            &hero
        ));
        assert_eq!(artwork.cache.len(), 2);
        assert!(queued.is_empty());
    }

    #[test]
    fn entry_eviction_returns_decoded_images_for_gpu_cleanup() {
        let (mut artwork, _) = fixture();
        let image = image(1, 1);
        artwork.insert(LoadedArtwork {
            request: request("loaded", ROW),
            image: Some(image.clone()),
        });
        for index in 0..CACHE_ENTRIES - 1 {
            assert!(
                artwork
                    .insert(LoadedArtwork {
                        request: request(&format!("failed-{index}"), ROW),
                        image: None,
                    })
                    .is_empty()
            );
        }
        let evicted = artwork.insert(LoadedArtwork {
            request: request("new", ROW),
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
                        request: request(&index.to_string(), ROW),
                        image: Some(image.clone()),
                    })
                    .is_empty()
            );
        }
        artwork.get("0", edge(48.));
        let hero = image(1600, 900);
        let evicted = artwork.insert(LoadedArtwork {
            request: request("hero", HERO),
            image: Some(hero.clone()),
        });
        assert_eq!(evicted.len(), 8);
        for (evicted, expected) in evicted.iter().zip(&thumbnails[1..9]) {
            assert!(Arc::ptr_eq(evicted, expected));
        }
        assert!(Arc::ptr_eq(
            &artwork.get("0", edge(48.)).unwrap(),
            &thumbnails[0]
        ));
        assert!(Arc::ptr_eq(
            &artwork.get("hero", edge(1200.)).unwrap(),
            &hero
        ));
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
                    request: request("same", ROW),
                    image: Some(original.clone()),
                })
                .is_empty()
        );
        let evicted = artwork.insert(LoadedArtwork {
            request: request("same", ROW),
            image: Some(replacement.clone()),
        });
        assert_eq!(evicted.len(), 1);
        assert!(Arc::ptr_eq(&evicted[0], &original));
        assert_eq!(artwork.cache.len(), 1);
        assert_eq!(artwork.recent.len(), 1);
        assert_eq!(artwork.cache_bytes, 160 * 160 * 4);
        assert!(Arc::ptr_eq(
            &artwork.get("same", edge(48.)).unwrap(),
            &replacement
        ));
    }

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image::RgbaImage::from_fn(width, height, |_, y| {
            // A 4:3 still letterboxes its 16:9 frame in black.
            let bar = (height - width * 9 / 16) / 2;
            if y < bar || y >= height - bar {
                image::Rgba([0, 0, 0, 255])
            } else {
                image::Rgba([200, 100, 50, 255])
            }
        }))
        .write_to(&mut bytes, image::ImageFormat::Png)
        .unwrap();
        bytes.into_inner()
    }

    #[test]
    fn decoding_covers_the_shown_size_without_enlarging_or_letterboxing() {
        let size = |bytes: Vec<u8>, resolution, still| {
            let image = decode(bytes, resolution, still).unwrap();
            let size = image.size(0);
            (size.width.0, size.height.0)
        };
        // A square row covers its box with the frame's full height.
        assert_eq!(size(png(1920, 1080), ROW, false), (85, 48));
        assert_eq!(
            size(
                png(1920, 1080),
                Resolution::Cover {
                    width: 1536,
                    height: 0
                },
                false
            ),
            (1536, 864)
        );
        // Small art keeps its pixels; GPUI enlarges it as cheaply as the CPU.
        assert_eq!(size(png(120, 120), HERO, false), (120, 120));
        let image = decode(png(480, 360), ROW, true).unwrap();
        let size = image.size(0);
        assert_eq!((size.width.0, size.height.0), (85, 48));
        let pixels = image.as_bytes(0).unwrap();
        assert!(
            pixels.chunks(4).all(|pixel| pixel[2] > 150),
            "a letterboxed still must lose its black bars"
        );
    }

    #[test]
    fn sources_ask_for_the_pixels_each_surface_shows() {
        let cover = |edge: f32, scale| Resolution::cover(size(px(edge), px(edge)), scale);
        assert_eq!(cover(48., 1.), ROW);
        assert_eq!(
            cover(48., 2.),
            Resolution::Cover {
                width: 96,
                height: 96
            }
        );
        assert_eq!(
            cover(5000., 1.),
            Resolution::Cover {
                width: 2048,
                height: 2048
            }
        );
        let album = "https://lh3.googleusercontent.com/album=w120-h120-l90-rj";
        assert_eq!(
            sources(album, cover(48., 2.)),
            ["https://lh3.googleusercontent.com/album=w96-h96-l90-rj"]
        );
        // A banner keeps its crop's aspect ratio at the shown width.
        assert_eq!(
            sources(
                "https://lh3.googleusercontent.com/banner=w1080-h450-p-l90-rj",
                Resolution::cover(size(px(1440.), px(400.)), 1.)
            ),
            ["https://lh3.googleusercontent.com/banner=w1536-h640-p-l90-rj"]
        );
        assert_eq!(
            sources(
                "https://yt3.ggpht.com/avatar=s88-c-k-c0x00ffffff-no-rj",
                cover(36., 2.)
            ),
            ["https://yt3.ggpht.com/avatar=s96-c-k-c0x00ffffff-no-rj"]
        );
        let signed = "https://lh3.googleusercontent.com/art=w120-h120?sig=1";
        assert_eq!(sources(signed, HERO), [signed]);
        // Listed stills serve rows and cards; a larger frame tries the HD stills first.
        let still = "https://i.ytimg.com/vi/abcdefghijk/hqdefault.jpg?sqp=a&rs=b";
        assert_eq!(sources(still, cover(240., 1.)), [still]);
        assert_eq!(
            sources(still, cover(400., 1.)),
            [
                "https://i.ytimg.com/vi/abcdefghijk/maxresdefault.jpg",
                "https://i.ytimg.com/vi/abcdefghijk/sddefault.jpg",
                still
            ]
        );
        let library = "https://www.gstatic.com/youtube/media/ytm/images/pbg/liked-music.png";
        assert_eq!(sources(library, HERO), [library]);
    }

    #[test]
    fn a_new_size_shows_the_loaded_size_until_it_arrives() {
        let (mut artwork, queued) = fixture();
        artwork.get("art", edge(48.));
        let row = image(48, 48);
        artwork.insert(LoadedArtwork {
            request: queued.try_recv().unwrap(),
            image: Some(row.clone()),
        });
        let shown = artwork.get("art", edge(1200.)).unwrap();
        assert!(Arc::ptr_eq(&shown, &row));
        let hero = queued.try_recv().unwrap();
        assert_eq!(hero, request("art", HERO));
        let large = image(1536, 1536);
        artwork.insert(LoadedArtwork {
            request: hero,
            image: Some(large.clone()),
        });
        assert!(Arc::ptr_eq(
            &artwork.get("art", edge(1200.)).unwrap(),
            &large
        ));
        assert!(Arc::ptr_eq(&artwork.get("art", edge(48.)).unwrap(), &row));
        assert!(
            artwork.get_wash("art").is_none(),
            "a wash is never the sharp art"
        );
    }
}
