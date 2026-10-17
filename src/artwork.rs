use gpui::RenderImage;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    io::Read,
    sync::Arc,
    thread,
    time::Duration,
};

pub struct LoadedArtwork {
    pub url: String,
    pub image: Option<Arc<RenderImage>>,
}

pub struct Artwork {
    requests: async_channel::Sender<String>,
    pending: HashSet<String>,
    cache: HashMap<String, Option<Arc<RenderImage>>>,
    recent: VecDeque<String>,
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
                pending: HashSet::new(),
                cache: HashMap::new(),
                recent: VecDeque::new(),
            },
            loaded,
        )
    }

    pub fn get(&mut self, url: &str) -> Option<Arc<RenderImage>> {
        if let Some(image) = self.cache.get(url) {
            self.recent.retain(|key| key != url);
            self.recent.push_back(url.to_owned());
            return image.clone();
        }
        if !self.pending.contains(url) && self.requests.try_send(url.to_owned()).is_ok() {
            self.pending.insert(url.to_owned());
        }
        None
    }

    pub fn insert(&mut self, loaded: LoadedArtwork) -> Option<Arc<RenderImage>> {
        self.pending.remove(&loaded.url);
        self.recent.push_back(loaded.url.clone());
        self.cache.insert(loaded.url, loaded.image);
        if self.cache.len() > 96
            && let Some(oldest) = self.recent.pop_front()
        {
            return self.cache.remove(&oldest).flatten();
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
