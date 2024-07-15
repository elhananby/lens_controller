use std::error::Error;
use std::time::{Duration, Instant};

use anyhow::Result;
use env_logger::Env;
use eventsource_stream::EventStream;
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
    #[clap(long, default_value = "http://10.40.80.6:8397/")]
    braid_url: String,

    #[clap(long, default_value = "/dev/optotune_ld")]
    lens_driver_port: String,

    #[clap(long, default_value_t = 20)]
    update_interval_ms: u64,

    #[clap(long, default_value = "test")]
    save_folder: String,
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
    update_interval: Duration,
    object_birth_time: Instant,
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
        update_interval_ms: u64,
        save_folder: &str,
    ) -> Result<Self, Box<dyn Error>> {
        let url = Url::parse(braid_url)?;
        let client = Client::new();

        // Setup lens driver
        let mut lens_driver = LensDriver::new(Some(lens_driver_port.to_string()));
        lens_driver.connect()?;
        lens_driver.mode(Some("focal"))?;

        // Verify connection
        client.get(url.clone()).send().await?;

        // setup the linear model for the lens
        let file_path = "/home/buchsbaum/lens_calibration/liquid_lens_calibration.csv";
        let model = Self::create_linear_model_from_csv(file_path)?;

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
            update_interval: Duration::from_millis(update_interval_ms),
            object_birth_time: Instant::now(),
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
            let distance: f64 = record.get(1).ok_or("Missing distance")?.parse()?;
            let dpt: f64 = record.get(0).ok_or("Missing dpt")?.parse()?;
            xs.push(distance);
            ys.push(dpt);
        }

        // Build the regression model
        let data = vec![("Y", ys), ("X1", xs)];
        let regression_data = RegressionDataBuilder::new().build_from(data)?;
        let model = FormulaRegressionBuilder::new()
            .data(&regression_data)
            .formula("Y ~ X1")
            .fit()?;

        // Extract slope and intercept
        let slope = model.parameters()[1]; // The slope is the second parameter
        let intercept = model.parameters()[0]; // The intercept is the first parameter
        info!("Linear model: y = {}x + {}", slope, intercept);

        Ok(LinearModel { slope, intercept })
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

    fn handle_event(&mut self, event: BraidEvent, received_time: Instant) {
        match event {
            BraidEvent::Birth(row) | BraidEvent::Update(row) => {
                // Check if object is in the tracking zone
                if self.is_in_zone(&row) {
                    // Check if it's a new object
                    if self.currently_tracked_obj.is_none() {
                        // new + in zone
                        info!("Object {} entered the tracking zone", row.obj_id);
                        self.object_birth_time = Instant::now();
                        self.currently_tracked_obj = Some(row.obj_id);
                    } else if self.currently_tracked_obj == Some(row.obj_id) {
                        // existing + in zone
                        debug!("Tracking object {}: z = {}", row.obj_id, row.z);
                    }

                    // update lens either way
                    self.update_lens_position(row.z, received_time);
                } else if self.currently_tracked_obj == Some(row.obj_id) {
                    info!(
                        "Object {} left the tracking zone after {} seconds",
                        row.obj_id,
                        self.object_birth_time.elapsed().as_secs()
                    );
                    self.currently_tracked_obj = None;
                }
            }
            BraidEvent::Death { obj_id } => {
                if self.currently_tracked_obj == Some(obj_id) {
                    info!(
                        "Tracked object {} died after {} seconds",
                        obj_id,
                        self.object_birth_time.elapsed().as_secs()
                    );
                    self.currently_tracked_obj = None;
                }
            }
        }
    }

    fn update_lens_position(&mut self, z: f64, received_time: Instant) {
        let now = Instant::now();
        if now.duration_since(self.last_update_time) >= self.update_interval {
            let processing_time = now.duration_since(received_time);
            debug!("Updating lens position for z = {}", z);
            let dpt = self.model.predict(z);
            let _ = self.lens_driver.focalpower(Some(dpt));
            let update_time = Instant::now().duration_since(now);
            self.last_update_time = now;

            debug!(
                "Performance: Processing time: {:?}, Update time: {:?}, Total time: {:?}",
                processing_time,
                update_time,
                processing_time + update_time
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
                Ok(event) => {
                    if event.event == "braid" {
                        let received_time = Instant::now();
                        let data = &event.data;
                        match serde_json::from_str::<Value>(data) {
                            Ok(json_data) => {
                                if let Some(msg) = json_data["msg"].as_object() {
                                    if let Some((event_type, event_data)) = msg.iter().next() {
                                        let braid_event_result = match event_type.as_str() {
                                            "Birth" => Self::parse_kalman_estimates(event_data)
                                                .map(BraidEvent::Birth),
                                            "Update" => Self::parse_kalman_estimates(event_data)
                                                .map(BraidEvent::Update),
                                            "Death" => Some(BraidEvent::Death {
                                                obj_id: event_data.as_u64().unwrap() as u32,
                                            }),
                                            _ => None,
                                        };

                                        if let Some(braid_event) = braid_event_result {
                                            self.handle_event(braid_event, received_time);
                                        } else {
                                            warn!("Received unknown event type: {}", event_type);
                                        }
                                    } else {
                                        warn!("Received empty message object");
                                    }
                                } else {
                                    warn!("Missing 'msg' field in data: {:?}", json_data);
                                }
                            }
                            Err(e) => {
                                error!("Error parsing JSON: {:?}", e);
                                error!("Raw data: {}", data);
                            }
                        }
                    }
                }
                Err(e) => {
                    error!("Error in event stream: {:?}", e);
                    // Optionally, implement retry logic here
                }
            }
        }

        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    env_logger::Builder::from_env(Env::default().default_filter_or("info")).init();

    let args = Args::parse();
    
    let tracking_zone = TrackingZone {
        x_min: -0.075,
        x_max: 0.075,
        y_min: -0.1,
        y_max: 0.10,
        z_min: 0.1,
        z_max: 0.3,
    };

    let braid_url = &args.braid_url;
    let lens_driver_port = &args.lens_driver_port;
    let update_interval_ms = args.update_interval_ms;
    let save_folder = &args.save_folder;

    info!(
        "Initializing LensController with URL: {}, port: {}, update interval: {}ms, save_folder: {}",
        braid_url, lens_driver_port, update_interval_ms, save_folder
    );
    let mut lens_controller = LensController::new(
        braid_url,
        lens_driver_port,
        tracking_zone,
        update_interval_ms,
        save_folder,
    )
    .await?;

    info!("Starting LensController");
    if let Err(e) = lens_controller.run().await {
        error!("LensController encountered an error: {}", e);
    }

    Ok(())
}
