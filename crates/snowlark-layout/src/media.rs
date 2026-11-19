// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Media elements' content (HTML §4.8.11, §15.4.?): a `<video>` shows its poster (an image, in
//! [`ReplacedContent::image`]) or its current frame (an [`snowlark_paint::Item::Video`] the
//! compositor fills), and a media element with the `controls` attribute shows the user agent's
//! controls over it: play/pause, the time, a seek bar, mute and volume, and (later) fullscreen.
//! [`controls_hit`] says which control a point of the content box is on, for the page's input.

use snowlark_paint::{
    Color, DisplayListBuilder, FontQuery, FontStretch, FontStyle, GradientStop, Point, Radii, Rect,
    ShapeOptions, ShapedText, Size, TextEngine, VideoId,
};

/// What a media element shows besides its poster.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MediaContent {
    /// The video's frames (the compositor draws them), when it shows a frame.
    pub video: Option<VideoId>,
    /// The frame copied to memory, for rasterizers without the compositor (headless renders).
    pub frame_image: Option<snowlark_paint::ImageId>,
    /// The user agent's controls (the `controls` attribute).
    pub controls: Option<MediaControls>,
}

/// The state the controls show.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MediaControls {
    pub paused: bool,
    /// Seconds.
    pub time: f64,
    /// Seconds (NaN: unknown; infinite: a stream).
    pub duration: f64,
    /// How much is buffered from the current position, as a fraction of the duration.
    pub buffered: f32,
    pub muted: bool,
    pub volume: f32,
    /// An `<audio>` element (the controls are all it shows).
    pub audio: bool,
    /// The pointer is over the element, or it is paused: the bar shows.
    pub visible: bool,
}

/// The height of the control bar (CSS px).
pub const BAR_HEIGHT: f32 = 36.0;

/// A part of the controls.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ControlsPart {
    PlayPause,
    /// The seek bar, at this fraction of the duration.
    Seek(f32),
    Mute,
    /// The volume slider, at this level.
    Volume(f32),
    /// Elsewhere on the bar (nothing happens).
    Bar,
    /// The picture above the bar (toggles play, as in Chrome).
    Picture,
}

/// Where each control is, in content-box coordinates.
struct Geometry {
    bar: Rect,
    play: Rect,
    time_x: f32,
    seek: Rect,
    mute: Rect,
    volume: Rect,
}

fn geometry(size: Size, audio: bool) -> Geometry {
    let h = BAR_HEIGHT.min(size.height);
    let bar = if audio {
        Rect::new(0.0, (size.height - h) / 2.0, size.width, h)
    } else {
        Rect::new(0.0, size.height - h, size.width, h)
    };
    let cy = bar.y + bar.height / 2.0;
    let play = Rect::new(bar.x + 6.0, cy - 12.0, 24.0, 24.0);
    let time_x = play.x + play.width + 6.0;
    // Right to left: volume slider, mute.
    let vol_w = if size.width >= 260.0 { 56.0 } else { 0.0 };
    let volume = Rect::new(bar.x + bar.width - 8.0 - vol_w, cy - 6.0, vol_w, 12.0);
    let mute = Rect::new(volume.x - 4.0 - 24.0, cy - 12.0, 24.0, 24.0);
    // The seek bar takes what is left after the time label (about 90 px).
    let seek_x = time_x + 92.0;
    let seek = Rect::new(seek_x, cy - 8.0, (mute.x - 8.0 - seek_x).max(0.0), 16.0);
    Geometry {
        bar,
        play,
        time_x,
        seek,
        mute,
        volume,
    }
}

/// Which part of the controls `p` (content-box coordinates) is on, for a content box of `size`.
pub fn controls_hit(size: Size, audio: bool, p: Point) -> Option<ControlsPart> {
    if p.x < 0.0 || p.y < 0.0 || p.x > size.width || p.y > size.height {
        return None;
    }
    let g = geometry(size, audio);
    let inside =
        |r: &Rect| p.x >= r.x && p.x <= r.x + r.width && p.y >= r.y && p.y <= r.y + r.height;
    if !inside(&g.bar) {
        return Some(ControlsPart::Picture);
    }
    let frac = |r: &Rect| ((p.x - r.x) / r.width.max(1.0)).clamp(0.0, 1.0);
    let wide = |r: &Rect| r.inflate(0.0, 6.0);
    Some(if inside(&g.play) {
        ControlsPart::PlayPause
    } else if g.seek.width > 0.0 && inside(&wide(&g.seek)) {
        ControlsPart::Seek(frac(&g.seek))
    } else if inside(&g.mute) {
        ControlsPart::Mute
    } else if g.volume.width > 0.0 && inside(&wide(&g.volume)) {
        ControlsPart::Volume(frac(&g.volume))
    } else {
        ControlsPart::Bar
    })
}

