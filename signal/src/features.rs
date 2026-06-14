pub struct Bar {
    pub close: f64,
    pub volume: i64,
}

#[allow(dead_code)]
pub struct PreEventFeatures {
    pub car_20d: Option<f64>,
    pub car_10d: Option<f64>,
    pub car_5d: Option<f64>,
    pub mean_abvol_20d: Option<f64>,
    pub max_abvol_20d: Option<f64>,
    pub realized_vol_20d: Option<f64>,
    pub price_momentum_20d: Option<f64>,
    pub vol_trend_slope: Option<f64>,
    pub pre_event_bars: i32,
}

/// Mean of a non-empty slice. Returns 0.0 for empty input.
fn mean(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        return 0.0;
    }
    xs.iter().sum::<f64>() / xs.len() as f64
}

/// Population standard deviation. Returns 0.0 for fewer than 2 elements.
fn std_dev(xs: &[f64]) -> f64 {
    if xs.len() < 2 {
        return 0.0;
    }
    let m = mean(xs);
    let variance = xs.iter().map(|x| (x - m).powi(2)).sum::<f64>() / xs.len() as f64;
    variance.sqrt()
}

/// OLS slope of ys over integer index t = 0, 1, ..., n-1.
/// Returns 0.0 if denominator is zero (constant index, i.e. n <= 1).
fn ols_slope(ys: &[f64]) -> f64 {
    let n = ys.len() as f64;
    if n <= 1.0 {
        return 0.0;
    }
    // Closed-form sums: Σt = n(n-1)/2, Σt² = n(n-1)(2n-1)/6
    let sum_t = n * (n - 1.0) / 2.0;
    let sum_t2 = n * (n - 1.0) * (2.0 * n - 1.0) / 6.0;
    let sum_v: f64 = ys.iter().sum();
    let sum_tv: f64 = ys.iter().enumerate().map(|(t, &v)| t as f64 * v).sum();
    let denom = n * sum_t2 - sum_t * sum_t;
    if denom == 0.0 {
        0.0
    } else {
        (n * sum_tv - sum_t * sum_v) / denom
    }
}

/// Compute daily returns for a slice of bars.
/// Returns a Vec with len = bars.len() - 1.
/// Entries are None when close_{t-1} == 0.
fn daily_returns(bars: &[Bar]) -> Vec<Option<f64>> {
    bars.windows(2)
        .map(|w| {
            let prev = w[0].close;
            if prev == 0.0 {
                None
            } else {
                Some((w[1].close - prev) / prev)
            }
        })
        .collect()
}

