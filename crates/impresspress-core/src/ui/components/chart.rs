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
                    h2 .card__title { (title) }
                    p .card__subtitle { (subtitle) }
                }
                a .btn .btn--ghost .btn--sm .card__actions href=(view_href) { "View" }
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
    // A flat series has zero span. Rendering it along the bottom edge would
    // read as "zero" / "lowest", which is wrong when the value is nonzero —
    // a flat line means "no change", so it belongs at the vertical centre
    // of the 24-tall viewBox (y = 12.0), not at y = 24.0.
    let flat = max == min;
    let step = if series.len() > 1 {
        100.0 / (series.len() - 1) as f64
    } else {
        0.0
    };
    let points = series
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let x = i as f64 * step;
            let y = if flat {
                12.0
            } else {
                24.0 - ((*v - min) as f64 / (max - min) as f64) * 24.0
            };
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
            //
            // `vector-effect="non-scaling-stroke"` matters here too: `.stat-spark`
            // is 4rem x 1.5rem, a different aspect ratio than the 100x24 viewBox,
            // so preserveAspectRatio="none" scales x and y unevenly. Without this,
            // the stroke renders visibly thicker in one axis than the other.
            polyline points=(points) fill="none" stroke="var(--chart-color)"
                stroke-width="1.5" vector-effect="non-scaling-stroke" {}
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
    // Four gridlines (top/max down to the baseline/0), matching the mockup's
    // 0/1/2/3. Integer division repeats values for a small `max` — e.g.
    // max=1 gives max*i/3 for i=3..=0 as [1, 0, 0, 0] — which would render
    // the same number three times in a row and read as a rendering bug, not
    // "count is small". `tick_labels` keeps all four gridlines (geometry
    // `line_chart_card`'s tests and the CSS depend on is unchanged) but
    // blanks a label that repeats the value immediately above it, so the
    // axis never shows a duplicate.
    let ticks: Vec<i64> = (0..=3).rev().map(|i| max * i / 3).collect();
    let mut prev_tick: Option<i64> = None;
    let tick_labels: Vec<Option<i64>> = ticks
        .iter()
        .map(|&t| {
            if prev_tick == Some(t) {
                None
            } else {
                prev_tick = Some(t);
                Some(t)
            }
        })
        .collect();
    let step = if data.len() > 1 {
        100.0 / (data.len() - 1) as f64
    } else {
        0.0
    };
    let pts = |f: &dyn Fn(usize, i64) -> String| {
        data.iter()
            .enumerate()
            .map(|(i, (_, v))| f(i, *v))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let line = pts(&|i, v| {
        format!(
            "{:.2},{:.2}",
            i as f64 * step,
            60.0 - (v as f64 / max as f64) * 60.0
        )
    });
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
                    h2 .card__title { (title) }
                    p .card__subtitle { (subtitle) }
                }
                a .btn .btn--ghost .btn--sm .card__actions href=(view_href) { "View" }
            }
            div .card__body {
                div .chart {
                    div .chart__yaxis {
                        @for label in &tick_labels {
                            span .chart__ytick { @if let Some(v) = label { (v) } }
                        }
                    }
                    // `--chart-color` is declared on this wrapper (not the <svg>
                    // itself) so it's visible to both the plot and the `.chart__dot`
                    // div below — a CSS custom property only inherits to descendants
                    // of the element it's set on, and the dot is now a sibling of the
                    // svg, not nested inside it.
                    div .chart__plot-wrap style=(format!("--chart-color: {color_var}")) {
                        svg .chart__plot viewBox="0 0 100 60" preserveAspectRatio="none"
                            role="img" aria-label=(format!("{title}, {subtitle}")) {
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
                        }
                        // The endpoint dot is an HTML div positioned with CSS, not an
                        // SVG <circle>. `viewBox="0 0 100 60"` with
                        // preserveAspectRatio="none" scales x and y by different
                        // factors depending on the rendered box size, which distorts a
                        // circle's *fill geometry* into an ellipse — vector-effect only
                        // preserves stroke width, it does not help here. A circular div
                        // positioned by percentage over the plot stays circular at any
                        // width; `--dot-y` is the one legitimate use of an inline style,
                        // a dynamic runtime value passed as a custom property.
                        @if let Some((_, last)) = data.last() {
                            div .chart__dot
                                style=(format!("--dot-y: {:.2}%", (1.0 - *last as f64 / max as f64) * 100.0)) {}
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
        let pts = m
            .split("points=\"")
            .nth(1)
            .unwrap()
            .split('"')
            .next()
            .unwrap();
        assert_eq!(
            pts.split_whitespace().count(),
            3,
            "one point per sample: {pts}"
        );
    }

    #[test]
    fn sparkline_flat_series_does_not_divide_by_zero() {
        let m = super::sparkline(&[4, 4, 4], "var(--primary-color)").into_string();
        assert!(m.contains("points="), "flat series must still render");
        assert!(!m.contains("NaN"), "flat series produced NaN: {m}");
    }

    #[test]
    fn sparkline_flat_series_renders_at_vertical_centre() {
        // A flat line means "no change"; drawing it along the bottom edge
        // (y = 24.0) would misleadingly read as "zero" / "lowest" instead.
        // It must sit at the viewBox's vertical midpoint, y = 12.0.
        let m = super::sparkline(&[4, 4, 4], "var(--primary-color)").into_string();
        let pts = m
            .split("points=\"")
            .nth(1)
            .unwrap()
            .split('"')
            .next()
            .unwrap();
        for point in pts.split_whitespace() {
            let y = point.split(',').nth(1).unwrap();
            assert_eq!(
                y, "12.00",
                "flat series must sit at the centre, not an edge: {pts}"
            );
        }
    }

    #[test]
    fn sparkline_empty_series_renders_nothing() {
        assert_eq!(
            super::sparkline(&[], "var(--primary-color)").into_string(),
            ""
        );
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
        assert!(
            m.contains("Aug 1") && m.contains("Aug 2"),
            "x range labels missing"
        );
        assert!(m.contains("chart__dot"), "endpoint dot missing");
    }

    #[test]
    fn line_chart_card_small_max_series_has_no_duplicate_y_tick_labels() {
        // A max of 1 (e.g. a fresh install with a single user) previously
        // computed integer-division ticks of [1, 0, 0, 0] — three identical
        // "0" labels in a row, which reads as a rendering bug rather than
        // "the count is small". This is the common case for a fresh
        // install, not an edge case.
        let data = vec![("2026-08-01".to_string(), 0), ("2026-08-02".to_string(), 1)];
        let m = super::line_chart_card(
            "New users",
            "Last 30 days",
            &data,
            "var(--primary-color)",
            "/b/admin/users",
        )
        .into_string();

        // Isolate the y-axis tick spans from the rest of the markup (the
        // plot itself also contains numbers, e.g. viewBox coordinates).
        let yaxis_start = m.find("chart__yaxis").expect("yaxis missing");
        let yaxis_end = m.find("chart__plot-wrap").expect("plot-wrap missing");
        let yaxis_html = &m[yaxis_start..yaxis_end];
        let labels: Vec<&str> = yaxis_html
            .split("chart__ytick\">")
            .skip(1)
            .map(|s| s.split("</span>").next().unwrap())
            .collect();

        assert_eq!(
            labels.len(),
            4,
            "gridline geometry (4 ticks) must be unchanged: {labels:?}"
        );
        let shown: Vec<&&str> = labels.iter().filter(|l| !l.is_empty()).collect();
        for pair in shown.windows(2) {
            assert_ne!(
                pair[0], pair[1],
                "adjacent shown y-axis labels must not repeat: {labels:?}"
            );
        }
        assert!(
            labels.contains(&"1"),
            "the max value must still be labeled: {labels:?}"
        );
    }
}
