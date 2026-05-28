//! Colour-coded sparkline of recent builds, used in repo header rows and the
//! build-times popup. Pure rendering — no app state required.

use ratatui::style::{Color, Style};
use ratatui::text::Span;

use build_watcher::status::{BuildSample, RunConclusion};

use super::{COLOR_FAILURE, COLOR_SUCCESS};

/// Block characters used by the sparkline, low → high.
const SPARK_BLOCKS: &[char] = &[
    '\u{2581}', '\u{2582}', '\u{2583}', '\u{2584}', '\u{2585}', '\u{2586}', '\u{2587}', '\u{2588}',
];

/// Colour for a sparkline bar based on the build's conclusion.
fn spark_color(conclusion: &RunConclusion) -> Color {
    match conclusion {
        RunConclusion::Success => COLOR_SUCCESS,
        RunConclusion::Failure | RunConclusion::TimedOut | RunConclusion::StartupFailure => {
            COLOR_FAILURE
        }
        RunConclusion::Cancelled => Color::Yellow,
        RunConclusion::Unknown => Color::DarkGray,
    }
}

/// Colour-coded sparkline of recent builds.
///
/// Input is newest-first; output is rendered oldest-on-the-left so the
/// rightmost bar represents the most recent build (standard time-axis
/// convention). Each bar is coloured by conclusion — green for Success,
/// red for Failure family, yellow for Cancelled, gray for Unknown — so a
/// glance at the row tells you both runtime variance *and* the pass/fail
/// pattern over the window.
///
/// Returns an empty Vec for fewer than 2 samples (no trend possible).
/// When all durations are identical, every bar renders at the mid-level block.
pub(crate) fn sparkline(samples_newest_first: &[BuildSample]) -> Vec<Span<'static>> {
    if samples_newest_first.len() < 2 {
        return Vec::new();
    }
    let min = samples_newest_first
        .iter()
        .map(|s| s.duration_secs)
        .min()
        .unwrap();
    let max = samples_newest_first
        .iter()
        .map(|s| s.duration_secs)
        .max()
        .unwrap();
    let span = max.saturating_sub(min);
    let mid = SPARK_BLOCKS.len() / 2;
    let max_idx = SPARK_BLOCKS.len() as u64 - 1;
    samples_newest_first
        .iter()
        .rev() // oldest first for display
        .map(|sample| {
            let block = match (sample.duration_secs - min)
                .checked_mul(max_idx)
                .and_then(|n| n.checked_div(span))
            {
                Some(idx) => SPARK_BLOCKS[idx as usize],
                None => SPARK_BLOCKS[mid], // identical samples → mid-level
            };
            Span::styled(
                block.to_string(),
                Style::default().fg(spark_color(&sample.conclusion)),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(duration_secs: u64, conclusion: RunConclusion) -> BuildSample {
        BuildSample {
            duration_secs,
            conclusion,
        }
    }

    fn sample_chars(spans: &[Span<'_>]) -> Vec<char> {
        spans
            .iter()
            .map(|sp| sp.content.chars().next().unwrap())
            .collect()
    }

    #[test]
    fn sparkline_empty_for_too_few_samples() {
        assert!(sparkline(&[]).is_empty());
        assert!(sparkline(&[s(100, RunConclusion::Success)]).is_empty());
    }

    #[test]
    fn sparkline_renders_one_span_per_sample() {
        let spans = sparkline(&[
            s(100, RunConclusion::Success),
            s(200, RunConclusion::Success),
            s(150, RunConclusion::Success),
            s(175, RunConclusion::Success),
        ]);
        assert_eq!(spans.len(), 4);
    }

    #[test]
    fn sparkline_handles_identical_samples_without_panic() {
        // All same → no span, every bar at mid level. The key thing is no
        // divide-by-zero. Result is non-empty (≥2 samples).
        let spans = sparkline(&[
            s(100, RunConclusion::Success),
            s(100, RunConclusion::Success),
            s(100, RunConclusion::Success),
        ]);
        assert_eq!(spans.len(), 3);
    }

    #[test]
    fn sparkline_renders_oldest_on_left() {
        // Input is newest-first; render should reverse so the rightmost bar
        // is the most recent. With samples [200, 100] (newest=200, oldest=100),
        // the rightmost bar should be at the high block, leftmost at the low.
        let spans = sparkline(&[
            s(200, RunConclusion::Success),
            s(100, RunConclusion::Success),
        ]);
        assert_eq!(spans.len(), 2);
        let chars = sample_chars(&spans);
        assert_eq!(chars[0], SPARK_BLOCKS[0], "leftmost = oldest = min");
        assert_eq!(chars[1], SPARK_BLOCKS[7], "rightmost = newest = max");
    }

    #[test]
    fn sparkline_colors_each_bar_by_conclusion() {
        let spans = sparkline(&[
            s(100, RunConclusion::Success),
            s(200, RunConclusion::Failure),
            s(150, RunConclusion::Cancelled),
        ]);
        // Rendered oldest-on-left, so order in output is reversed from input:
        //   spans[0] = Cancelled, spans[1] = Failure, spans[2] = Success
        assert_eq!(spans[0].style.fg, Some(Color::Yellow), "cancelled = yellow");
        assert_eq!(spans[1].style.fg, Some(COLOR_FAILURE), "failure = red");
        assert_eq!(spans[2].style.fg, Some(COLOR_SUCCESS), "success = green");
    }

    #[test]
    fn sparkline_groups_failure_family_as_red() {
        let spans = sparkline(&[
            s(100, RunConclusion::Failure),
            s(200, RunConclusion::TimedOut),
            s(150, RunConclusion::StartupFailure),
        ]);
        for sp in &spans {
            assert_eq!(sp.style.fg, Some(COLOR_FAILURE));
        }
    }
}
