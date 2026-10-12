//! Store a closed source format alongside player-authored text; never store rendered HTML.
use serde::{Deserialize, Serialize};
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum TextFormat {
    #[default]
    Markdown,
    Bbcode,
}
impl TextFormat {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Markdown => "markdown",
            Self::Bbcode => "bbcode",
        }
    }
}
