//! Font-backed Unicode advances. No platform font APIs or guessed widths.

use harfrust::{Buffer, Direction, Font, ShapeOptions, ShaperFont};
use unicode_bidi::BidiInfo;
use unicode_script::{Script, UnicodeScript};
use unicode_segmentation::UnicodeSegmentation;

fn strong_script(text: &str) -> Option<Script> {
    text.chars()
        .map(|c| c.script())
        .find(|script| !matches!(script, Script::Common | Script::Inherited | Script::Unknown))
}

// Itemize at grapheme boundaries: a combining mark or joined sequence must
// never become a separate font-shaping call just because its script differs.
fn script_runs(text: &str) -> Vec<(std::ops::Range<usize>, Script)> {
    let clusters: Vec<_> = text.grapheme_indices(true).collect();
    let mut result: Vec<(std::ops::Range<usize>, Script)> = Vec::new();
    let mut previous = None;
    for (index, &(start, cluster)) in clusters.iter().enumerate() {
        let script = strong_script(cluster)
            .or(previous)
            .or_else(|| {
                clusters[index + 1..]
                    .iter()
                    .find_map(|(_, c)| strong_script(c))
            })
            .unwrap_or(Script::Common);
        let end = start + cluster.len();
        if let Some((range, last)) = result.last_mut() {
            if *last == script {
                range.end = end;
                previous = Some(script);
                continue;
            }
        }
        result.push((start..end, script));
        previous = Some(script);
    }
    result
}

pub(super) fn advance(font: &Font, text: &str, size: f64) -> Result<f64, String> {
    if text.is_empty() {
        return Ok(0.0);
    }
    let units = font.units_per_em();
    if units == 0 {
        return Err("Font has invalid units per em".into());
    }
    let bidi = BidiInfo::new(text, None);
    let shaper = ShaperFont::new(font);
    let mut buffer = Buffer::new();
    let mut total = 0i64;
    for paragraph in &bidi.paragraphs {
        let (levels, runs) = bidi.visual_runs(paragraph, paragraph.range.clone());
        for run in runs {
            let direction = if levels[run.start].is_rtl() {
                Direction::RightToLeft
            } else {
                Direction::LeftToRight
            };
            let segment = &text[run];
            for (range, script) in script_runs(segment) {
                buffer.clear();
                buffer.push_str(&segment[range.clone()]);
                buffer.set_direction(direction);
                buffer.set_script(script.short_name().parse().ok());
                buffer.set_pre_context(&segment[..range.start]);
                buffer.set_post_context(&segment[range.end..]);
                harfrust::shape(&shaper, &mut buffer, ShapeOptions::new())
                    .map_err(|_| "Font shaping failed or exceeded its work budget")?;
                if buffer.glyph_infos().iter().any(|glyph| glyph.glyph_id == 0) {
                    return Err("Requested font does not contain a required glyph".into());
                }
                total += buffer
                    .glyph_positions()
                    .iter()
                    .map(|position| i64::from(position.x_advance))
                    .sum::<i64>()
                    .abs();
            }
        }
    }
    let width = total as f64 * size / f64::from(units);
    if !width.is_finite() || !(0.0..=4_194_304.0).contains(&width) {
        return Err("Invalid shaped text bounds".into());
    }
    Ok(width)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn latin() -> Font {
        Font::new(
            include_bytes!("../../../tests/fixtures/fonts/NotoSans-Regular.ttf").as_slice(),
            0,
        )
        .unwrap()
    }

    fn arabic() -> Font {
        Font::new(
            include_bytes!("../../../tests/fixtures/fonts/NotoNaskhArabic-Regular.ttf").as_slice(),
            0,
        )
        .unwrap()
    }

    #[test]
    fn kerning_marks_and_font_size_use_real_font_metrics() {
        let font = latin();
        let width = |text, size| advance(&font, text, size).unwrap();
        assert!(width("AV", 20.0) < width("A", 20.0) + width("V", 20.0));
        assert!((width("e\u{301}", 20.0) - width("é", 20.0)).abs() < 0.001);
        assert!((width("Hello", 40.0) - 2.0 * width("Hello", 20.0)).abs() < 0.001);
        assert!(width("WWWW", 20.0) > width("iiii", 20.0));
        assert!(advance(&font, "\u{10ffff}", 20.0).is_err());
    }

    #[test]
    fn advances_match_independent_coretext_reference_for_pinned_fonts() {
        // CTFontCreateWithGraphicsFont + CTLineGetTypographicBounds, measured
        // from these exact OFL fixtures on macOS on 2026-11-14. Windows runs
        // the same reference checks without linking any Apple framework.
        let font = latin();
        for (text, expected) in [
            ("Hello", 48.52),
            ("AV", 23.98),
            ("e\u{301}", 11.28),
            ("WWWW", 74.4),
        ] {
            let actual = advance(&font, text, 20.0).unwrap();
            assert!(
                (actual - expected).abs() < 0.0001,
                "{text}: {actual} != {expected}"
            );
        }
        let font = arabic();
        for (text, expected) in [("لا", 12.432), ("سلام 123 سلام", 136.128)] {
            let actual = advance(&font, text, 24.0).unwrap();
            assert!(
                (actual - expected).abs() < 0.0001,
                "{text}: {actual} != {expected}"
            );
        }
    }

    #[test]
    fn arabic_joining_and_bidirectional_runs_shape_in_logical_order() {
        let font = arabic();
        let joined = advance(&font, "لا", 24.0).unwrap();
        let separated = advance(&font, "ل", 24.0).unwrap() + advance(&font, "ا", 24.0).unwrap();
        assert!(joined < separated, "joined={joined}, separated={separated}");
        let mixed = advance(&font, "سلام 123 سلام", 24.0).unwrap();
        assert!(mixed > joined);
        let isolated = advance(&font, "\u{2067}سلام\u{2069}", 24.0).unwrap();
        assert_eq!(isolated, advance(&font, "سلام", 24.0).unwrap());
    }

    #[test]
    fn script_itemization_keeps_combining_clusters_intact() {
        let text = "a\u{301}日本語";
        let runs = script_runs(text);
        assert_eq!(&text[runs[0].0.clone()], "a\u{301}");
        assert_eq!(runs[0].1, Script::Latin);
        assert_eq!(runs[1].1, Script::Han);
    }
}
