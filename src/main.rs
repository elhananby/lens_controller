use std::error::Error;
use std::time::{Duration, Instant};

use anyhow::Result;
use env_logger::Env;
use eventsource_stream::{Event, EventStream};
use futures_util::StreamExt;
use lens_driver::LensDriver;
use linregress::{FormulaRegressionBuilder, RegressionDataBuilder};
use log::{debug, error, info, warn};
use reqwest::Client;
use serde_json::Value;
use std::fs::File;

use url::Url;

use clap::Parser;

#[derive(Parser, Debug)]
#[clap(name = "LensController")]
#[clap(about = "Controls the liquid lens system", long_about = None)]

struct Args {
    #[clap(long, default_value = "http://127.0.0.1:8397/")]
    braid_url: String,

    #[clap(long, default_value = "/dev/optotune_ld")]
    lens_driver_port: String,

    #[clap(long, default_value = "test")]
    save_folder: String,

    #[clap(long, default_value = "0.5")]
    smoothing_factor: f64,

    #[clap(long, default_value = "100")]
    prediction_time_delta_ms: u64,

    #[clap(long, default_value = "10")]
    min_update_interval_ms: u64,

    #[clap(long, default_value = "100")]
    max_update_interval_ms: u64,

    #[clap(long, default_value = "0.5")]
    velocity_scaling_factor: f64,
}

struct LinearModel {
    slope: f64,
    intercept: f64,
}

impl LinearModel {
    fn predict(&self, distance: f64) -> f64 {
        self.slope * distance + self.intercept
    }
}

#[derive(Debug, Clone)]
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

#[derive(Debug)]
enum BraidEvent {
    Birth(KalmanEstimatesRow),
    Update(KalmanEstimatesRow),
    Death { obj_id: u32 },
}

struct LensController {
    braid_url: Url,
    client: Client,
    lens_driver: LensDriver,
    save_folder: String,
    tracking_zone: TrackingZone,
    model: LinearModel,
    currently_tracked_obj: Option<u32>,
    last_update_time: Instant,
    object_birth_time: Instant,
    current_focal_power: f64,
    smoothing_factor: f64,
    prediction_time_delta: Duration,
    min_update_interval: Duration,
    max_update_interval: Duration,
    velocity_scaling_factor: f64,
}

#[derive(Debug, Clone)]
struct TrackingZone {
    x_min: f64,
    x_max: f64,
    y_min: f64,
    y_max: f64,
    z_min: f64,
    z_max: f64,
}

impl LensController {
    async fn new(
        braid_url: &str,
        lens_driver_port: &str,
        tracking_zone: TrackingZone,
        save_folder: &str,
        smoothing_factor: f64,
        prediction_time_delta: Duration,
        min_update_interval_ms: u64,
        max_update_interval_ms: u64,
        velocity_scaling_factor: f64,
    ) -> Result<Self, Box<dyn Error>> {
        let url = Url::parse(braid_url)?;
        let client = Client::new();

        // Setup lens driver
        log::debug!("Connecting to lens driver on port {}", lens_driver_port);
        let mut lens_driver = LensDriver::new(Some(lens_driver_port.to_string()));
        lens_driver.connect()?;
        lens_driver.mode(Some("focal"))?;

        // Verify connection
        log::debug!("Verifying connection...");
        client.get(url.clone()).send().await?;

        // setup the linear model for the lens
        log::debug!("Setting up linear model...");
        let file_path = "/home/buchsbaum/liquid_lens_calibration_20240930.csv";
        let model = Self::create_linear_model_from_csv(file_path)?;

        log::debug!("Initializing LensController...");
        // Return the initialized LensController
        Ok(Self {
            braid_url: url,
            client,
            lens_driver,
            save_folder: save_folder.to_string(),
            tracking_zone,
            model,
            currently_tracked_obj: None,
            last_update_time: Instant::now(),
            object_birth_time: Instant::now(),
            current_focal_power: 0.0,
            smoothing_factor,
            prediction_time_delta,
            min_update_interval: Duration::from_millis(min_update_interval_ms),
            max_update_interval: Duration::from_millis(max_update_interval_ms),
            velocity_scaling_factor,
        })
    }

