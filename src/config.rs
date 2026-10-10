//! User configuration, read from `$XDG_CONFIG_HOME/plush/config.toml`.
//!
//! Every key is optional: a missing file, or a missing key inside it, keeps the
//! bundled default. A file that *is* present but wrong is an error rather than a
//! silent fallback, so a typo cannot leave the daemon running with a look you did
//! not ask for. Unknown keys are rejected for the same reason.
//!
//! With no user file at all the `data/config.toml` shipped in this repository is
//! compiled in and used, so that file is the real default rather than a description
//! of the Rust one. A test holds it to the look plush had before any of it was
//! configurable.

use std::{
    env, fs, io,
    path::{Path, PathBuf},
};

use cosmic_text::{Family, Metrics, Weight};
use serde::Deserialize;

/// An 8-bit-per-channel colour, written as `#rgb`, `#rgba`, `#rrggbb` or `#rrggbbaa`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Rgba(pub u8, pub u8, pub u8, pub u8);

impl<'de> Deserialize<'de> for Rgba {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        parse_color(&raw).map_err(serde::de::Error::custom)
    }
}

fn parse_color(raw: &str) -> Result<Rgba, String> {
    let hex = raw
        .strip_prefix('#')
        .ok_or_else(|| format!("colour {raw:?} must start with '#'"))?;
    if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("colour {raw:?} has a digit outside 0-9a-f"));
    }

    let pair = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).expect("two hex digits");
    let single = |i: usize| u8::from_str_radix(&hex[i..=i], 16).expect("one hex digit");
    // A single digit repeats, so #abc is #aabbcc — the usual CSS shorthand.
    let widen = |i: usize| single(i) * 17;

    Ok(match hex.len() {
        3 => Rgba(widen(0), widen(1), widen(2), 255),
        4 => Rgba(widen(0), widen(1), widen(2), widen(3)),
        6 => Rgba(pair(0), pair(2), pair(4), 255),
        8 => Rgba(pair(0), pair(2), pair(4), pair(6)),
        digits => {
            return Err(format!(
                "colour {raw:?} has {digits} digits; expected 3, 4, 6 or 8"
            ))
        }
    })
}

/// A font weight, named rather than numbered.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FontWeight(pub Weight);

impl Default for FontWeight {
    fn default() -> Self {
        Self(Weight::NORMAL)
    }
}

