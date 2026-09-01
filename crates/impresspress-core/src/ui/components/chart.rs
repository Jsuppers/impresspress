//! Bar chart card. Moved from `blocks/admin/pages/dashboard.rs`.

use maud::{html, Markup};

/// Render a 30-day column bar chart card. `data` is ordered
/// chronologically; bars are normalized against the max count.
pub fn bar_chart_card(
    title: &str,
    subtitle: &str,
    data: &[(String, i64)],
    color_var: &str,
    view_href: &str,
) -> maud::Markup {
    let max = data.iter().map(|(_, v)| *v).max().unwrap_or(0).max(1);
    let fmt_short = |s: &str| -> String {
        chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
            .map(|d| d.format("%b %-d").to_string())
            .unwrap_or_else(|_| s.to_string())
    };
    let first_label = data.first().map(|(d, _)| fmt_short(d)).unwrap_or_default();
    let last_label = data.last().map(|(d, _)| fmt_short(d)).unwrap_or_default();
    html! {
        section .card {
            header .card__head {
                div {
                    h3 .card__title { (title) }
                    p style="margin:0;font-size:var(--text-xs);color:var(--text-muted)" { (subtitle) }
                }
                a .btn .btn-ghost .btn-sm .card__actions href=(view_href) { "View" }
            }
            div .card__body {
                table .charts-css .column style=(format!("--chart-color: {color_var}")) {
                    tbody {
                        @for (day, val) in data {
                            tr data-tooltip=(format!("{day}: {val}")) {
                                td style=(format!("--size: {:.4}", *val as f64 / max as f64)) {
                                    (val)
                                }
                            }
                        }
                    }
                }
                div .charts-css__range {
                    span { (first_label) }
                    span { (last_label) }
                }
            }
        }
    }
}

/// Tiny inline-SVG trend line for a stat tile. Decorative: the tile already
/// states the value in text, so this is aria-hidden.
///
/// Uses a 100x24 viewBox with `preserveAspectRatio="none"` so it stretches to
/// whatever width the tile gives it without needing a layout measurement.
pub fn sparkline(series: &[i64], color_var: &str) -> Markup {
    if series.is_empty() {
        return html! {};
    }
    let max = series.iter().copied().max().unwrap_or(0);
    let min = series.iter().copied().min().unwrap_or(0);
    // A flat series has zero span; clamp the divisor so it renders as a
    // centred straight line instead of dividing by zero.
    let span = (max - min).max(1) as f64;
    let step = if series.len() > 1 { 100.0 / (series.len() - 1) as f64 } else { 0.0 };
    let points = series
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let x = i as f64 * step;
            let y = 24.0 - ((*v - min) as f64 / span) * 24.0;
            format!("{x:.2},{y:.2}")
        })
        .collect::<Vec<_>>()
        .join(" ");
    html! {
        svg .sparkline viewBox="0 0 100 24" preserveAspectRatio="none"
            aria-hidden="true" style=(format!("--chart-color: {color_var}")) {
            // See the note in `line_chart_card` on why SVG shape elements use `{}`
            // rather than maud's `;` void syntax — it matters once a shape has
            // siblings, but keeping it consistent here avoids relying on the
            // enclosing `</svg>` to implicitly close a dangling tag.
            polyline points=(points) fill="none" stroke="var(--chart-color)" stroke-width="1.5" {}
        }
    }
}