    fn create_linear_model_from_csv(file_path: &str) -> Result<LinearModel, Box<dyn Error>> {
        // Open the CSV file
        let file = File::open(file_path).expect("csv file not found");
        let mut rdr = csv::Reader::from_reader(file);

        // Prepare data for regression
        let mut xs: Vec<f64> = Vec::new();
        let mut ys: Vec<f64> = Vec::new();
        // Read and add data points
        for result in rdr.records() {
            let record = result?;
            let distance: f64 = record.get(0).ok_or("Missing distance")?.parse()?;
            let dpt: f64 = record.get(1).ok_or("Missing dpt")?.parse()?;
            xs.push(distance);
            ys.push(dpt);
        }

        // Check if we have enough observations for regression
        if xs.len() >= 3 {
            // Build the regression model
            let data = vec![("Y", ys.clone()), ("X1", xs.clone())];
            let regression_data = RegressionDataBuilder::new().build_from(data)?;
            let model = FormulaRegressionBuilder::new()
                .data(&regression_data)
                .formula("Y ~ X1")
                .fit();

            match model {
                Ok(fitted_model) => {
                    let slope = fitted_model.parameters()[1];
                    let intercept = fitted_model.parameters()[0];
                    debug!("Linear model (regression): y = {}x + {}", slope, intercept);
                    Ok(LinearModel { slope, intercept })
                }
                Err(_) => Self::fallback_linear_model(&xs, &ys),
            }
        } else {
            Self::fallback_linear_model(&xs, &ys)
        }
    }

    fn fallback_linear_model(xs: &[f64], ys: &[f64]) -> Result<LinearModel, Box<dyn Error>> {
        if xs.len() != ys.len() || xs.is_empty() {
            return Err("Invalid data for fallback method".into());
        }

        if xs.len() == 1 {
            // If we have only one point, assume a horizontal line
            warn!("Only one data point available. Assuming horizontal line.");
            Ok(LinearModel {
                slope: 0.0,
                intercept: ys[0],
            })
        } else {
            // Use the first and last points to calculate slope and intercept
            let x1 = xs[0];
            let y1 = ys[0];
            let x2 = xs[xs.len() - 1];
            let y2 = ys[ys.len() - 1];

            let slope = (y2 - y1) / (x2 - x1);
            let intercept = y1 - slope * x1;

            warn!("Using fallback method with {} data points.", xs.len());
            debug!("Linear model (fallback): y = {}x + {}", slope, intercept);
            Ok(LinearModel { slope, intercept })
        }
    }