/// Compute pre-event CAR and ABVOL features from a chronologically-sorted
/// slice of bars covering the [-80, -1] window before an event.
///
/// The last 20 bars form the feature window; any earlier bars are the baseline.
/// All features are `None` when fewer than 20 bars are present in the feature window.
pub fn compute_pre_event_features(bars: &[Bar]) -> PreEventFeatures {
    let n = bars.len();
    let feature_start = n.saturating_sub(20);
    let feature_bars = &bars[feature_start..];
    let baseline_bars = &bars[..feature_start];

    let pre_event_bars = feature_bars.len() as i32;

    if feature_bars.len() < 20 {
        return PreEventFeatures {
            car_20d: None,
            car_10d: None,
            car_5d: None,
            mean_abvol_20d: None,
            max_abvol_20d: None,
            realized_vol_20d: None,
            price_momentum_20d: None,
            vol_trend_slope: None,
            pre_event_bars,
        };
    }

    // ── Baseline statistics ────────────────────────────────────────────────
    let baseline_returns: Vec<f64> = daily_returns(baseline_bars).into_iter().flatten().collect();
    let expected_return = mean(&baseline_returns);

    let baseline_volumes: Vec<f64> = baseline_bars.iter().map(|b| b.volume as f64).collect();
    let mean_vol_baseline = mean(&baseline_volumes);
    let std_vol_baseline = std_dev(&baseline_volumes);

    // ── Feature-window returns ─────────────────────────────────────────────
    // For the return on feature_bars[0] (day -20) we need close_{-21}, which
    // is baseline_bars.last() if available.
    let extended: Vec<&Bar> = baseline_bars
        .last()
        .into_iter()
        .chain(feature_bars.iter())
        .collect();
    let feature_returns: Vec<f64> = daily_returns_ref(&extended).into_iter().flatten().collect();

    // ── ABVOL z-scores over the feature window ─────────────────────────────
    let abvol: Vec<f64> = feature_bars
        .iter()
        .map(|b| {
            if std_vol_baseline == 0.0 {
                0.0
            } else {
                (b.volume as f64 - mean_vol_baseline) / std_vol_baseline
            }
        })
        .collect();

    // ── Abnormal returns ───────────────────────────────────────────────────
    let abr: Vec<f64> = feature_returns
        .iter()
        .map(|&r| r - expected_return)
        .collect();

    // CAR windows (sum over last N abnormal returns)
    let car = |last_n: usize| -> f64 { abr.iter().rev().take(last_n).sum::<f64>() };
    let car_20d = car(20);
    let car_10d = car(10);
    let car_5d = car(5);

    // ── Remaining features ─────────────────────────────────────────────────
    let realized_vol_20d = std_dev(&feature_returns);

    let price_momentum_20d = {
        let first = feature_bars.first().unwrap().close;
        let last = feature_bars.last().unwrap().close;
        if first == 0.0 {
            0.0
        } else {
            (last - first) / first
        }
    };

    let vol_volumes: Vec<f64> = feature_bars.iter().map(|b| b.volume as f64).collect();
    let vol_trend_slope = ols_slope(&vol_volumes);

    PreEventFeatures {
        car_20d: Some(car_20d),
        car_10d: Some(car_10d),
        car_5d: Some(car_5d),
        mean_abvol_20d: Some(mean(&abvol)),
        max_abvol_20d: abvol.iter().copied().reduce(f64::max),
        realized_vol_20d: Some(realized_vol_20d),
        price_momentum_20d: Some(price_momentum_20d),
        vol_trend_slope: Some(vol_trend_slope),
        pre_event_bars,
    }
}

/// Like `daily_returns` but works on a slice of references.
fn daily_returns_ref(bars: &[&Bar]) -> Vec<Option<f64>> {
    bars.windows(2)
        .map(|w| {
            let prev = w[0].close;
            if prev == 0.0 {
                None
            } else {
                Some((w[1].close - prev) / prev)
            }
        })
        .collect()
}

#[allow(dead_code)]
pub struct PostEventAr {
    pub ar_1d: f64,
    pub ar_5d: f64,
}

