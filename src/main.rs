use braid_event_stream::{BraidEvent, BraidEventStream, KalmanEstimates};
use env_logger::Env;
use lens_driver::LensDriver;
use linregress::{FormulaRegressionBuilder, RegressionDataBuilder};
use log::{debug, error, info, warn};
use std::env;
use std::error::Error;
use std::fs::File;
use std::time::{Duration, Instant};
use tokio;
use url::Url;

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
struct TrackingZone {
    x_min: f64,
    x_max: f64,
    y_min: f64,
    y_max: f64,
    z_min: f64,
    z_max: f64,
}

struct LensController {
    braid_url: Url,
    lens_driver: LensDriver,
    tracking_zone: TrackingZone,
    model: LinearModel,
    currently_tracked_obj: Option<u32>,
    last_update_time: Instant,
    update_interval: Duration,
}

impl LensController {
    fn new(
        braid_url: &str,
        lens_driver_port: &str,
        tracking_zone: TrackingZone,
        update_interval_ms: u64,
    ) -> Result<Self, Box<dyn Error>> {
        let url = Url::parse(braid_url)?;

        // Setup lens driver
        let mut lens_driver = LensDriver::new(Some(lens_driver_port.to_string()));
        lens_driver.connect()?;
        lens_driver.mode(Some("focal"))?;

        // setup the linear model for the lens
        let file_path = "/home/buchsbaum/lens_calibration/liquid_lens_calibration.csv";
        let model = Self::create_linear_model_from_csv(file_path)?;

        Ok(Self {
            braid_url: url,
            lens_driver,
            tracking_zone,
            model,
            currently_tracked_obj: None,
            last_update_time: Instant::now(),
            update_interval: Duration::from_millis(update_interval_ms),
        })
    }

    fn create_linear_model_from_csv(file_path: &str) -> Result<LinearModel, Box<dyn Error>> {
        let file = File::open(file_path).expect("csv file not found");
        let mut rdr = csv::Reader::from_reader(file);

        let mut xs: Vec<f64> = Vec::new();
        let mut ys: Vec<f64> = Vec::new();
        for result in rdr.records() {
            let record = result?;
            let distance: f64 = record.get(1).ok_or("Missing distance")?.parse()?;
            let dpt: f64 = record.get(0).ok_or("Missing dpt")?.parse()?;
            xs.push(distance);
            ys.push(dpt);
        }

        let data = vec![("Y", ys), ("X1", xs)];
        let regression_data = RegressionDataBuilder::new().build_from(data)?;
        let model = FormulaRegressionBuilder::new()
            .data(&regression_data)
            .formula("Y ~ X1")
            .fit()?;

        let slope = model.parameters()[1];
        let intercept = model.parameters()[0];
        info!("Linear model: y = {}x + {}", slope, intercept);

        Ok(LinearModel { slope, intercept })
    }

    fn is_in_zone(&self, estimates: &KalmanEstimates) -> bool {
        estimates.x >= self.tracking_zone.x_min
            && estimates.x <= self.tracking_zone.x_max
            && estimates.y >= self.tracking_zone.y_min
            && estimates.y <= self.tracking_zone.y_max
            && estimates.z >= self.tracking_zone.z_min
            && estimates.z <= self.tracking_zone.z_max
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

            info!(
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
        let braid_stream = BraidEventStream::new(events_url.to_string());
        let mut event_receiver = braid_stream.stream_events().await?;

        while let Some(event_result) = event_receiver.recv().await {
            match event_result {
                Ok(braid_event) => {
                    let received_time = Instant::now();
                    match braid_event {
                        BraidEvent::Birth(estimates) | BraidEvent::Update(estimates) => {
                            if self.is_in_zone(&estimates) {
                                if self.currently_tracked_obj.is_none()
                                    || self.currently_tracked_obj == Some(estimates.obj_id)
                                {
                                    self.currently_tracked_obj = Some(estimates.obj_id);
                                    self.update_lens_position(estimates.z, received_time);
                                    info!(
                                        "Tracking object {}: z = {}",
                                        estimates.obj_id, estimates.z
                                    );
                                }
                            } else if self.currently_tracked_obj == Some(estimates.obj_id) {
                                info!("Object {} left the tracking zone", estimates.obj_id);
                                self.currently_tracked_obj = None;
                            }
                        }
                        BraidEvent::Death { obj_id } => {
                            if self.currently_tracked_obj == Some(obj_id) {
                                info!("Tracked object {} died", obj_id);
                                self.currently_tracked_obj = None;
                            }
                        }
                    }
                }
                Err(e) => {
                    error!("Error processing Braid event: {:?}", e);
                }
            }
        }

        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    env_logger::Builder::from_env(Env::default().default_filter_or("debug")).init();

    let args: Vec<String> = env::args().collect();
    let braid_url = args
        .get(1)
        .map(String::as_str)
        .unwrap_or("http://10.40.80.6:8397/");
    let lens_driver_port = args
        .get(2)
        .map(String::as_str)
        .unwrap_or("/dev/optotune_ld");
    let update_interval_ms = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(20); // Default to 20ms if not provided

    let tracking_zone = TrackingZone {
        x_min: -0.05,
        x_max: 0.05,
        y_min: -0.05,
        y_max: 0.05,
        z_min: 0.1,
        z_max: 0.25,
    };

    info!(
        "Initializing LensController with URL: {}, port: {}, update interval: {}ms",
        braid_url, lens_driver_port, update_interval_ms
    );
    let mut lens_controller = LensController::new(
        braid_url,
        lens_driver_port,
        tracking_zone,
        update_interval_ms,
    )?;

    info!("Starting LensController");
    if let Err(e) = lens_controller.run().await {
        error!("LensController encountered an error: {}", e);
    }

    Ok(())
}
