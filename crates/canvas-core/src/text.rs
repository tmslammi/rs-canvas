//! Text measurement and layout.
//!
//! The core never assumes a font is available: everything goes through the
//! `TextMeasurer` seam. The wasm façade implements it over `measureText`;
//! tests and headless use get a deterministic approximation.

/// Measures text in device-independent units for a given font size.
pub trait TextMeasurer {
    fn measure_width(&self, text: &str, font_size: f64) -> f64;

    fn line_height(&self, font_size: f64) -> f64 {
        font_size * 1.3
    }
}

/// A deterministic heuristic measurer (no font required).
#[derive(Clone, Copy, Debug, Default)]
pub struct ApproxMeasurer;

impl TextMeasurer for ApproxMeasurer {
    fn measure_width(&self, text: &str, font_size: f64) -> f64 {
        text.chars().count() as f64 * font_size * 0.55
    }
}

/// A laid-out text block.
#[derive(Clone, Debug, PartialEq)]
pub struct TextLayout {
    pub lines: Vec<String>,
    pub width: f64,
    pub height: f64,
    pub line_height: f64,
}

/// Lay out text: split on newlines, wrap to `max_width` when given.
pub fn layout_text(
    text: &str,
    font_size: f64,
    max_width: Option<f64>,
    measurer: &dyn TextMeasurer,
) -> TextLayout {
    let line_height = measurer.line_height(font_size);
    let mut lines: Vec<String> = Vec::new();
    for paragraph in text.split('\n') {
        match max_width {
            Some(max) if max > 1.0 => {
                lines.extend(wrap_paragraph(paragraph, font_size, max, measurer));
            }
            _ => lines.push(paragraph.to_string()),
        }
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    let content_width = lines
        .iter()
        .map(|line| measurer.measure_width(line, font_size))
        .fold(0.0, f64::max);
    let width = match max_width {
        Some(max) if max > 1.0 => max,
        _ => content_width,
    };
    TextLayout {
        height: lines.len() as f64 * line_height,
        width,
        lines,
        line_height,
    }
}

fn wrap_paragraph(
    paragraph: &str,
    font_size: f64,
    max_width: f64,
    measurer: &dyn TextMeasurer,
) -> Vec<String> {
    if paragraph.is_empty() {
        return vec![String::new()];
    }
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut candidate = String::new();
    for word in paragraph.split_whitespace() {
        if current.is_empty() {
            if measurer.measure_width(word, font_size) <= max_width {
                current.push_str(word);
            } else {
                let mut chunk = String::new();
                for ch in word.chars() {
                    candidate.clear();
                    candidate.push_str(&chunk);
                    candidate.push(ch);
                    if !chunk.is_empty()
                        && measurer.measure_width(&candidate, font_size) > max_width
                    {
                        lines.push(std::mem::take(&mut chunk));
                    }
                    chunk.push(ch);
                }
                current = chunk;
            }
            continue;
        }
        candidate.clear();
        candidate.push_str(&current);
        candidate.push(' ');
        candidate.push_str(word);
        if measurer.measure_width(&candidate, font_size) <= max_width {
            std::mem::swap(&mut current, &mut candidate);
        } else {
            lines.push(std::mem::take(&mut current));
            current.push_str(word);
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_width_is_content_width() {
        let layout = layout_text("hello", 20.0, None, &ApproxMeasurer);
        assert_eq!(layout.lines, vec!["hello"]);
        assert!((layout.width - 5.0 * 20.0 * 0.55).abs() < 1e-9);
        assert!((layout.height - 26.0).abs() < 1e-9);
    }

    #[test]
    fn newlines_are_preserved() {
        let layout = layout_text("a\n\nb", 10.0, None, &ApproxMeasurer);
        assert_eq!(layout.lines, vec!["a", "", "b"]);
        assert_eq!(layout.lines.len(), 3);
    }

    #[test]
    fn wraps_at_word_boundaries() {
        let measured = |text: &str| ApproxMeasurer.measure_width(text, 20.0);
        let text = "one two three four five six";
        let max = measured("one two three");
        let layout = layout_text(text, 20.0, Some(max), &ApproxMeasurer);
        assert!(layout.lines.len() > 1);
        for line in &layout.lines {
            assert!(measured(line) <= max + 1e-9, "line too wide: {line}");
        }
        assert_eq!(layout.width, max);
    }

    #[test]
    fn hard_breaks_very_long_words() {
        let layout = layout_text(
            "supercalifragilisticexpialidocious",
            20.0,
            Some(60.0),
            &ApproxMeasurer,
        );
        assert!(layout.lines.len() > 1);
        for line in &layout.lines {
            assert!(ApproxMeasurer.measure_width(line, 20.0) <= 60.0 + 1e-9);
        }
    }
}