    fn parse_kalman_estimates(data: &Value) -> Option<KalmanEstimatesRow> {
        Some(KalmanEstimatesRow {
            obj_id: data.get("obj_id")?.as_u64()? as u32,
            frame: data.get("frame")?.as_u64()? as u32,
            timestamp: data
                .get("timestamp")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0) as f32,
            x: data.get("x")?.as_f64()?,
            y: data.get("y")?.as_f64()?,
            z: data.get("z")?.as_f64()?,
            xvel: data.get("xvel")?.as_f64()?,
            yvel: data.get("yvel")?.as_f64()?,
            zvel: data.get("zvel")?.as_f64()?,
        })
    }

    fn is_in_zone(&self, row: &KalmanEstimatesRow) -> bool {
        row.x >= self.tracking_zone.x_min
            && row.x <= self.tracking_zone.x_max
            && row.y >= self.tracking_zone.y_min
            && row.y <= self.tracking_zone.y_max
            && row.z >= self.tracking_zone.z_min
            && row.z <= self.tracking_zone.z_max
    }

    fn predict_future_position(&self, row: &KalmanEstimatesRow) -> (f64, f64, f64) {
        let time_delta = self.prediction_time_delta.as_secs_f64();
        (
            row.x + row.xvel * time_delta,
            row.y + row.yvel * time_delta,
            row.z + row.zvel * time_delta,
        )
    }

    fn smooth_lens_adjustment(&mut self, target_dpt: f64) -> Result<f64, Box<dyn Error>> {
        self.current_focal_power = self.current_focal_power * (1.0 - self.smoothing_factor)
            + target_dpt * self.smoothing_factor;
        self.lens_driver.focalpower(Some(self.current_focal_power))
    }

    fn calculate_dynamic_update_interval(&self, row: &KalmanEstimatesRow) -> Duration {
        let velocity = (row.xvel.powi(2) + row.yvel.powi(2) + row.zvel.powi(2)).sqrt();
        let interval = self.max_update_interval.as_secs_f64()
            / (1.0 + self.velocity_scaling_factor * velocity);
        let interval = interval.max(self.min_update_interval.as_secs_f64());
        Duration::from_secs_f64(interval)
    }

    fn handle_event(&mut self, event: BraidEvent, received_time: Instant) {
        match event {
            BraidEvent::Birth(row) | BraidEvent::Update(row) => {
                if self.is_in_zone(&row) {
                    if self.currently_tracked_obj != Some(row.obj_id) {
                        debug!("Object {} entered the tracking zone", row.obj_id);
                        self.object_birth_time = received_time;
                        self.currently_tracked_obj = Some(row.obj_id);
                    } else {
                        debug!("Tracking object {}: z = {}", row.obj_id, row.z);
                    }
                    self.update_lens_position(&row, received_time);
                } else if self.currently_tracked_obj == Some(row.obj_id) {
                    let tracking_duration = received_time.duration_since(self.object_birth_time);
                    debug!(
                        "Object {} left the tracking zone after {:.2} seconds",
                        row.obj_id,
                        tracking_duration.as_secs_f64()
                    );
                    self.currently_tracked_obj = None;
                }
            }
            BraidEvent::Death { obj_id } => {
                if self.currently_tracked_obj == Some(obj_id) {
                    let tracking_duration = received_time.duration_since(self.object_birth_time);
                    debug!(
                        "Tracked object {} died after {:.2} seconds",
                        obj_id,
                        tracking_duration.as_secs_f64()
                    );
                    self.currently_tracked_obj = None;
                }
            }
        }
    }

    fn update_lens_position(&mut self, row: &KalmanEstimatesRow, received_time: Instant) {
        let now = Instant::now();
        let dynamic_update_interval = self.calculate_dynamic_update_interval(row);
        if now.duration_since(self.last_update_time) >= dynamic_update_interval {
            let processing_time = now.duration_since(received_time);

            // Predict future position
            let (_, _, predicted_z) = self.predict_future_position(row);
            debug!("Updating lens position for predicted z = {}", predicted_z);

            // Calculate target diopters using the predicted position
            let target_dpt = self.model.predict(predicted_z);

            // Apply smooth lens adjustment
            if let Err(e) = self.smooth_lens_adjustment(target_dpt) {
                error!("Error adjusting lens: {:?}", e);
            }

            let update_time = Instant::now().duration_since(now);
            self.last_update_time = now;

            debug!(
                "Performance: Processing time: {:?}, Update time: {:?}, Total time: {:?}, Dynamic interval: {:?}",
                processing_time,
                update_time,
                processing_time + update_time,
                dynamic_update_interval
            );
        } else {
            debug!("Skipping lens update due to time constraint");
        }
    }

    async fn run(&mut self) -> Result<(), Box<dyn Error>> {
        let events_url = self.braid_url.join("events")?;
        let response = self
            .client
            .get(events_url)
            .header("Accept", "text/event-stream")
            .send()
            .await?;

        let stream = EventStream::new(response.bytes_stream());
        tokio::pin!(stream);

        while let Some(event_result) = stream.next().await {
            match event_result {
                Ok(event) => self.handle_event_stream(event).await,
                Err(e) => error!("Error in event stream: {:?}", e),
            }
        }

        Ok(())
    }

    async fn handle_event_stream(&mut self, event: Event) {
        if event.event != "braid" {
            return;
        }

        let received_time = Instant::now();
        match self.parse_braid_event(&event.data) {
            Ok(Some(braid_event)) => self.handle_event(braid_event, received_time),
            Ok(None) => {} // Unknown event type, already logged in parse_braid_event
            Err(e) => error!("Error processing event: {:?}", e),
        }
    }

    fn parse_braid_event(&self, data: &str) -> Result<Option<BraidEvent>, Box<dyn Error>> {
        let json_data: Value = serde_json::from_str(data)?;

        let msg = json_data["msg"]
            .as_object()
            .ok_or("Missing 'msg' field in data")?;

        let (event_type, event_data) = msg.iter().next().ok_or("Received empty message object")?;

        Ok(match event_type.as_str() {
            "Birth" => Self::parse_kalman_estimates(event_data).map(BraidEvent::Birth),
            "Update" => Self::parse_kalman_estimates(event_data).map(BraidEvent::Update),
            "Death" => Some(BraidEvent::Death {
                obj_id: event_data.as_u64().ok_or("Invalid obj_id")? as u32,
            }),
            _ => {
                warn!("Received unknown event type: {}", event_type);
                None
            }
        })
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    env_logger::Builder::from_env(Env::default().default_filter_or("debug")).init();

    let args = Args::parse();
    let tracking_zone = TrackingZone {
        x_min: -0.1,
        x_max: 0.1,
        y_min: -0.1,
        y_max: 0.1,
        z_min: 0.10,
        z_max: 0.30,
    };

    let braid_url = &args.braid_url;
    let lens_driver_port = &args.lens_driver_port;
    let save_folder = &args.save_folder;

    info!(
        "Initializing LensController with URL: {}, port: {}, update interval: {}ms-{}ms, save_folder: {}, smoothing_factor: {}, prediction_time_delta: {}ms, velocity_scaling_factor: {}",
        braid_url, lens_driver_port, args.min_update_interval_ms, args.max_update_interval_ms, save_folder, args.smoothing_factor, args.prediction_time_delta_ms, args.velocity_scaling_factor
    );
    let mut lens_controller = LensController::new(
        braid_url,
        lens_driver_port,
        tracking_zone,
        save_folder,
        args.smoothing_factor,
        Duration::from_millis(args.prediction_time_delta_ms),
        args.min_update_interval_ms,
        args.max_update_interval_ms,
        args.velocity_scaling_factor,
    )
    .await?;

    info!("Starting LensController");
    if let Err(e) = lens_controller.run().await {
        error!("LensController encountered an error: {}", e);
    }

    Ok(())
}