/// Compute post-event abnormal returns from a bar slice that covers
/// [baseline_start, event_date + 6 calendar days].
///
/// `bars` are sorted ascending. The function splits at `event_date`:
/// bars before it form the baseline; bars after it are the post-event window.
/// Returns `None` when fewer than 3 post-event bars are available.
#[allow(dead_code)]
pub fn compute_post_event_ar(bars: &[Bar], event_idx: usize) -> Option<PostEventAr> {
    // bars[..event_idx] = pre-event (baseline), bars[event_idx..] = post-event
    let post_bars = &bars[event_idx..];
    if post_bars.len() < 3 {
        return None;
    }

    // Baseline: bars before event_idx (need at least a pair for returns)
    let baseline_bars = &bars[..event_idx];
    let baseline_returns: Vec<f64> = daily_returns(baseline_bars).into_iter().flatten().collect();
    let expected_return = mean(&baseline_returns);

    // Post-event returns: need the last pre-event bar as reference for day +1
    // Build extended = [last baseline bar] + post_bars
    let extended: Vec<&Bar> = baseline_bars
        .last()
        .into_iter()
        .chain(post_bars.iter())
        .collect();
    let post_returns: Vec<f64> = daily_returns_ref(&extended).into_iter().flatten().collect();

    // ar_1d = abnormal return on the first post-event bar
    let ar_1d = post_returns.first().map(|&r| r - expected_return)?;

    // ar_5d = cumulative abnormal return over up to 5 post-event bars
    let ar_5d: f64 = post_returns
        .iter()
        .take(5)
        .map(|&r| r - expected_return)
        .sum();

    Some(PostEventAr { ar_1d, ar_5d })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat_bars(n: usize, close: f64, volume: i64) -> Vec<Bar> {
        (0..n).map(|_| Bar { close, volume }).collect()
    }

    #[test]
    fn test_insufficient_bars_returns_none() {
        let bars = flat_bars(15, 100.0, 1_000_000);
        let f = compute_pre_event_features(&bars);
        assert_eq!(f.pre_event_bars, 15);
        assert!(f.car_20d.is_none());
        assert!(f.car_10d.is_none());
        assert!(f.car_5d.is_none());
        assert!(f.mean_abvol_20d.is_none());
        assert!(f.max_abvol_20d.is_none());
        assert!(f.realized_vol_20d.is_none());
        assert!(f.price_momentum_20d.is_none());
        assert!(f.vol_trend_slope.is_none());
    }

    #[test]
    fn test_flat_price_and_volume_car_near_zero() {
        let bars = flat_bars(80, 100.0, 1_000_000);
        let f = compute_pre_event_features(&bars);
        assert_eq!(f.pre_event_bars, 20);
        assert!(f.car_20d.unwrap().abs() < 1e-10);
        assert!(f.car_10d.unwrap().abs() < 1e-10);
        assert!(f.car_5d.unwrap().abs() < 1e-10);
        assert!(f.mean_abvol_20d.unwrap().abs() < 1e-10);
    }

    #[test]
    fn test_step_up_in_price_and_volume_car_positive() {
        // 60 baseline bars: price 100, volume oscillates 900K–1.1M so std > 0
        let mut bars: Vec<Bar> = (0..60)
            .map(|i| Bar {
                close: 100.0,
                volume: if i % 2 == 0 { 900_000 } else { 1_100_000 },
            })
            .collect();
        // 20 feature bars: price rises +1/day, volume 3M (well above baseline mean)
        for i in 0..20 {
            bars.push(Bar {
                close: 105.0 + i as f64,
                volume: 3_000_000,
            });
        }
        let f = compute_pre_event_features(&bars);
        assert_eq!(f.pre_event_bars, 20);
        assert!(f.car_20d.unwrap() > 0.0, "CAR_20d should be positive");
        assert!(f.car_5d.unwrap() > 0.0, "CAR_5d should be positive");
        assert!(
            f.mean_abvol_20d.unwrap() > 0.0,
            "mean_abvol should be positive"
        );
        assert!(
            f.max_abvol_20d.unwrap() > 0.0,
            "max_abvol should be positive"
        );
    }

    // ── post-event AR tests ───────────────────────────────────────────────

    #[test]
    fn test_post_event_insufficient_bars_returns_none() {
        // 60 baseline + 2 post-event bars — fewer than the required 3
        let mut bars = flat_bars(60, 100.0, 1_000_000);
        bars.push(Bar {
            close: 110.0,
            volume: 1_000_000,
        });
        bars.push(Bar {
            close: 111.0,
            volume: 1_000_000,
        });
        assert!(compute_post_event_ar(&bars, 60).is_none());
    }

    #[test]
    fn test_post_event_flat_ar_near_zero() {
        // 60 baseline bars at 100, 5 post-event bars also at 100 (no move)
        let mut bars = flat_bars(60, 100.0, 1_000_000);
        bars.extend(flat_bars(5, 100.0, 1_000_000));
        let ar = compute_post_event_ar(&bars, 60).unwrap();
        assert!(ar.ar_1d.abs() < 1e-10, "ar_1d should be ≈ 0");
        assert!(ar.ar_5d.abs() < 1e-10, "ar_5d should be ≈ 0");
    }

    #[test]
    fn test_post_event_large_move_ar_positive() {
        // 60 flat baseline bars at 100, then a +10% jump on day +1
        let mut bars = flat_bars(60, 100.0, 1_000_000);
        bars.push(Bar {
            close: 110.0,
            volume: 1_000_000,
        }); // day +1: +10%
        bars.push(Bar {
            close: 110.0,
            volume: 1_000_000,
        });
        bars.push(Bar {
            close: 110.0,
            volume: 1_000_000,
        });
        bars.push(Bar {
            close: 110.0,
            volume: 1_000_000,
        });
        bars.push(Bar {
            close: 110.0,
            volume: 1_000_000,
        }); // days +2..+5: flat
        let ar = compute_post_event_ar(&bars, 60).unwrap();
        assert!(
            ar.ar_1d > 0.05,
            "ar_1d should be significantly positive (≈ 0.10)"
        );
        assert!(ar.ar_5d > 0.05, "ar_5d should be significantly positive");
    }
}