impl<'de> Deserialize<'de> for FontWeight {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        let weight = match raw.as_str() {
            "thin" => Weight::THIN,
            "extra-light" => Weight::EXTRA_LIGHT,
            "light" => Weight::LIGHT,
            "normal" => Weight::NORMAL,
            "medium" => Weight::MEDIUM,
            "semibold" => Weight::SEMIBOLD,
            "bold" => Weight::BOLD,
            "extra-bold" => Weight::EXTRA_BOLD,
            "black" => Weight::BLACK,
            other => {
                return Err(serde::de::Error::custom(format!(
                    "unknown font weight {other:?}; expected one of thin, extra-light, light, \
                     normal, medium, semibold, bold, extra-bold, black"
                )))
            }
        };
        Ok(Self(weight))
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct Border {
    /// Stroke width in logical pixels. `0` draws no border at all.
    pub width: f32,
    pub color: Rgba,
}

impl Default for Border {
    fn default() -> Self {
        Self { width: 1.0, color: Rgba(255, 255, 255, 26) }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Background {
    pub color: Rgba,
}

impl Default for Background {
    fn default() -> Self {
        Self { color: Rgba(28, 28, 30, 214) }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct Font {
    /// Any installed family name, or one of the generic `sans-serif`, `serif`,
    /// `monospace`, `cursive`, `fantasy`.
    pub family: String,
    /// The app name line.
    pub app_size: f32,
    pub app_line_height: f32,
    pub app_weight: FontWeight,
    /// The summary line.
    pub title_size: f32,
    pub title_line_height: f32,
    pub title_weight: FontWeight,
    /// The body text.
    pub body_size: f32,
    pub body_line_height: f32,
    pub body_weight: FontWeight,
}

impl Default for Font {
    fn default() -> Self {
        Self {
            family: "sans-serif".to_string(),
            app_size: 11.0,
            app_line_height: 15.0,
            app_weight: FontWeight(Weight::MEDIUM),
            title_size: 14.0,
            title_line_height: 19.0,
            // Bold rather than semibold: fontconfig resolves weight 600 for the
            // generic sans family to a serif CJK face whose space glyph is
            // missing, which then falls back to a colour-emoji space seventeen
            // pixels wide. `semibold` is accepted, just not the default.
            title_weight: FontWeight(Weight::BOLD),
            body_size: 13.0,
            body_line_height: 18.0,
            body_weight: FontWeight(Weight::NORMAL),
        }
    }
}

impl Font {
    pub fn family(&self) -> Family<'_> {
        match self.family.as_str() {
            "sans-serif" => Family::SansSerif,
            "serif" => Family::Serif,
            "monospace" => Family::Monospace,
            "cursive" => Family::Cursive,
            "fantasy" => Family::Fantasy,
            name => Family::Name(name),
        }
    }

    pub fn app_metrics(&self) -> Metrics {
        Metrics::new(self.app_size, self.app_line_height)
    }

    pub fn title_metrics(&self) -> Metrics {
        Metrics::new(self.title_size, self.title_line_height)
    }

    pub fn body_metrics(&self) -> Metrics {
        Metrics::new(self.body_size, self.body_line_height)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Animation {
    /// Slide and fade each panel in from the right, and back out when it goes away.
    /// Off makes panels appear and vanish instantly.
    pub enabled: bool,
}

impl Default for Animation {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub border: Border,
    pub background: Background,
    pub font: Font,
    pub animation: Animation,
}

/// Where the configuration is looked for.
pub fn path() -> PathBuf {
    let base = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        // The XDG spec says a relative XDG_CONFIG_HOME is not valid and must be ignored.
        .filter(|dir| dir.is_absolute())
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_else(|| PathBuf::from(".config"));
    base.join("plush").join("config.toml")
}

/// The configuration shipped with plush, compiled into the binary.
///
/// When there is no user file this is what runs, so `data/config.toml` is the real
/// default rather than documentation of the Rust one.
const BUNDLED: &str = include_str!("../data/config.toml");

/// Reads the configuration, falling back to the bundled file when the user has none.
pub fn load() -> Result<Config, String> {
    load_from(&path())
}

fn load_from(path: &Path) -> Result<Config, String> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return bundled(),
        Err(err) => return Err(format!("{}: {err}", path.display())),
    };
    toml::from_str(&text).map_err(|err| format!("{}: {err}", path.display()))
}

fn bundled() -> Result<Config, String> {
    toml::from_str(BUNDLED).map_err(|err| format!("bundled config.toml is invalid: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colours_accept_every_shorthand() {
        assert_eq!(parse_color("#abc").unwrap(), Rgba(0xaa, 0xbb, 0xcc, 255));
        assert_eq!(parse_color("#abcd").unwrap(), Rgba(0xaa, 0xbb, 0xcc, 0xdd));
        assert_eq!(parse_color("#aabbcc").unwrap(), Rgba(0xaa, 0xbb, 0xcc, 255));
        assert_eq!(parse_color("#aabbccdd").unwrap(), Rgba(0xaa, 0xbb, 0xcc, 0xdd));
    }

    #[test]
    fn malformed_colours_are_rejected() {
        for bad in ["aabbcc", "#abcde", "#gggggg", "#", ""] {
            assert!(parse_color(bad).is_err(), "{bad:?} should not parse");
        }
    }

    #[test]
    fn partial_config_keeps_the_other_defaults() {
        let config: Config = toml::from_str("[border]\nwidth = 3.0\n").unwrap();
        assert_eq!(config.border.width, 3.0);
        assert_eq!(config.border.color, Border::default().color);
        assert_eq!(config.font.title_size, Font::default().title_size);
    }

    /// The bundled file is what runs when there is no user configuration, so it has to
    /// reproduce exactly the look plush had before any of this was configurable: a 1px
    /// white hairline, a translucent dark grey panel, and sans-serif at the original
    /// sizes and weights.
    #[test]
    fn the_bundled_default_is_the_original_look() {
        let bundled: Config = toml::from_str(BUNDLED).expect("data/config.toml must parse");
        assert_eq!(bundled, Config::default(), "a missing key must match the bundled value");

        assert_eq!(bundled.border.width, 1.0);
        assert_eq!(bundled.border.color, Rgba(255, 255, 255, 26));
        assert_eq!(bundled.background.color, Rgba(28, 28, 30, 214));
        assert_eq!(bundled.font.family, "sans-serif");
        assert_eq!(bundled.font.app_metrics(), Metrics::new(11.0, 15.0));
        assert_eq!(bundled.font.title_metrics(), Metrics::new(14.0, 19.0));
        assert_eq!(bundled.font.body_metrics(), Metrics::new(13.0, 18.0));
        assert_eq!(bundled.font.app_weight, FontWeight(Weight::MEDIUM));
        assert_eq!(bundled.font.title_weight, FontWeight(Weight::BOLD));
        assert_eq!(bundled.font.body_weight, FontWeight(Weight::NORMAL));
        assert!(bundled.animation.enabled, "animation is on out of the box");
    }

    #[test]
    fn animations_can_be_turned_off() {
        let config: Config = toml::from_str("[animation]\nenabled = false\n").unwrap();
        assert!(!config.animation.enabled);
        assert_eq!(config.border, Border::default(), "and nothing else moves");
    }

    #[test]
    fn with_no_user_file_the_bundled_default_applies() {
        let missing = env::temp_dir().join("plush-absent-config").join("config.toml");
        let config = load_from(&missing).expect("a missing file is not an error");
        assert_eq!(config, Config::default());
    }

    #[test]
    fn a_user_file_wins_over_the_bundled_default() {
        let dir = env::temp_dir().join("plush-config-test");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");

        fs::write(&path, "[border]\nwidth = 7.0\n").unwrap();
        let config = load_from(&path).unwrap();
        assert_eq!(config.border.width, 7.0);
        assert_eq!(config.background.color, Rgba(28, 28, 30, 214), "untouched keys stay default");

        fs::write(&path, "[border]\nwidth = \"thick\"\n").unwrap();
        let err = load_from(&path).unwrap_err();
        assert!(err.contains(&path.display().to_string()), "the error must name the file: {err}");

        fs::remove_file(&path).unwrap();
    }

    #[test]
    fn typos_are_errors_rather_than_silent_no_ops() {
        let unknown_key: Result<Config, _> = toml::from_str("[border]\ncolour = \"#fff\"\n");
        assert!(unknown_key.is_err());

        let unknown_table: Result<Config, _> = toml::from_str("[panel]\nwidth = 400\n");
        assert!(unknown_table.is_err(), "panel geometry is not configurable");

        let bad_weight: Result<Config, _> = toml::from_str("[font]\ntitle-weight = \"heavyish\"\n");
        assert!(bad_weight.is_err());

        let bad_animation: Result<Config, _> = toml::from_str("[animation]\nspeed = \"fast\"\n");
        assert!(bad_animation.is_err(), "only `enabled` is available");
    }
}
