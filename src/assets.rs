use gpui::{AssetSource, SharedString};
use std::borrow::Cow;
pub struct Assets;
impl AssetSource for Assets {
    fn load(&self, path: &str) -> anyhow::Result<Option<Cow<'static, [u8]>>> {
        Ok(match path {
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
            "history" => Some(Cow::Borrowed(include_bytes!("../assets/icons/history.svg"))),
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
            "settings" => Some(Cow::Borrowed(include_bytes!(
                "../assets/icons/settings.svg"
            ))),
            "help" => Some(Cow::Borrowed(include_bytes!("../assets/icons/help.svg"))),
            _ => None,
        })
    }
    fn list(&self, _: &str) -> anyhow::Result<Vec<SharedString>> {
        Ok(Vec::new())
    }
}