/// `m:ss` (or `h:mm:ss`) of a time in seconds.
pub fn format_time(t: f64) -> String {
    if !t.is_finite() || t < 0.0 {
        return "0:00".into();
    }
    let s = t.floor() as u64;
    let (h, m, s) = (s / 3600, (s / 60) % 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

fn shape(
    text: &TextEngine,
    value: &str,
    size: f32,
) -> Option<(ShapedText, snowlark_paint::FontId)> {
    shape_in(text, &["Segoe UI", "Arial", "sans-serif"], value, size)
}

fn shape_in(
    text: &TextEngine,
    families: &[&str],
    value: &str,
    size: f32,
) -> Option<(ShapedText, snowlark_paint::FontId)> {
    let list = text
        .font_list(&FontQuery {
            families,
            weight: 400,
            style: FontStyle::Normal,
            stretch: FontStretch::NORMAL,
        })
        .ok()?;
    let mut out = ShapedText::default();
    text.shape(
        value,
        list,
        &ShapeOptions {
            size,
            ..ShapeOptions::default()
        },
        &mut out,
    )
    .ok()?;
    Some((out, text.primary_font(list)))
}

/// The Windows icon fonts' media glyphs (Segoe Fluent Icons, and Segoe MDL2 Assets before it).
const ICON_FONTS: &[&str] = &["Segoe Fluent Icons", "Segoe MDL2 Assets"];
const PLAY: &str = "\u{E768}";
const PAUSE: &str = "\u{E769}";
const VOLUME: &str = "\u{E767}";
const MUTE: &str = "\u{E74F}";

/// An icon glyph centered in `r`.
fn icon(out: &mut DisplayListBuilder, text: &TextEngine, glyph: &str, r: Rect, color: Color) {
    if let Some((shaped, _)) = shape_in(text, ICON_FONTS, glyph, 16.0) {
        let w = shaped.width();
        shaped.paint(
            0..shaped.clusters.len(),
            Point::new(r.x + (r.width - w) / 2.0, r.y + r.height / 2.0 + 8.0),
            color,
            out,
        );
    }
}

/// Paints a media element's content: the frame (into `rect`, clipped to `clip`, the content box,
/// both in the list's coordinates) and its controls.
pub fn paint(
    out: &mut DisplayListBuilder,
    text: &TextEngine,
    media: &MediaContent,
    rect: Rect,
    clip: Rect,
) {
    if let Some(video) = media.video {
        out.video(video, media.frame_image, rect, clip);
    }
    let Some(c) = &media.controls else {
        return;
    };
    if !c.visible && !c.audio {
        return;
    }
    let g = geometry(Size::new(clip.width, clip.height), c.audio);
    let o = |r: Rect| r.translate(clip.x, clip.y);
    let white = Color::rgb(255, 255, 255);
    out.push_clip_rect(clip);
    if c.audio {
        out.fill_rounded_rect(
            o(g.bar),
            Radii::uniform(g.bar.height / 2.0),
            Color::rgb(0xF1, 0xF3, 0xF4),
        );
    } else {
        // A shade up from the bottom, so the white controls read on any picture.
        let bar = o(Rect::new(
            g.bar.x,
            g.bar.y - 24.0,
            g.bar.width,
            g.bar.height + 24.0,
        ));
        out.linear_gradient(
            bar,
            Radii::ZERO,
            Point::new(bar.x, bar.y),
            Point::new(bar.x, bar.y + bar.height),
            &[
                GradientStop {
                    offset: 0.0,
                    color: Color::rgba(0, 0, 0, 0),
                },
                GradientStop {
                    offset: 1.0,
                    color: Color::rgba(0, 0, 0, 160),
                },
            ],
            snowlark_paint::Extend::Clamp,
        );
    }
    let fg = if c.audio {
        Color::rgb(0x20, 0x21, 0x24)
    } else {
        white
    };
    let track_bg = if c.audio {
        Color::rgba(0, 0, 0, 60)
    } else {
        Color::rgba(255, 255, 255, 90)
    };
    // Play or pause.
    icon(
        out,
        text,
        if c.paused { PLAY } else { PAUSE },
        o(g.play),
        fg,
    );
    // The time.
    let label = if c.duration.is_finite() {
        format!("{} / {}", format_time(c.time), format_time(c.duration))
    } else {
        format_time(c.time)
    };
    if let Some((shaped, _)) = shape(text, &label, 12.0) {
        let baseline = clip.y + g.bar.y + g.bar.height / 2.0 + 4.0;
        shaped.paint(
            0..shaped.clusters.len(),
            Point::new(clip.x + g.time_x, baseline),
            fg,
            out,
        );
    }
    // The seek bar: the track, what is buffered, the played part and the thumb.
    if g.seek.width > 0.0 {
        let s = o(g.seek);
        let track = Rect::new(s.x, s.y + 6.0, s.width, 4.0);
        out.fill_rounded_rect(track, Radii::uniform(2.0), track_bg);
        let played = if c.duration.is_finite() && c.duration > 0.0 {
            (c.time / c.duration).clamp(0.0, 1.0) as f32
        } else {
            0.0
        };
        let buffered = c.buffered.clamp(played, 1.0);
        out.fill_rounded_rect(
            Rect::new(track.x, track.y, track.width * buffered, 4.0),
            Radii::uniform(2.0),
            if c.audio {
                Color::rgba(0, 0, 0, 90)
            } else {
                Color::rgba(255, 255, 255, 140)
            },
        );
        out.fill_rounded_rect(
            Rect::new(track.x, track.y, track.width * played, 4.0),
            Radii::uniform(2.0),
            fg,
        );
        let tx = track.x + track.width * played;
        out.fill_rounded_rect(
            Rect::new(tx - 6.0, track.y - 4.0, 12.0, 12.0),
            Radii::uniform(6.0),
            fg,
        );
    }
    // Mute: a speaker, struck through when muted.
    let muted = c.muted || c.volume <= 0.0;
    icon(out, text, if muted { MUTE } else { VOLUME }, o(g.mute), fg);
    if g.volume.width > 0.0 {
        let v = o(g.volume);
        let level = if c.muted {
            0.0
        } else {
            c.volume.clamp(0.0, 1.0)
        };
        let track = Rect::new(v.x, v.y + 4.0, v.width, 4.0);
        out.fill_rounded_rect(track, Radii::uniform(2.0), track_bg);
        out.fill_rounded_rect(
            Rect::new(track.x, track.y, track.width * level, 4.0),
            Radii::uniform(2.0),
            fg,
        );
    }
    out.pop_clip();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times() {
        assert_eq!(format_time(0.0), "0:00");
        assert_eq!(format_time(65.4), "1:05");
        assert_eq!(format_time(3725.0), "1:02:05");
        assert_eq!(format_time(f64::NAN), "0:00");
    }

    #[test]
    fn hits() {
        let size = Size::new(640.0, 360.0);
        assert_eq!(
            controls_hit(size, false, Point::new(300.0, 100.0)),
            Some(ControlsPart::Picture)
        );
        assert_eq!(
            controls_hit(size, false, Point::new(18.0, 342.0)),
            Some(ControlsPart::PlayPause)
        );
        let g = geometry(size, false);
        let mid = Point::new(g.seek.x + g.seek.width / 2.0, g.seek.y + 8.0);
        match controls_hit(size, false, mid) {
            Some(ControlsPart::Seek(f)) => assert!((f - 0.5).abs() < 0.01),
            other => panic!("{other:?}"),
        }
        let mute = Point::new(g.mute.x + 12.0, g.mute.y + 12.0);
        assert_eq!(controls_hit(size, false, mute), Some(ControlsPart::Mute));
        assert_eq!(controls_hit(size, false, Point::new(-1.0, 5.0)), None);
        // An audio element's bar is centered.
        let audio = Size::new(300.0, 54.0);
        assert_eq!(
            controls_hit(audio, true, Point::new(18.0, 27.0)),
            Some(ControlsPart::PlayPause)
        );
    }

    #[test]
    fn paints_the_frame_then_the_controls() {
        let text = TextEngine::new().unwrap();
        let mut b = DisplayListBuilder::new();
        let content = MediaContent {
            video: Some(VideoId(3)),
            frame_image: None,
            controls: Some(MediaControls {
                paused: true,
                time: 5.0,
                duration: 60.0,
                buffered: 0.5,
                muted: false,
                volume: 1.0,
                audio: false,
                visible: true,
            }),
        };
        let r = Rect::new(10.0, 10.0, 640.0, 360.0);
        paint(&mut b, &text, &content, r, r);
        let list = b.build();
        assert!(matches!(
            list.items[0],
            snowlark_paint::Item::Video {
                video: VideoId(3),
                ..
            }
        ));
        assert!(list
            .items
            .iter()
            .any(|i| matches!(i, snowlark_paint::Item::Text(_))));
    }
}
