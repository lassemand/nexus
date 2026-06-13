pub struct Bar {
    pub close: f64,
    pub volume: i64,
}

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
}
