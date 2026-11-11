use gpui::{
    App, Asset, AssetSource, DevicePixels, ImageCacheError, RenderImage, SharedString, Size,
    SvgSize,
};
use std::{borrow::Cow, sync::Arc};
pub struct Assets;
impl AssetSource for Assets {
    fn load(&self, path: &str) -> anyhow::Result<Option<Cow<'static, [u8]>>> {
        Ok(match path {
            "sort" => Some(Cow::Borrowed(include_bytes!("../assets/icons/sort.svg"))),
            "comment-expand" => Some(Cow::Borrowed(include_bytes!(
                "../assets/icons/comment-expand.svg"
            ))),
            "comment-collapse" => Some(Cow::Borrowed(include_bytes!(
                "../assets/icons/comment-collapse.svg"
            ))),
            "play" => Some(Cow::Borrowed(include_bytes!("../assets/icons/play.svg"))),
            "pause" => Some(Cow::Borrowed(include_bytes!("../assets/icons/pause.svg"))),
            "next" => Some(Cow::Borrowed(include_bytes!("../assets/icons/next.svg"))),
            "previous" => Some(Cow::Borrowed(include_bytes!(
                "../assets/icons/previous.svg"
            ))),
            "home" => Some(Cow::Borrowed(include_bytes!("../assets/icons/home.svg"))),
            "explore" => Some(Cow::Borrowed(include_bytes!("../assets/icons/explore.svg"))),
            "search" => Some(Cow::Borrowed(include_bytes!("../assets/icons/search.svg"))),
            "heart" => Some(Cow::Borrowed(include_bytes!("../assets/icons/heart.svg"))),
            "heart-filled" => Some(Cow::Borrowed(include_bytes!(
                "../assets/icons/heart-filled.svg"
            ))),
            "library" => Some(Cow::Borrowed(include_bytes!("../assets/icons/library.svg"))),
            "bookmark" => Some(Cow::Borrowed(include_bytes!(
                "../assets/icons/bookmark.svg"
            ))),
            "bookmark-filled" => Some(Cow::Borrowed(include_bytes!(
                "../assets/icons/bookmark-filled.svg"
            ))),
            "history" => Some(Cow::Borrowed(include_bytes!("../assets/icons/history.svg"))),
            "check" => Some(Cow::Borrowed(include_bytes!("../assets/icons/check.svg"))),
            "expand-more" => Some(Cow::Borrowed(include_bytes!(
                "../assets/icons/expand-more.svg"
            ))),
            "player-controls" => Some(Cow::Borrowed(include_bytes!(
                "../assets/icons/player-controls.svg"
            ))),
            "queue" => Some(Cow::Borrowed(include_bytes!("../assets/icons/queue.svg"))),
            "volume" => Some(Cow::Borrowed(include_bytes!("../assets/icons/volume.svg"))),
            "mute" => Some(Cow::Borrowed(include_bytes!("../assets/icons/mute.svg"))),
            "plus" => Some(Cow::Borrowed(include_bytes!("../assets/icons/plus.svg"))),
            "close" => Some(Cow::Borrowed(include_bytes!("../assets/icons/close.svg"))),
            "back" => Some(Cow::Borrowed(include_bytes!("../assets/icons/back.svg"))),
            "forward" => Some(Cow::Borrowed(include_bytes!("../assets/icons/forward.svg"))),
            "external" => Some(Cow::Borrowed(include_bytes!(
                "../assets/icons/external.svg"
            ))),
            "shuffle" => Some(Cow::Borrowed(include_bytes!("../assets/icons/shuffle.svg"))),
            "repeat" => Some(Cow::Borrowed(include_bytes!("../assets/icons/repeat.svg"))),
            "music" => Some(Cow::Borrowed(include_bytes!("../assets/icons/music.svg"))),
            "explicit" => Some(Cow::Borrowed(include_bytes!(
                "../assets/icons/explicit.svg"
            ))),
            "home-filled" => Some(Cow::Borrowed(include_bytes!(
                "../assets/icons/home-filled.svg"
            ))),
            "explore-filled" => Some(Cow::Borrowed(include_bytes!(
                "../assets/icons/explore-filled.svg"
            ))),
            "new-releases" => Some(Cow::Borrowed(include_bytes!(
                "../assets/icons/new-releases.svg"
            ))),
            "trending-up" => Some(Cow::Borrowed(include_bytes!(
                "../assets/icons/trending-up.svg"
            ))),
            "mood" => Some(Cow::Borrowed(include_bytes!("../assets/icons/mood.svg"))),
            "podcasts" => Some(Cow::Borrowed(include_bytes!(
                "../assets/icons/podcasts.svg"
            ))),
            "mix" => Some(Cow::Borrowed(include_bytes!("../assets/icons/mix.svg"))),
            "home-outlined" => Some(Cow::Borrowed(include_bytes!(
                "../assets/icons/home-outlined.svg"
            ))),
            "explore-outlined" => Some(Cow::Borrowed(include_bytes!(
                "../assets/icons/explore-outlined.svg"
            ))),
            "settings" => Some(Cow::Borrowed(include_bytes!(
                "../assets/icons/settings.svg"
            ))),
            "video" => Some(Cow::Borrowed(include_bytes!("../assets/icons/video.svg"))),
            "fullscreen" => Some(Cow::Borrowed(include_bytes!(
                "../assets/icons/fullscreen.svg"
            ))),
            "exit-fullscreen" => Some(Cow::Borrowed(include_bytes!(
                "../assets/icons/exit-fullscreen.svg"
            ))),
            "help" => Some(Cow::Borrowed(include_bytes!("../assets/icons/help.svg"))),
            "menu" => Some(Cow::Borrowed(include_bytes!("../assets/icons/menu.svg"))),
            "person" => Some(Cow::Borrowed(include_bytes!("../assets/icons/person.svg"))),
            "more" => Some(Cow::Borrowed(include_bytes!("../assets/icons/more.svg"))),
            "brand" => Some(Cow::Borrowed(include_bytes!("../assets/icons/brand.svg"))),
            "thumb-up" => Some(Cow::Borrowed(include_bytes!(
                "../assets/icons/thumb-up.svg"
            ))),
            "thumb-up-filled" => Some(Cow::Borrowed(include_bytes!(
                "../assets/icons/thumb-up-filled.svg"
            ))),
            "thumb-down" => Some(Cow::Borrowed(include_bytes!(
                "../assets/icons/thumb-down.svg"
            ))),
            "wordmark" => Some(Cow::Borrowed(include_bytes!(
                "../assets/icons/wordmark.svg"
            ))),
            "radio" => Some(Cow::Borrowed(include_bytes!("../assets/icons/radio.svg"))),
            "album" => Some(Cow::Borrowed(include_bytes!("../assets/icons/album.svg"))),
            "playlist-add" => Some(Cow::Borrowed(include_bytes!(
                "../assets/icons/playlist-add.svg"
            ))),
            "play-next" => Some(Cow::Borrowed(include_bytes!(
                "../assets/icons/play-next.svg"
            ))),
            "link" => Some(Cow::Borrowed(include_bytes!("../assets/icons/link.svg"))),
            "description" => Some(Cow::Borrowed(include_bytes!(
                "../assets/icons/description.svg"
            ))),
            "logout" => Some(Cow::Borrowed(include_bytes!("../assets/icons/logout.svg"))),
            "switch-account" => Some(Cow::Borrowed(include_bytes!(
                "../assets/icons/switch-account.svg"
            ))),
            "drag-handle" => Some(Cow::Borrowed(include_bytes!(
                "../assets/icons/drag-handle.svg"
            ))),
            "expand" => Some(Cow::Borrowed(include_bytes!("../assets/icons/expand.svg"))),
            "collapse" => Some(Cow::Borrowed(include_bytes!(
                "../assets/icons/collapse.svg"
            ))),
            "thumb-down-filled" => Some(Cow::Borrowed(include_bytes!(
                "../assets/icons/thumb-down-filled.svg"
            ))),
            "menu-mix" => Some(Cow::Borrowed(include_bytes!(
                "../assets/icons/menu-mix.svg"
            ))),
            "menu-play-next" => Some(Cow::Borrowed(include_bytes!(
                "../assets/icons/menu-play-next.svg"
            ))),
            "menu-add-queue" => Some(Cow::Borrowed(include_bytes!(
                "../assets/icons/menu-add-queue.svg"
            ))),
            "menu-library" => Some(Cow::Borrowed(include_bytes!(
                "../assets/icons/menu-library.svg"
            ))),
            "menu-like" => Some(Cow::Borrowed(include_bytes!(
                "../assets/icons/menu-like.svg"
            ))),
            "menu-save-playlist" => Some(Cow::Borrowed(include_bytes!(
                "../assets/icons/menu-save-playlist.svg"
            ))),
            "menu-remove" => Some(Cow::Borrowed(include_bytes!(
                "../assets/icons/menu-remove.svg"
            ))),
            "menu-album" => Some(Cow::Borrowed(include_bytes!(
                "../assets/icons/menu-album.svg"
            ))),
            "menu-artist" => Some(Cow::Borrowed(include_bytes!(
                "../assets/icons/menu-artist.svg"
            ))),
            "menu-credits" => Some(Cow::Borrowed(include_bytes!(
                "../assets/icons/menu-credits.svg"
            ))),
            "menu-share" => Some(Cow::Borrowed(include_bytes!(
                "../assets/icons/menu-share.svg"
            ))),
            "menu-delete" => Some(Cow::Borrowed(include_bytes!(
                "../assets/icons/menu-delete.svg"
            ))),
            "menu-up" => Some(Cow::Borrowed(include_bytes!("../assets/icons/menu-up.svg"))),
            "menu-down" => Some(Cow::Borrowed(include_bytes!(
                "../assets/icons/menu-down.svg"
            ))),
            _ => None,
        })
    }
    fn list(&self, _: &str) -> anyhow::Result<Vec<SharedString>> {
        Ok(Vec::new())
    }
}

/// Rasterize at device resolution to avoid resampling the logo's thin glyphs.
pub struct HeaderLogo;

impl Asset for HeaderLogo {
    type Source = Size<DevicePixels>;
    type Output = Result<Arc<RenderImage>, ImageCacheError>;

    fn load(
        size: Self::Source,
        cx: &mut App,
    ) -> impl Future<Output = Self::Output> + Send + 'static {
        let renderer = cx.svg_renderer();
        async move {
            let svg = renderer.parse_svg(include_bytes!("../assets/icons/logo.svg"))?;
            renderer
                .render_parsed(&svg, SvgSize::ExactSize(size))
                .map_err(Into::into)
        }
    }
}
