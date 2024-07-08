#[derive(Debug)]
pub struct KalmanEstimatesRow {
    pub obj_id: u32,
    pub frame: u32,
    pub timestamp: f32,
    pub x: f64,
    pub y: f64,
    pub z: f64,
    pub xvel: f64,
    pub yvel: f64,
    pub zvel: f64,
}