/// 30-day line + area chart with gridlines and y-axis ticks.
///
/// `bar_chart_card` renders the same data as columns; pick per series —
/// the dashboard uses bars for Requests and lines for New users / Errors.
pub fn line_chart_card(
    title: &str,
    subtitle: &str,
    data: &[(String, i64)],
    color_var: &str,
    view_href: &str,
) -> Markup {
    let max = data.iter().map(|(_, v)| *v).max().unwrap_or(0).max(1);
    // Three gridlines plus the baseline, matching the mockup's 0/1/2/3.
    let ticks: Vec<i64> = (0..=3).map(|i| max * i / 3).collect();
    let step = if data.len() > 1 { 100.0 / (data.len() - 1) as f64 } else { 0.0 };
    let pts = |f: &dyn Fn(usize, i64) -> String| {
        data.iter().enumerate().map(|(i, (_, v))| f(i, *v)).collect::<Vec<_>>().join(" ")
    };
    let line = pts(&|i, v| format!("{:.2},{:.2}", i as f64 * step, 60.0 - (v as f64 / max as f64) * 60.0));
    let area = format!("0,60 {line} 100,60");
    let fmt_short = |s: &str| -> String {
        chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
            .map(|d| d.format("%b %-d").to_string())
            .unwrap_or_else(|_| s.to_string())
    };
    html! {
        section .card {
            header .card__head {
                div {
                    h3 .card__title { (title) }
                    p .card__subtitle { (subtitle) }
                }
                a .btn .btn-ghost .btn-sm .card__actions href=(view_href) { "View" }
            }
            div .card__body {
                div .chart {
                    div .chart__yaxis {
                        @for t in ticks.iter().rev() { span .chart__ytick { (t) } }
                    }
                    svg .chart__plot viewBox="0 0 100 60" preserveAspectRatio="none"
                        role="img" aria-label=(format!("{title}, {subtitle}"))
                        style=(format!("--chart-color: {color_var}")) {
                        // maud's `;` void-element syntax emits `<tag attrs>` with no
                        // closing tag for *any* element name — it isn't restricted to
                        // real HTML5 void elements. Browsers require SVG shape elements
                        // (line/polygon/polyline/circle) to be explicitly closed; without
                        // that, each of these becomes a nested *child* of the previous
                        // one instead of a sibling, and browsers refuse to paint shape
                        // elements nested inside another shape element — only the first
                        // gridline would render. `{}` (an empty block body) generates a
                        // matched `<tag></tag>` pair, keeping them proper siblings.
                        @for i in 0..4 {
                            line .chart__gridline x1="0" x2="100"
                                y1=(i as f64 * 20.0) y2=(i as f64 * 20.0) {}
                        }
                        polygon .chart__area points=(area) {}
                        polyline .chart__line points=(line) fill="none" {}
                        @if let Some((_, last)) = data.last() {
                            circle .chart__dot cx="100"
                                cy=(format!("{:.2}", 60.0 - (*last as f64 / max as f64) * 60.0))
                                r="2" {}
                        }
                    }
                }
                div .charts-css__range {
                    span { (data.first().map(|(d, _)| fmt_short(d)).unwrap_or_default()) }
                    span { (data.last().map(|(d, _)| fmt_short(d)).unwrap_or_default()) }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn sparkline_emits_one_point_per_sample_and_is_decorative() {
        let m = super::sparkline(&[0, 5, 3], "var(--primary-color)").into_string();
        assert!(m.contains("<svg"));
        assert!(
            m.contains(r#"aria-hidden="true""#),
            "sparkline duplicates the value beside it"
        );
        let pts = m.split("points=\"").nth(1).unwrap().split('"').next().unwrap();
        assert_eq!(pts.split_whitespace().count(), 3, "one point per sample: {pts}");
    }

    #[test]
    fn sparkline_flat_series_does_not_divide_by_zero() {
        let m = super::sparkline(&[4, 4, 4], "var(--primary-color)").into_string();
        assert!(m.contains("points="), "flat series must still render");
        assert!(!m.contains("NaN"), "flat series produced NaN: {m}");
    }

    #[test]
    fn sparkline_empty_series_renders_nothing() {
        assert_eq!(super::sparkline(&[], "var(--primary-color)").into_string(), "");
    }

    #[test]
    fn line_chart_card_has_gridlines_and_axis_labels() {
        let data = vec![("2026-08-01".to_string(), 1), ("2026-08-02".to_string(), 3)];
        let m = super::line_chart_card(
            "New users",
            "Last 30 days",
            &data,
            "var(--primary-color)",
            "/b/admin/users",
        )
        .into_string();
        assert!(m.contains("chart__gridline"), "gridlines missing");
        assert!(m.contains("chart__ytick"), "y-axis ticks missing");
        assert!(m.contains("Aug 1") && m.contains("Aug 2"), "x range labels missing");
        assert!(m.contains("chart__dot"), "endpoint dot missing");
    }
}
