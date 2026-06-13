use ndarray::Array2;
use ort::{session::Session, value::TensorRef};
use tracing::{info, warn};

/// Feature order — must match `ml/train_insider_score.py` FEATURE_NAMES.
pub const FEATURE_NAMES: &[&str] = &[
    "car_20d",
    "car_10d",
    "car_5d",
    "mean_abvol_20d",
    "max_abvol_20d",
    "realized_vol_20d",
    "price_momentum_20d",
    "vol_trend_slope",
];

pub struct Scorer {
    session: Session,
}

impl Scorer {
    /// Load the ONNX model from `path`.
    /// Returns `None` if path is `None` or the file does not exist —
    /// signal continues without scoring until the model is trained.
    pub fn load(path: Option<&str>) -> Option<Self> {
        let path = match path {
            Some(p) => p,
            None => {
                info!("--insider-model-path not set — running without insider scoring");
                return None;
            }
        };

        if !std::path::Path::new(path).exists() {
            warn!(path = %path, "model file does not exist — running without insider scoring");
            return None;
        }

        match Session::builder().and_then(|mut b| b.commit_from_file(path)) {
            Ok(session) => {
                info!(path = %path, "insider score model loaded");
                Some(Self { session })
            }
            Err(e) => {
                warn!(path = %path, error = %e, "failed to load ONNX model — running without insider scoring");
                None
            }
        }
    }

    /// Run inference on a feature vector.
    ///
    /// `values` must have exactly `FEATURE_NAMES.len()` elements in the same
    /// order as `FEATURE_NAMES`. Returns the positive-class probability, or
    /// `None` on inference failure.
    pub fn score(&mut self, values: &[f32]) -> Option<f32> {
        let input = Array2::from_shape_vec((1, FEATURE_NAMES.len()), values.to_vec()).ok()?;
        let tensor = TensorRef::from_array_view(input.view()).ok()?;
        let outputs = self.session.run(ort::inputs![tensor]).ok()?;

        // XGBoost ONNX outputs: index 1 is the probability tensor, shape [1, 2].
        // Offset 1 = positive-class probability.
        let (_shape, data) = outputs[1].try_extract_tensor::<f32>().ok()?;
        data.get(1).copied()
    }
}
